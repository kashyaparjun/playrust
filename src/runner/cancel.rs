use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxPropertyName, AxValue, GetFullAxTreeParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetContentQuadsParams, GetFrameOwnerParams,
    ResolveNodeParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EventJavascriptDialogOpening, EventScreencastFrame, FrameId,
    GetFrameTreeParams, GetNavigationHistoryParams, HandleJavaScriptDialogParams, NavigateParams,
    NavigateToHistoryEntryParams, ScreencastFrameAckParams, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams, Viewport as ScreenshotViewport,
};
use chromiumoxide::cdp::browser_protocol::storage::{ClearCookiesParams, ClearDataForOriginParams};
use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, ExecutionContextId, ReleaseObjectParams,
};
use chromiumoxide::error::CdpError;
use chromiumoxide::keys::get_key_definition;
use chromiumoxide::listeners::EventStream;
use chromiumoxide::page::ScreenshotParams;
use futures_util::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, oneshot};

use super::StepError;
use super::context::ActiveContext;
use crate::browser::{BrowserContext, BrowserHost, BrowserStatus, Geolocation, Viewport};
use crate::flow::{
    Assertion, ClearTarget, CompiledFlow, CompiledStep, Crop, Expression, FrameSwitch, GuardKind,
    Key, Locator, LocatorStrategy, MAX_RUNTIME_VALUE_BYTES, Modifier, NamedKey,
    NativeDialogResponse, Operation, PageSwitch, PresentationOverlays, RecordingControl, Redactor,
    RelationKind, RelativePoint, Resolved, SettleCondition, TextMatch, UrlExpectation, VideoMode,
    VisualExpectation, When,
};
use crate::locator::{
    Actionability, LocatorEngine, LocatorError, Observation, POLL_INTERVAL, ResolvedElement,
    id_selector, retryable, retryable_cdp_message, text_matches,
};
use crate::oopif::{CdpTarget, OopifRouter};
use crate::report::{
    ArtifactPaths, Failure, FailureCategory, FlowReport, FlowStatus, SafeText, StepContext,
};
use crate::session_snapshot::{
    Bounds as SnapshotBounds, CapturedElement, CapturedSnapshot, LocatorIdentity,
    Scroll as SnapshotScroll, SemanticNode, SemanticState, Viewport as SnapshotViewport,
};
use crate::video::{VideoConfig, VideoRecorder};
use crate::visual;

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
pub(crate) struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}
pub(crate) async fn wait_for_cancellation(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn browser_unavailable(
    status: &mut tokio::sync::watch::Receiver<BrowserStatus>,
) -> String {
    loop {
        match status.borrow().clone() {
            BrowserStatus::Running => {}
            BrowserStatus::Failed(error) => return format!("Chromium is unavailable: {error}"),
            BrowserStatus::Closed => return "Chromium is closed".to_owned(),
        }
        if status.changed().await.is_err() {
            return "Chromium status monitor stopped".to_owned();
        }
    }
}

pub(crate) fn is_cancelled(cancellation: Option<&CancellationToken>) -> bool {
    cancellation.is_some_and(CancellationToken::is_cancelled)
}
