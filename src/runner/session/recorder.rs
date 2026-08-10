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

use super::super::actions::*;
use super::super::actions::{call_on_target, page_point, sleep_until_poll};
use super::super::assert::*;
use super::super::cancel::*;
use super::super::context::*;
use super::super::guards::*;
use super::super::http::*;
use super::super::interactive::*;
use super::super::outputs::*;
use super::super::snapshot::*;
use super::super::snapshot::{
    SnapshotNodeMetadata, ax_text, bounded_inspection_text, bounded_snapshot_text, locator_json,
    simple_locator, snapshot_ax_node, snapshot_state, snapshot_transform, stable_dom_id,
};
use super::super::state::RuntimeState;
use super::super::{
    CLEAR_CACHE_STORAGE_EXPRESSION, CLEAR_INDEXEDDB_EXPRESSION, CLEAR_STORAGE_EXPRESSION,
    ERASE_FUNCTION, FINAL_FRAME_DELAY, FOCUS_FUNCTION, FRAME_SIZE_FUNCTION, INNER_TEXT_FUNCTION,
    INSPECT_AX_BYTES, INSPECT_AX_DEPTH, INSPECT_AX_NODES, INSPECT_PAGES, INSPECT_TEXT_CHARS,
    INSPECTION_TIMEOUT, PREPARE_FILL_FUNCTION, RECORDING_NAME, SCREENSHOT_NAME, SECONDARY_TIMEOUT,
    SELECT_FUNCTION, SNAPSHOT_AX_DEPTH, SNAPSHOT_ELEMENTS, SNAPSHOT_NODE_FUNCTION,
    SNAPSHOT_TEXT_CHARS, StepError, VIDEO_FINALIZE_TIMEOUT, assertion_locator_error,
    browser_error_category, failure, locator_error, path_text, protocol, report, safe,
    step_context,
};
use super::super::{deadline_timeout_ms, operation_locator, settle_video};
use super::RunOptions;
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

pub(crate) struct VideoSession {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<(VideoRecorder, Result<(), String>)>,
    partial_path: PathBuf,
}

pub(crate) struct ScreencastSource {
    page: Page,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

pub(crate) struct SessionRecorder {
    recorder: Option<VideoRecorder>,
    source: Option<ScreencastSource>,
    output_path: PathBuf,
    partial_path: PathBuf,
    viewport_width: u32,
    viewport_height: u32,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionRecordingFinish {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_path: Option<String>,
    pub warnings: Vec<String>,
}

impl SessionRecorder {
    pub(crate) async fn start(config: VideoConfig, page: &Page) -> Result<Self, String> {
        tokio::fs::create_dir_all(
            config
                .output_path
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        )
        .await
        .map_err(|error| format!("create recording directory: {error}"))?;
        let recorder = VideoRecorder::start(&config)
            .await
            .map_err(|error| error.to_string())?;
        let mut session = Self {
            output_path: config.output_path.clone(),
            partial_path: config.partial_path(),
            recorder: Some(recorder),
            source: None,
            viewport_width: config.viewport_width,
            viewport_height: config.viewport_height,
            warnings: Vec::new(),
        };
        session.bind(page).await?;
        Ok(session)
    }

