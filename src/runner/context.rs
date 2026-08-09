#![allow(unused_imports)]
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
use super::actions::find_frame;
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

pub(crate) struct ActiveContext {
    pub(crate) page: Page,
    pub(crate) router: Option<Arc<OopifRouter>>,
    pub(crate) frames: Vec<ActiveFrame>,
}

pub(crate) struct ActiveFrame {
    pub(crate) id: FrameId,
}

#[derive(Clone, Copy)]
pub(crate) struct PageSettings {
    pub(crate) viewport: Viewport,
    pub(crate) geolocation: Option<Geolocation>,
}

impl ActiveContext {
    pub(crate) fn new(page: Page) -> Self {
        Self {
            page,
            router: None,
            frames: Vec::new(),
        }
    }

    pub(crate) fn with_router(page: Page, router: Arc<OopifRouter>) -> Self {
        let mut active = Self::new(page);
        active.router = Some(router);
        active
    }

    pub(crate) fn frame(&self) -> Option<&FrameId> {
        self.frames.last().map(|frame| &frame.id)
    }

    pub(crate) fn oopif_index(&self) -> Option<usize> {
        let router = self.router.as_deref()?;
        self.frames
            .iter()
            .rposition(|frame| router.has_target(frame.id.as_ref()))
    }

    pub(crate) fn target(&self) -> CdpTarget<'_> {
        self.oopif_index()
            .map_or(CdpTarget::Root(&self.page), |index| {
                CdpTarget::Oopif(
                    self.router.as_deref().expect("OOPIF router missing"),
                    self.frames[index].id.as_ref(),
                )
            })
    }

    pub(crate) fn target_before(&self, frame_index: usize) -> CdpTarget<'_> {
        self.frames[..frame_index]
            .iter()
            .rposition(|frame| {
                self.router
                    .as_deref()
                    .is_some_and(|router| router.has_target(frame.id.as_ref()))
            })
            .map_or(CdpTarget::Root(&self.page), |index| {
                CdpTarget::Oopif(
                    self.router.as_deref().expect("OOPIF router missing"),
                    self.frames[index].id.as_ref(),
                )
            })
    }

    pub(crate) fn local_frame(&self) -> Option<&FrameId> {
        match self.oopif_index() {
            Some(index) if index + 1 == self.frames.len() => None,
            _ => self.frame(),
        }
    }

    pub(crate) fn locator(&self) -> LocatorEngine<'_> {
        LocatorEngine::in_target(self.target(), self.local_frame())
    }

    pub(crate) async fn url(&self) -> anyhow::Result<Option<String>> {
        let tree = self
            .target()
            .execute(GetFrameTreeParams::default())
            .await?
            .frame_tree;
        Ok(match self.local_frame() {
            Some(frame) => find_frame(&tree, frame).map(|frame| frame.url.clone()),
            None => Some(tree.frame.url),
        })
    }
}
