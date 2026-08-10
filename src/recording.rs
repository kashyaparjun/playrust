//! Shared browser video recording lifecycle for batch flows and interactive sessions.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use base64::Engine as _;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams,
};
use chromiumoxide::page::ScreenshotParams;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::report::ArtifactPaths;
use crate::video::{VideoConfig, VideoRecorder};

const SECONDARY_TIMEOUT: Duration = Duration::from_secs(2);
const VIDEO_FINALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const FINAL_FRAME_DELAY: Duration = Duration::from_millis(250);

pub trait FlowCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

#[derive(Debug, Serialize)]
pub struct RecordingFinish {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_path: Option<String>,
    pub warnings: Vec<String>,
}

pub type SessionRecordingFinish = RecordingFinish;

#[derive(Clone, Copy, Debug)]
pub struct FinalizeOptions {
    pub retain_on_failure: bool,
    pub capture_final_frame: bool,
}

impl Default for FinalizeOptions {
    fn default() -> Self {
        Self {
            retain_on_failure: false,
            capture_final_frame: true,
        }
    }
}

struct ScreencastSource {
    page: Page,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

pub struct RecordingController {
    recorder: Option<VideoRecorder>,
    source: Option<ScreencastSource>,
    output_path: PathBuf,
    partial_path: PathBuf,
    viewport_width: u32,
    viewport_height: u32,
    warnings: Vec<String>,
}

pub enum FlowRecordingStartup {
    Ready(Option<FlowRecording>),
    /// Early cancellation after optional recording cleanup. `Ok` keeps any
    /// finalized path; `Err` records cleanup/finalization failures.
    Cancelled(Option<Result<Option<PathBuf>, FlowRecordingFinish>>),
}

pub struct FlowRecording {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<(VideoRecorder, Result<(), String>)>,
    partial_path: PathBuf,
}

pub enum FlowRecordingFinish {
    Complete {
        error: String,
        partial: PathBuf,
        recording: Option<PathBuf>,
    },
}

impl RecordingController {
    pub async fn start(config: VideoConfig, page: &Page) -> Result<Self, String> {
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
        let mut controller = Self {
            output_path: config.output_path.clone(),
            partial_path: config.partial_path(),
            recorder: Some(recorder),
            source: None,
            viewport_width: config.viewport_width,
            viewport_height: config.viewport_height,
            warnings: Vec::new(),
        };
        controller.bind(page).await?;
        Ok(controller)
    }