    pub(crate) async fn bind(&mut self, page: &Page) -> Result<(), String> {
        if self
            .source
            .as_ref()
            .is_some_and(|source| source.page.target_id() == page.target_id())
        {
            return Ok(());
        }
        self.stop_source().await;
        let Some(recorder) = &self.recorder else {
            return Ok(());
        };
        let mut events = page
            .event_listener::<EventScreencastFrame>()
            .await
            .map_err(|error| format!("listen for screencast frames: {error}"))?;
        page.execute(
            StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(90)
                .max_width(i64::from(self.viewport_width))
                .max_height(i64::from(self.viewport_height))
                .every_nth_frame(1)
                .build(),
        )
        .await
        .map_err(|error| format!("start screencast: {error}"))?;
        let sink = recorder.frame_sink();
        let task_page = page.clone();
        let (stop, mut stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return Ok(()),
                    event = events.next() => {
                        let Some(event) = event else {
                            return Err("screencast event stream closed".to_owned());
                        };
                        task_page
                            .execute(ScreencastFrameAckParams::new(event.session_id))
                            .await
                            .map_err(|error| format!("acknowledge screencast frame: {error}"))?;
                        let jpeg = base64::engine::general_purpose::STANDARD
                            .decode(event.data.as_ref() as &[u8])
                            .map_err(|error| format!("decode screencast frame: {error}"))?;
                        sink.push_frame(jpeg);
                    }
                }
            }
        });
        self.source = Some(ScreencastSource {
            page: page.clone(),
            stop: Some(stop),
            task,
        });
        Ok(())
    }

    pub(crate) async fn stop_source(&mut self) {
        let Some(mut source) = self.source.take() else {
            return;
        };
        if let Some(error) = stop_screencast(&source.page).await {
            self.warnings.push(error);
        }
        if let Some(stop) = source.stop.take() {
            let _ = stop.send(());
        }
        match tokio::time::timeout(SECONDARY_TIMEOUT, &mut source.task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => self.warnings.push(error),
            Ok(Err(error)) => self
                .warnings
                .push(format!("screencast task failed: {error}")),
            Err(_) => {
                source.task.abort();
                self.warnings
                    .push("screencast task shutdown timed out".to_owned());
            }
        }
    }

    pub(crate) async fn finalize(mut self) -> SessionRecordingFinish {
        let final_frame = if let Some(source) = &self.source {
            settle_video(&source.page).await;
            match source
                .page
                .screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Jpeg)
                        .quality(90)
                        .build(),
                )
                .await
            {
                Ok(frame) => Some(frame),
                Err(error) => {
                    self.warnings
                        .push(format!("capture final recording frame: {error}"));
                    None
                }
            }
        } else {
            None
        };
        self.stop_source().await;
        if let (Some(recorder), Some(frame)) = (&self.recorder, final_frame) {
            recorder.push_frame(frame);
        }
        let result = match self.recorder.take() {
            Some(recorder) => {
                tokio::time::timeout(
                    VIDEO_FINALIZE_TIMEOUT,
                    recorder.finalize(Instant::now(), false),
                )
                .await
            }
            None => return self.finish_result(None),
        };
        let path = match result {
            Ok(Ok(path)) => path,
            Ok(Err(error)) => {
                self.warnings.push(error.to_string());
                None
            }
            Err(_) => {
                self.warnings
                    .push("video finalization timed out".to_owned());
                None
            }
        };
        self.finish_result(path)
    }

    pub(crate) fn finish_result(self, path: Option<PathBuf>) -> SessionRecordingFinish {
        let partial = self
            .partial_path
            .exists()
            .then(|| path_text(&self.partial_path));
        let complete = path.or_else(|| self.output_path.exists().then(|| self.output_path.clone()));
        SessionRecordingFinish {
            status: if complete.is_some() {
                "complete"
            } else if partial.is_some() {
                "partial"
            } else {
                "failed"
            },
            path: complete.as_deref().map(path_text),
            partial_path: partial,
            warnings: self.warnings,
        }
    }
}

pub(crate) enum VideoStartup {
    Ready(Option<VideoSession>),
    Cancelled(Option<Result<Option<PathBuf>, VideoFinishError>>),
}

