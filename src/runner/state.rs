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
use super::context::{ActiveContext, PageSettings};
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

pub(crate) struct RuntimeState {
    pub(crate) outputs: BTreeMap<String, Resolved<Value>>,
    pub(crate) redactor: Redactor,
    pub(crate) page_settings: PageSettings,
    pub(crate) guard_results: BTreeMap<usize, bool>,
    pub(crate) stopped_loops: BTreeSet<usize>,
    pub(crate) expects_dialog: bool,
    pub(crate) dialog_listener: Option<EventStream<EventJavascriptDialogOpening>>,
    pub(crate) presentation_overlays: PresentationOverlays,
    pub(crate) presentation_overlay_recording: bool,
}

impl RuntimeState {
    pub(crate) fn presentation_overlays_active(&self) -> bool {
        self.presentation_overlay_recording
            && self.presentation_overlays != PresentationOverlays::default()
    }
}
