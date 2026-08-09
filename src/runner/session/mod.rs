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

use super::actions::*;
use super::actions::{call_on_target, page_point, sleep_until_poll};
use super::assert::*;
use super::cancel::*;
use super::context::*;
use super::guards::*;
use super::http::*;
use super::interactive::*;
use super::outputs::*;
use super::snapshot::*;
use super::snapshot::{
    SnapshotNodeMetadata, ax_text, bounded_inspection_text, bounded_snapshot_text, locator_json,
    simple_locator, snapshot_ax_node, snapshot_state, snapshot_transform, stable_dom_id,
};
use super::state::RuntimeState;
use super::{
    CLEAR_CACHE_STORAGE_EXPRESSION, CLEAR_INDEXEDDB_EXPRESSION, CLEAR_STORAGE_EXPRESSION,
    ERASE_FUNCTION, FINAL_FRAME_DELAY, FOCUS_FUNCTION, FRAME_SIZE_FUNCTION, INNER_TEXT_FUNCTION,
    INSPECT_AX_BYTES, INSPECT_AX_DEPTH, INSPECT_AX_NODES, INSPECT_PAGES, INSPECT_TEXT_CHARS,
    INSPECTION_TIMEOUT, PREPARE_FILL_FUNCTION, RECORDING_NAME, SCREENSHOT_NAME, SECONDARY_TIMEOUT,
    SELECT_FUNCTION, SNAPSHOT_AX_DEPTH, SNAPSHOT_ELEMENTS, SNAPSHOT_NODE_FUNCTION,
    SNAPSHOT_TEXT_CHARS, StepError, VIDEO_FINALIZE_TIMEOUT, assertion_locator_error,
    browser_error_category, failure, locator_error, path_text, protocol, report, safe,
    step_context,
};
use super::{deadline_timeout_ms, operation_locator, settle_video};
use crate::browser::{BrowserContext, BrowserHost, BrowserStatus, Geolocation, Viewport};
use crate::flow::{
    Assertion, ClearTarget, CompiledFlow, CompiledStep, Crop, Expression, FrameSwitch, GuardKind,
    Key, Locator, LocatorStrategy, MAX_RUNTIME_VALUE_BYTES, Modifier, NamedKey,
    NativeDialogResponse, Operation, PageSwitch, PresentationOverlays, RecordingControl, Redactor,
    RelationKind, RelativePoint, Resolved, SettleCondition, TextMatch, UrlExpectation, VideoMode,
    VisualExpectation, When,
};
use crate::flow::{MAX_GESTURE_DURATION, meets_min_secret_len};
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
mod execute;
mod overlays;
mod recorder;
mod runtime;

pub(crate) use execute::{execute_flow, execute_step};
pub(crate) use overlays::{
    deactivate_presentation_overlay, pause_until, remove_presentation_overlay,
    step_captures_screenshot, update_presentation_overlay,
};
pub use recorder::SessionRecordingFinish;
pub(crate) use recorder::{
    ScreencastSource, SessionRecorder, VideoFinishError, VideoSession, VideoStartAwait,
    VideoStartup, apply_video_finish, await_video_start, capture_failure_screenshot,
    capture_screenshot, publish_bytes, screenshot_bytes, should_retain_video, start_video,
    step_failure, stop_screencast, video_start_cleanup_error,
};
pub(crate) use runtime::SessionRuntime;
pub use runtime::{SessionInspection, SessionPage};

/// Per-flow resources. Give each concurrent flow a distinct artifact directory.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub artifact_directory: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
    pub cancellation: Option<CancellationToken>,
    #[cfg(test)]
    pub(crate) step_started_observer: Option<StepStartedObserver>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct StepStartedObserver(pub(crate) Arc<dyn Fn(&'static str) + Send + Sync>);

#[cfg(test)]
impl fmt::Debug for StepStartedObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StepStartedObserver")
    }
}

impl RunOptions {
    pub fn new(artifact_directory: impl Into<PathBuf>) -> Self {
        Self {
            artifact_directory: artifact_directory.into(),
            ffmpeg_path: None,
            cancellation: None,
            #[cfg(test)]
            step_started_observer: None,
        }
    }

    pub fn with_ffmpeg(mut self, ffmpeg_path: impl Into<PathBuf>) -> Self {
        self.ffmpeg_path = Some(ffmpeg_path.into());
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

pub async fn run_flow(host: &BrowserHost, flow: &CompiledFlow, options: &RunOptions) -> FlowReport {
    let started = Instant::now();
    let artifacts = ArtifactPaths {
        directory: path_text(&options.artifact_directory),
        ..ArtifactPaths::default()
    };
    if is_cancelled(options.cancellation.as_ref()) {
        return report(flow, started, artifacts, Vec::new(), true);
    }
    let mut session = match SessionRuntime::open(host, flow).await {
        Ok(session) => session,
        Err(error) => {
            let category = browser_error_category(host);
            return report(
                flow,
                started,
                artifacts,
                vec![failure(flow, category, error.to_string(), None)],
                is_cancelled(options.cancellation.as_ref()),
            );
        }
    };
    let mut result = session.execute(host, flow, options).await;
    match tokio::time::timeout(SECONDARY_TIMEOUT * 2, session.close(host)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => result.failures.push(failure(
            flow,
            browser_error_category(host),
            error.to_string(),
            None,
        )),
        Err(_) => result.failures.push(failure(
            flow,
            FailureCategory::Protocol,
            "dispose browser context timed out",
            None,
        )),
    }
    if !result.failures.is_empty() && result.status == FlowStatus::Passed {
        result.status = FlowStatus::Failed;
    }
    result
}