    pub async fn bind(&mut self, page: &Page) -> Result<(), String> {
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

    pub async fn finalize(mut self, options: FinalizeOptions) -> RecordingFinish {
        let final_frame = if options.capture_final_frame {
            if let Some(source) = &self.source {
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
                    recorder.finalize(Instant::now(), options.retain_on_failure),
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

    async fn stop_source(&mut self) {
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

    fn finish_result(self, path: Option<PathBuf>) -> RecordingFinish {
        let partial = self
            .partial_path
            .exists()
            .then(|| path_text(&self.partial_path));
        let complete = path.or_else(|| self.output_path.exists().then(|| self.output_path.clone()));
        RecordingFinish {
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

impl FlowRecording {
    pub async fn start(
        page: &Page,
        config: VideoConfig,
        cancellation: Option<&dyn FlowCancellation>,
        deadline: Instant,
    ) -> Result<FlowRecordingStartup, String> {
        if cancellation.is_some_and(FlowCancellation::is_cancelled) {
            return Ok(FlowRecordingStartup::Cancelled(None));
        }
        let partial_path = config.partial_path();
        let result = match await_flow_start(
            cancellation,
            deadline,
            tokio::fs::create_dir_all(
                config
                    .output_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            ),
        )
        .await
        {
            FlowStartAwait::Ready(result) => result,
            FlowStartAwait::Cancelled => return Ok(FlowRecordingStartup::Cancelled(None)),
            FlowStartAwait::Deadline => {
                return Err("recording start deadline expired".to_owned());
            }
        };
        result.map_err(|error| format!("create artifact directory: {error}"))?;
        let events = match await_flow_start(
            cancellation,
            deadline,
            page.event_listener::<EventScreencastFrame>(),
        )
        .await
        {
            FlowStartAwait::Ready(events) => events,
            FlowStartAwait::Cancelled => return Ok(FlowRecordingStartup::Cancelled(None)),
            FlowStartAwait::Deadline => {
                return Err("recording start deadline expired".to_owned());
            }
        };
        let mut events = events.map_err(|error| error.to_string())?;
        let command = page.execute(
            StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(90)
                .max_width(i64::from(config.viewport_width))
                .max_height(i64::from(config.viewport_height))
                .every_nth_frame(1)
                .build(),
        );
        let started = match await_flow_start(cancellation, deadline, command).await {
            FlowStartAwait::Ready(started) => started,
            FlowStartAwait::Cancelled => {
                let cleanup =
                    stop_screencast(page)
                        .await
                        .map(|error| FlowRecordingFinish::Complete {
                            error,
                            partial: partial_path,
                            recording: None,
                        });
                return Ok(FlowRecordingStartup::Cancelled(cleanup.map(Err)));
            }
            FlowStartAwait::Deadline => {
                return Err(flow_start_cleanup_error(
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
        let recorder =
            match await_flow_start(cancellation, deadline, VideoRecorder::start(&config)).await {
                FlowStartAwait::Ready(recorder) => recorder,
                FlowStartAwait::Cancelled => {
                    let cleanup =
                        stop_screencast(page)
                            .await
                            .map(|error| FlowRecordingFinish::Complete {
                                error,
                                partial: partial_path,
                                recording: None,
                            });
                    return Ok(FlowRecordingStartup::Cancelled(cleanup.map(Err)));
                }
                FlowStartAwait::Deadline => {
                    return Err(flow_start_cleanup_error(
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
        let session = Self {
            stop: Some(stop),
            task,
            partial_path,
        };
        let first_frame = match await_flow_start(cancellation, deadline, first_frame_rx).await {
            FlowStartAwait::Ready(first_frame) => first_frame,
            FlowStartAwait::Cancelled => {
                return Ok(FlowRecordingStartup::Cancelled(Some(
                    session.finish(page, true, Instant::now()).await,
                )));
            }
            FlowStartAwait::Deadline => {
                let cleanup = session
                    .finish(page, true, Instant::now())
                    .await
                    .err()
                    .map(|FlowRecordingFinish::Complete { error, .. }| error);
                return Err(flow_start_cleanup_error(
                    "recording start deadline expired",
                    cleanup,
                ));
            }
        };
        let first_frame_error = match first_frame {
            Ok(()) => return Ok(FlowRecordingStartup::Ready(Some(session))),
            Err(_) => "screencast ended before the first frame".to_owned(),
        };
        let cleanup_error = session
            .finish(page, true, Instant::now())
            .await
            .err()
            .map(|FlowRecordingFinish::Complete { error, .. }| error);
        Err(match cleanup_error {
            Some(cleanup) => format!("{first_frame_error}; cleanup failed: {cleanup}"),
            None => first_frame_error,
        })
    }

    pub async fn finish(
        mut self,
        page: &Page,
        flow_failed: bool,
        stop_at: Instant,
    ) -> Result<Option<PathBuf>, FlowRecordingFinish> {
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
                    return Err(FlowRecordingFinish::Complete {
                        error: format!("screencast task failed: {error}"),
                        partial: self.partial_path.clone(),
                        recording: None,
                    });
                }
                Err(_) => {
                    self.task.abort();
                    let _ = (&mut self.task).await;
                    return Err(FlowRecordingFinish::Complete {
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
                return Err(FlowRecordingFinish::Complete {
                    error: error.to_string(),
                    partial: self.partial_path.clone(),
                    recording: None,
                });
            }
            Err(_) => {
                return Err(FlowRecordingFinish::Complete {
                    error: "video finalization timed out".to_owned(),
                    partial: self.partial_path.clone(),
                    recording: None,
                });
            }
        };
        if !errors.is_empty() {
            return Err(FlowRecordingFinish::Complete {
                error: errors.join("; "),
                partial: self.partial_path.clone(),
                recording,
            });
        }
        Ok(recording)
    }
}

impl Drop for FlowRecording {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum FlowStartAwait<T> {
    Ready(T),
    Cancelled,
    Deadline,
}

async fn await_flow_start<T>(
    cancellation: Option<&dyn FlowCancellation>,
    deadline: Instant,
    future: impl Future<Output = T>,
) -> FlowStartAwait<T> {
    tokio::select! {
        biased;
        _ = async {
            match cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        } => FlowStartAwait::Cancelled,
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future) => {
            match result {
                Ok(result) => FlowStartAwait::Ready(result),
                Err(_) => FlowStartAwait::Deadline,
            }
        }
    }
}

fn flow_start_cleanup_error(message: &str, cleanup: Option<String>) -> String {
    cleanup.map_or_else(
        || message.to_owned(),
        |cleanup| format!("{message}; cleanup failed: {cleanup}"),
    )
}

pub async fn settle_video(page: &Page) {
    let _ = tokio::time::timeout(
        SECONDARY_TIMEOUT,
        page.evaluate(
            "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
        ),
    )
    .await;
    tokio::time::sleep(FINAL_FRAME_DELAY).await;
}

async fn stop_screencast(page: &Page) -> Option<String> {
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

fn should_retain_video(flow_failed: bool, screencast_errors: &[String]) -> bool {
    flow_failed || !screencast_errors.is_empty()
}

pub fn missing_recording_finish(recording_enabled: bool, warnings: Vec<String>) -> RecordingFinish {
    RecordingFinish {
        status: if recording_enabled { "failed" } else { "off" },
        path: None,
        partial_path: None,
        warnings,
    }
}

pub fn apply_recording_finish(
    finish: Result<Option<PathBuf>, FlowRecordingFinish>,
    artifacts: &mut ArtifactPaths,
    recording_error: &mut Option<String>,
) {
    match finish {
        Ok(Some(path)) => artifacts.recording = Some(path_text(&path)),
        Ok(None) => {}
        Err(FlowRecordingFinish::Complete {
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

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screencast_errors_retain_failure_only_video() {
        assert!(should_retain_video(false, &["stream failed".to_owned()]));
        assert!(should_retain_video(true, &[]));
        assert!(!should_retain_video(false, &[]));
    }

    #[test]
    fn video_off_stays_off_when_unrelated_warnings_exist() {
        let finish = missing_recording_finish(false, vec!["dialog warning".to_owned()]);
        assert_eq!(finish.status, "off");
        assert_eq!(finish.warnings, ["dialog warning"]);
        assert_eq!(missing_recording_finish(true, Vec::new()).status, "failed");
    }
}