impl VideoSession {
    pub(crate) async fn finish(
        mut self,
        page: &Page,
        flow_failed: bool,
        stop_at: Instant,
    ) -> Result<Option<PathBuf>, VideoFinishError> {
        let mut errors = Vec::new();
        if let Some(error) = stop_screencast(page).await {
            errors.push(error);
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let (recorder, stream_result) =
            match tokio::time::timeout(SECONDARY_TIMEOUT, &mut self.task).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    return Err(VideoFinishError::Complete {
                        error: format!("screencast task failed: {error}"),
                        partial: self.partial_path.clone(),
                        recording: None,
                    });
                }
                Err(_) => {
                    self.task.abort();
                    let _ = (&mut self.task).await;
                    return Err(VideoFinishError::Complete {
                        error: "screencast task shutdown timed out".to_owned(),
                        partial: self.partial_path.clone(),
                        recording: None,
                    });
                }
            };
        if let Err(error) = stream_result {
            errors.push(error);
        }
        let recording = match tokio::time::timeout(
            VIDEO_FINALIZE_TIMEOUT,
            recorder.finalize(stop_at, should_retain_video(flow_failed, &errors)),
        )
        .await
        {
            Ok(Ok(recording)) => recording,
            Ok(Err(error)) => {
                return Err(VideoFinishError::Complete {
                    error: error.to_string(),
                    partial: self.partial_path.clone(),
                    recording: None,
                });
            }
            Err(_) => {
                return Err(VideoFinishError::Complete {
                    error: "video finalization timed out".to_owned(),
                    partial: self.partial_path.clone(),
                    recording: None,
                });
            }
        };
        if !errors.is_empty() {
            return Err(VideoFinishError::Complete {
                error: errors.join("; "),
                partial: self.partial_path.clone(),
                recording,
            });
        }
        Ok(recording)
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn should_retain_video(flow_failed: bool, screencast_errors: &[String]) -> bool {
    flow_failed || !screencast_errors.is_empty()
}

pub(crate) enum VideoFinishError {
    Complete {
        error: String,
        partial: PathBuf,
        recording: Option<PathBuf>,
    },
}

pub(crate) fn apply_video_finish(
    finish: Result<Option<PathBuf>, VideoFinishError>,
    artifacts: &mut ArtifactPaths,
    recording_error: &mut Option<String>,
) {
    match finish {
        Ok(Some(path)) => artifacts.recording = Some(path_text(&path)),
        Ok(None) => {}
        Err(VideoFinishError::Complete {
            error,
            partial,
            recording,
        }) => {
            if let Some(recording) = recording.filter(|path| path.exists()) {
                artifacts.recording = Some(path_text(&recording));
            }
            if partial.exists() {
                artifacts.partial_recording = Some(path_text(&partial));
            }
            *recording_error = Some(error);
        }
    }
}

pub(crate) async fn start_video(
    page: &Page,
    flow: &CompiledFlow,
    options: &RunOptions,
    deadline: Instant,
) -> Result<VideoStartup, String> {
    if is_cancelled(options.cancellation.as_ref()) {
        return Ok(VideoStartup::Cancelled(None));
    }
    if flow.settings.video == VideoMode::Off {
        return Ok(VideoStartup::Ready(None));
    }
    let ffmpeg_path = options
        .ffmpeg_path
        .as_ref()
        .ok_or_else(|| "video is enabled but no FFmpeg path was provided".to_owned())?;
    let result = match await_video_start(
        options.cancellation.as_ref(),
        deadline,
        tokio::fs::create_dir_all(&options.artifact_directory),
    )
    .await
    {
        VideoStartAwait::Ready(result) => result,
        VideoStartAwait::Cancelled => return Ok(VideoStartup::Cancelled(None)),
        VideoStartAwait::Deadline => return Err("recording start deadline expired".to_owned()),
    };
    result.map_err(|error| format!("create artifact directory: {error}"))?;
    let config = VideoConfig {
        mode: flow.settings.video,
        ffmpeg_path: ffmpeg_path.clone(),
        output_path: options.artifact_directory.join(RECORDING_NAME),
        viewport_width: flow.settings.viewport.width,
        viewport_height: flow.settings.viewport.height,
    };
    let partial_path = config.partial_path();
    let events = match await_video_start(
        options.cancellation.as_ref(),
        deadline,
        page.event_listener::<EventScreencastFrame>(),
    )
    .await
    {
        VideoStartAwait::Ready(events) => events,
        VideoStartAwait::Cancelled => return Ok(VideoStartup::Cancelled(None)),
        VideoStartAwait::Deadline => return Err("recording start deadline expired".to_owned()),
    };
    let mut events = events.map_err(|error| error.to_string())?;
    let command = page.execute(
        StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(90)
            .max_width(i64::from(flow.settings.viewport.width))
            .max_height(i64::from(flow.settings.viewport.height))
            .every_nth_frame(1)
            .build(),
    );
    let started = match await_video_start(options.cancellation.as_ref(), deadline, command).await {
        VideoStartAwait::Ready(started) => started,
        VideoStartAwait::Cancelled => {
            let cleanup = stop_screencast(page).await.map(|error| {
                Err(VideoFinishError::Complete {
                    error,
                    partial: partial_path,
                    recording: None,
                })
            });
            return Ok(VideoStartup::Cancelled(cleanup));
        }
        VideoStartAwait::Deadline => {
            return Err(video_start_cleanup_error(
                "recording start deadline expired",
                stop_screencast(page).await,
            ));
        }
    };
    if let Err(error) = started {
        let cleanup = stop_screencast(page).await;
        return Err(match cleanup {
            Some(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
            None => error.to_string(),
        });
    }
    let recorder = match await_video_start(
        options.cancellation.as_ref(),
        deadline,
        VideoRecorder::start(&config),
    )
    .await
    {
        VideoStartAwait::Ready(recorder) => recorder,
        VideoStartAwait::Cancelled => {
            let cleanup = stop_screencast(page).await.map(|error| {
                Err(VideoFinishError::Complete {
                    error,
                    partial: partial_path,
                    recording: None,
                })
            });
            return Ok(VideoStartup::Cancelled(cleanup));
        }
        VideoStartAwait::Deadline => {
            return Err(video_start_cleanup_error(
                "recording start deadline expired",
                stop_screencast(page).await,
            ));
        }
    };
    let recorder = match recorder {
        Ok(recorder) => recorder,
        Err(error) => {
            let cleanup = stop_screencast(page).await;
            return Err(match cleanup {
                Some(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
                None => error.to_string(),
            });
        }
    };
    let task_page = page.clone();
    let (stop, mut stop_rx) = oneshot::channel();
    let (first_frame, first_frame_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut first_frame = Some(first_frame);
        let result = loop {
            tokio::select! {
                _ = &mut stop_rx => break Ok(()),
                event = events.next() => {
                    let Some(event) = event else {
                        break Err("screencast event stream closed".to_owned());
                    };
                    if let Err(error) = task_page.execute(ScreencastFrameAckParams::new(event.session_id)).await {
                        break Err(format!("acknowledge screencast frame: {error}"));
                    }
                    match base64::engine::general_purpose::STANDARD.decode(event.data.as_ref() as &[u8]) {
                        Ok(jpeg) => {
                            recorder.push_frame(jpeg);
                            if let Some(first_frame) = first_frame.take() {
                                let _ = first_frame.send(());
                            }
                        }
                        Err(error) => break Err(format!("decode screencast frame: {error}")),
                    }
                }
            }
        };
        (recorder, result)
    });
    let session = VideoSession {
        stop: Some(stop),
        task,
        partial_path,
    };
    let first_frame =
        match await_video_start(options.cancellation.as_ref(), deadline, first_frame_rx).await {
            VideoStartAwait::Ready(first_frame) => first_frame,
            VideoStartAwait::Cancelled => {
                return Ok(VideoStartup::Cancelled(Some(
                    session.finish(page, true, Instant::now()).await,
                )));
            }
            VideoStartAwait::Deadline => {
                let cleanup = session
                    .finish(page, true, Instant::now())
                    .await
                    .err()
                    .map(|VideoFinishError::Complete { error, .. }| error);
                return Err(video_start_cleanup_error(
                    "recording start deadline expired",
                    cleanup,
                ));
            }
        };
    let first_frame_error = match first_frame {
        Ok(()) => return Ok(VideoStartup::Ready(Some(session))),
        Err(_) => "screencast ended before the first frame".to_owned(),
    };
    let cleanup_error = session
        .finish(page, true, Instant::now())
        .await
        .err()
        .map(|VideoFinishError::Complete { error, .. }| error);
    Err(match cleanup_error {
        Some(cleanup) => format!("{first_frame_error}; cleanup failed: {cleanup}"),
        None => first_frame_error,
    })
}

pub(crate) enum VideoStartAwait<T> {
    Ready(T),
    Cancelled,
    Deadline,
}

pub(crate) async fn await_video_start<T>(
    cancellation: Option<&CancellationToken>,
    deadline: Instant,
    future: impl Future<Output = T>,
) -> VideoStartAwait<T> {
    tokio::select! {
        biased;
        _ = wait_for_cancellation(cancellation) => VideoStartAwait::Cancelled,
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future) => {
            match result {
                Ok(result) => VideoStartAwait::Ready(result),
                Err(_) => VideoStartAwait::Deadline,
            }
        }
    }
}

pub(crate) fn video_start_cleanup_error(message: &str, cleanup: Option<String>) -> String {
    cleanup.map_or_else(
        || message.to_owned(),
        |cleanup| format!("{message}; cleanup failed: {cleanup}"),
    )
}

pub(crate) async fn stop_screencast(page: &Page) -> Option<String> {
    match tokio::time::timeout(
        SECONDARY_TIMEOUT,
        page.execute(StopScreencastParams::default()),
    )
    .await
    {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(format!("stop screencast: {error}")),
        Err(_) => Some("stop screencast timed out".to_owned()),
    }
}

pub(crate) async fn capture_failure_screenshot(
    active: &ActiveContext,
    directory: &Path,
) -> Option<PathBuf> {
    match tokio::time::timeout(
        SECONDARY_TIMEOUT,
        capture_screenshot(active, directory, SCREENSHOT_NAME, None),
    )
    .await
    {
        Ok(Ok(path)) => Some(path),
        _ => None,
    }
}

pub(crate) async fn capture_screenshot(
    active: &ActiveContext,
    directory: &Path,
    file_name: &str,
    crop: Option<Crop>,
) -> Result<PathBuf, StepError> {
    let bytes = screenshot_bytes(active, crop).await?;
    let path = directory.join(file_name);
    publish_bytes(directory, &path, &bytes).await?;
    Ok(path)
}

pub(crate) async fn screenshot_bytes(
    active: &ActiveContext,
    crop: Option<Crop>,
) -> Result<Vec<u8>, StepError> {
    let mut params = ScreenshotParams::builder().format(CaptureScreenshotFormat::Png);
    if active.frame().is_some() || crop.is_some() {
        let viewport = active
            .page
            .layout_metrics()
            .await
            .map_err(protocol)?
            .css_visual_viewport;
        let (frame_x, frame_y) = page_point(active, 0.0, 0.0).await?;
        let [frame_width, frame_height]: [f64; 2] =
            evaluate_value(active, "[innerWidth, innerHeight]").await?;
        if active.frame().is_some()
            && crop.is_some_and(|crop| {
                f64::from(crop.x + crop.width) > frame_width
                    || f64::from(crop.y + crop.height) > frame_height
            })
        {
            return Err(StepError::new(
                FailureCategory::Protocol,
                "screenshot crop must fit within the active frame viewport",
            ));
        }
        let (x, y, width, height) = crop.map_or((0.0, 0.0, frame_width, frame_height), |crop| {
            (
                f64::from(crop.x),
                f64::from(crop.y),
                f64::from(crop.width),
                f64::from(crop.height),
            )
        });
        params = params.clip(ScreenshotViewport {
            x: viewport.page_x + frame_x + x,
            y: viewport.page_y + frame_y + y,
            width,
            height,
            scale: 1.0,
        });
    }
    let bytes = active
        .page
        .screenshot(params.build())
        .await
        .map_err(protocol)?;
    if bytes.len() > visual::MAX_IMAGE_BYTES {
        return Err(protocol("captured screenshot exceeds the image byte limit"));
    }
    Ok(bytes)
}

pub(crate) async fn publish_bytes(
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), StepError> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| protocol(format!("create artifact directory: {error}")))?;
    let temporary = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| protocol(format!("create temporary screenshot: {error}")))?;
    let mut writer = tokio::fs::File::from_std(
        temporary
            .reopen()
            .map_err(|error| protocol(format!("open temporary screenshot: {error}")))?,
    );
    writer
        .write_all(bytes)
        .await
        .map_err(|error| protocol(format!("write screenshot: {error}")))?;
    writer
        .sync_all()
        .await
        .map_err(|error| protocol(format!("flush screenshot: {error}")))?;
    drop(writer);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(protocol(format!("replace screenshot: {error}"))),
    }
    // Keep publication await-free so cancellation cannot publish an unreported screenshot.
    temporary
        .persist(path)
        .map_err(|error| protocol(format!("publish screenshot: {}", error.error)))?;
    Ok(())
}

pub(crate) async fn step_failure(
    host: &BrowserHost,
    flow: &CompiledFlow,
    redactor: &Redactor,
    active: &ActiveContext,
    step: &CompiledStep,
    error: StepError,
) -> Failure {
    let current_url = tokio::time::timeout(SECONDARY_TIMEOUT, active.url())
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .map(|url| SafeText::public(redactor.redact(&url)));
    let category = match host.status() {
        BrowserStatus::Running => error.category,
        BrowserStatus::Failed(_) | BrowserStatus::Closed => FailureCategory::BrowserCrash,
    };
    let timeout_ms = deadline_timeout_ms(&error, step.timeout);
    let mut failure = Failure::new(category, SafeText::public(redactor.redact(&error.message)));
    failure.step = Some(step_context(flow, step));
    failure.current_url = current_url;
    failure.timeout_ms = timeout_ms;
    failure.last_observed = error
        .last_observed
        .map(|value| SafeText::public(redactor.redact(&value)));
    failure
}
