use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::Notify,
    task::JoinHandle,
    time::{self, timeout},
};

pub use crate::flow::VideoMode;

const FRAME_RATE: u32 = 15;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STDERR_BYTES: u64 = 64 * 1024;

impl VideoMode {
    pub fn enabled(self) -> bool {
        self != Self::Off
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::On => "on",
            Self::RetainOnFailure => "retain-on-failure",
        })
    }
}

impl FromStr for VideoMode {
    type Err = VideoError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            "retain-on-failure" => Ok(Self::RetainOnFailure),
            _ => Err(VideoError::InvalidMode(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub mode: VideoMode,
    pub ffmpeg_path: PathBuf,
    pub output_path: PathBuf,
    pub viewport_width: u32,
    pub viewport_height: u32,
}

impl VideoConfig {
    pub fn validate(&self) -> Result<(), VideoError> {
        if !self.mode.enabled() {
            return Ok(());
        }
        if self.viewport_width == 0 || self.viewport_height == 0 {
            return Err(VideoError::InvalidConfig(
                "video viewport dimensions must be greater than zero".into(),
            ));
        }
        if !self.viewport_width.is_multiple_of(2) || !self.viewport_height.is_multiple_of(2) {
            return Err(VideoError::InvalidConfig(
                "video viewport width and height must both be even".into(),
            ));
        }
        if self.ffmpeg_path.as_os_str().is_empty() {
            return Err(VideoError::InvalidConfig(
                "FFmpeg path must not be empty".into(),
            ));
        }
        if self.output_path.file_name().is_none() {
            return Err(VideoError::InvalidConfig(
                "video output path must include a file name".into(),
            ));
        }
        if self.output_path.extension() != Some(OsStr::new("mp4")) {
            return Err(VideoError::InvalidConfig(
                "video output path must end in .mp4".into(),
            ));
        }
        Ok(())
    }

    pub fn partial_path(&self) -> PathBuf {
        self.output_path.with_extension("partial.mp4")
    }
}

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("invalid video mode {0:?}; expected off, on, or retain-on-failure")]
    InvalidMode(String),
    #[error("invalid video configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to start FFmpeg at {path}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("FFmpeg preflight timed out after {0:?}")]
    PreflightTimeout(Duration),
    #[error("FFmpeg preflight failed: {0}")]
    Preflight(String),
    #[error("FFmpeg does not provide the required libx264 encoder")]
    H264Unavailable,
    #[error("timed out waiting {0:?} for the first video frame")]
    FirstFrameTimeout(Duration),
    #[error("video frame writer failed: {0}")]
    FrameWriter(String),
    #[error("FFmpeg finalization timed out after {0:?}; partial recording retained at {1}")]
    FinalizationTimeout(Duration, PathBuf),
    #[error(
        "FFmpeg exited unsuccessfully ({status}): {stderr}; partial recording retained at {partial_path}"
    )]
    FfmpegFailed {
        status: std::process::ExitStatus,
        stderr: String,
        partial_path: PathBuf,
    },
    #[error("video artifact operation failed for {path}: {source}")]
    Artifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Checks that the configured executable starts and exposes the H.264 encoder used by Playrust.
pub async fn preflight_ffmpeg(config: &VideoConfig) -> Result<(), VideoError> {
    config.validate()?;
    if !config.mode.enabled() {
        return Ok(());
    }

    let child = Command::new(&config.ffmpeg_path)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| VideoError::Spawn {
            path: config.ffmpeg_path.clone(),
            source,
        })?;

    let result = match timeout(PROCESS_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result.map_err(|source| VideoError::Preflight(source.to_string()))?,
        Err(_) => return Err(VideoError::PreflightTimeout(PROCESS_TIMEOUT)),
    };
    if !result.status.success() {
        return Err(VideoError::Preflight(output_text(
            &result.stdout,
            &result.stderr,
        )));
    }
    if !result
        .stdout
        .split(|byte| byte.is_ascii_whitespace())
        .any(|word| word == b"libx264")
    {
        return Err(VideoError::H264Unavailable);
    }
    Ok(())
}

struct PendingFrame {
    jpeg: Vec<u8>,
    received_at: Instant,
}

#[derive(Default)]
struct SharedState {
    latest: Option<PendingFrame>,
    first_frame_received: bool,
    stop_at: Option<Instant>,
}

pub struct VideoRecorder {
    mode: VideoMode,
    output_path: PathBuf,
    partial_path: PathBuf,
    child: Child,
    frame_sink: VideoFrameSink,
    writer: JoinHandle<Result<(), VideoError>>,
    stderr: JoinHandle<String>,
}

/// A cloneable input for a recorder that can outlive any one screencast source.
#[derive(Clone)]
pub struct VideoFrameSink {
    shared: Arc<Mutex<SharedState>>,
    writer_notify: Arc<Notify>,
    first_frame_notify: Arc<Notify>,
}

impl VideoRecorder {
    /// Starts FFmpeg. Call `preflight_ffmpeg` once before launching Chromium.
    pub async fn start(config: &VideoConfig) -> Result<Self, VideoError> {
        config.validate()?;
        if !config.mode.enabled() {
            return Err(VideoError::InvalidConfig(
                "cannot start a recorder when video mode is off".into(),
            ));
        }

        let partial_path = config.partial_path();
        let mut child = Command::new(&config.ffmpeg_path)
            .args(ffmpeg_arguments(config, &partial_path))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| VideoError::Spawn {
                path: config.ffmpeg_path.clone(),
                source,
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            VideoError::InvalidConfig("FFmpeg did not expose its piped stdin".into())
        })?;
        let child_stderr = child.stderr.take().ok_or_else(|| {
            VideoError::InvalidConfig("FFmpeg did not expose its piped stderr".into())
        })?;

        let shared = Arc::new(Mutex::new(SharedState::default()));
        let writer_notify = Arc::new(Notify::new());
        let first_frame_notify = Arc::new(Notify::new());
        let writer = tokio::spawn(write_frames(shared.clone(), writer_notify.clone(), stdin));
        let stderr = tokio::spawn(capture_stderr(child_stderr));
        let frame_sink = VideoFrameSink {
            shared,
            writer_notify,
            first_frame_notify,
        };

        Ok(Self {
            mode: config.mode,
            output_path: config.output_path.clone(),
            partial_path,
            child,
            frame_sink,
            writer,
            stderr,
        })
    }

    /// Returns a frame input that can be moved between screencast source tasks.
    pub fn frame_sink(&self) -> VideoFrameSink {
        self.frame_sink.clone()
    }

    /// Replaces any queued frame without blocking the CDP event loop.
    pub fn push_frame_at(&self, jpeg: Vec<u8>, received_at: Instant) {
        self.frame_sink.push_frame_at(jpeg, received_at);
    }

    pub fn push_frame(&self, jpeg: Vec<u8>) {
        self.frame_sink.push_frame(jpeg);
    }

    pub async fn wait_for_first_frame(&self, wait: Duration) -> Result<(), VideoError> {
        self.frame_sink.wait_for_first_frame(wait).await
    }

    /// Stops accepting frames. Repeated calls preserve the first stop boundary.
    pub fn begin_finalization(&self, stop_at: Instant) {
        self.frame_sink.begin_finalization(stop_at);
    }

    /// Flushes the last frame through `stop_at`, then publishes or removes the completed MP4.
    pub async fn finalize(
        mut self,
        stop_at: Instant,
        flow_failed: bool,
    ) -> Result<Option<PathBuf>, VideoError> {
        self.begin_finalization(stop_at);

        let deadline = time::Instant::now() + PROCESS_TIMEOUT;
        let writer_result = match time::timeout_at(deadline, &mut self.writer).await {
            Ok(result) => result
                .map_err(|error| VideoError::FrameWriter(error.to_string()))?
                .map_err(|error| VideoError::FrameWriter(error.to_string())),
            Err(_) => {
                terminate(&mut self.child).await;
                self.writer.abort();
                let _ = (&mut self.writer).await;
                return Err(VideoError::FinalizationTimeout(
                    PROCESS_TIMEOUT,
                    self.partial_path.clone(),
                ));
            }
        };

        let status = match time::timeout_at(deadline, self.child.wait()).await {
            Ok(result) => result.map_err(|source| VideoError::Artifact {
                path: self.partial_path.clone(),
                source,
            })?,
            Err(_) => {
                terminate(&mut self.child).await;
                return Err(VideoError::FinalizationTimeout(
                    PROCESS_TIMEOUT,
                    self.partial_path.clone(),
                ));
            }
        };
        let stderr = (&mut self.stderr).await.unwrap_or_default();
        if !status.success() {
            return Err(VideoError::FfmpegFailed {
                status,
                stderr,
                partial_path: self.partial_path.clone(),
            });
        }
        writer_result?;

        if self.mode == VideoMode::RetainOnFailure && !flow_failed {
            remove_if_exists(&self.output_path).await?;
            fs::remove_file(&self.partial_path)
                .await
                .map_err(|source| VideoError::Artifact {
                    path: self.partial_path.clone(),
                    source,
                })?;
            Ok(None)
        } else {
            publish(&self.partial_path, &self.output_path).await?;
            Ok(Some(self.output_path.clone()))
        }
    }
}

impl VideoFrameSink {
    /// Replaces any queued frame without blocking the source event loop.
    pub fn push_frame_at(&self, jpeg: Vec<u8>, received_at: Instant) {
        let mut shared = self.shared.lock().expect("video frame state poisoned");
        if shared.stop_at.is_some() {
            return;
        }
        shared.latest = Some(PendingFrame { jpeg, received_at });
        shared.first_frame_received = true;
        drop(shared);
        self.writer_notify.notify_one();
        self.first_frame_notify.notify_one();
    }

    pub fn push_frame(&self, jpeg: Vec<u8>) {
        self.push_frame_at(jpeg, Instant::now());
    }

    pub async fn wait_for_first_frame(&self, wait: Duration) -> Result<(), VideoError> {
        let deadline = time::Instant::now() + wait;
        loop {
            let notified = self.first_frame_notify.notified();
            if self
                .shared
                .lock()
                .expect("video frame state poisoned")
                .first_frame_received
            {
                return Ok(());
            }
            if time::timeout_at(deadline, notified).await.is_err() {
                return Err(VideoError::FirstFrameTimeout(wait));
            }
        }
    }

    pub fn begin_finalization(&self, stop_at: Instant) {
        self.shared
            .lock()
            .expect("video frame state poisoned")
            .stop_at
            .get_or_insert(stop_at);
        self.writer_notify.notify_one();
    }
}

impl Drop for VideoRecorder {
    fn drop(&mut self) {
        self.writer.abort();
        self.stderr.abort();
        let _ = self.child.start_kill();
    }
}

async fn terminate(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn remove_if_exists(path: &Path) -> Result<(), VideoError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(VideoError::Artifact {
            path: path.to_owned(),
            source,
        }),
    }
}

async fn publish(partial: &Path, output: &Path) -> Result<(), VideoError> {
    remove_if_exists(output).await?;
    fs::rename(partial, output)
        .await
        .map_err(|source| VideoError::Artifact {
            path: output.to_owned(),
            source,
        })
}

async fn capture_stderr<R: AsyncRead + Unpin>(mut stderr: R) -> String {
    let mut bytes = Vec::new();
    let mut limited = (&mut stderr).take(MAX_STDERR_BYTES);
    let _ = limited.read_to_end(&mut bytes).await;
    drop(limited);
    let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

async fn write_frames<W>(
    shared: Arc<Mutex<SharedState>>,
    notify: Arc<Notify>,
    mut output: W,
) -> Result<(), VideoError>
where
    W: AsyncWrite + Unpin,
{
    let mut pacer = FramePacer::default();
    loop {
        let notified = notify.notified();
        let (frame, stop_at) = {
            let mut shared = shared.lock().expect("video frame state poisoned");
            (shared.latest.take(), shared.stop_at)
        };

        if let Some(frame) =
            frame.filter(|frame| stop_at.is_none_or(|stop| frame.received_at <= stop))
        {
            pacer
                .receive(frame.jpeg, frame.received_at, &mut output)
                .await?;
        }
        if let Some(stop_at) = stop_at {
            pacer.finish(stop_at, &mut output).await?;
            output
                .shutdown()
                .await
                .map_err(|error| VideoError::FrameWriter(error.to_string()))?;
            return Ok(());
        }
        notified.await;
    }
}

#[derive(Default)]
struct FramePacer {
    started_at: Option<Instant>,
    latest_received_at: Option<Instant>,
    latest_jpeg: Vec<u8>,
    latest_emitted: bool,
    emitted: u64,
}

impl FramePacer {
    async fn receive<W: AsyncWrite + Unpin>(
        &mut self,
        jpeg: Vec<u8>,
        received_at: Instant,
        output: &mut W,
    ) -> Result<(), VideoError> {
        if self.started_at.is_none() {
            self.started_at = Some(received_at);
            self.latest_received_at = Some(received_at);
            self.latest_jpeg = jpeg;
            self.latest_emitted = false;
            return self.emit_until(received_at, output).await;
        }
        if self
            .latest_received_at
            .is_some_and(|last| received_at < last)
        {
            return Ok(());
        }
        self.emit_until(received_at, output).await?;
        self.latest_received_at = Some(received_at);
        self.latest_jpeg = jpeg;
        self.latest_emitted = false;
        Ok(())
    }

    async fn finish<W: AsyncWrite + Unpin>(
        &mut self,
        stop_at: Instant,
        output: &mut W,
    ) -> Result<(), VideoError> {
        self.emit_until(stop_at, output).await?;
        if self.started_at.is_some() && !self.latest_emitted {
            output
                .write_all(&self.latest_jpeg)
                .await
                .map_err(|error| VideoError::FrameWriter(error.to_string()))?;
            self.emitted += 1;
            self.latest_emitted = true;
        }
        Ok(())
    }

    async fn emit_until<W: AsyncWrite + Unpin>(
        &mut self,
        through: Instant,
        output: &mut W,
    ) -> Result<(), VideoError> {
        let Some(started_at) = self.started_at else {
            return Ok(());
        };
        let target = frames_due(started_at, through);
        while self.emitted < target {
            output
                .write_all(&self.latest_jpeg)
                .await
                .map_err(|error| VideoError::FrameWriter(error.to_string()))?;
            self.emitted += 1;
            self.latest_emitted = true;
        }
        Ok(())
    }
}

fn frames_due(started_at: Instant, through: Instant) -> u64 {
    let elapsed = through.saturating_duration_since(started_at).as_nanos();
    let frames = elapsed
        .saturating_mul(u128::from(FRAME_RATE))
        .div_ceil(1_000_000_000);
    u64::try_from(frames.max(1)).unwrap_or(u64::MAX)
}

fn ffmpeg_arguments(config: &VideoConfig, partial_path: &Path) -> Vec<OsString> {
    let filter = format!(
        "scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
        config.viewport_width,
        config.viewport_height,
        config.viewport_width,
        config.viewport_height
    );
    [
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "image2pipe".into(),
        "-framerate".into(),
        FRAME_RATE.to_string().into(),
        "-vcodec".into(),
        "mjpeg".into(),
        "-i".into(),
        "pipe:0".into(),
        "-vf".into(),
        filter.into(),
        "-an".into(),
        "-c:v".into(),
        "libx264".into(),
        "-crf".into(),
        "20".into(),
        "-preset".into(),
        "veryfast".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-y".into(),
        partial_path.as_os_str().to_owned(),
    ]
    .into()
}

fn output_text(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    format!("{} {}", stdout.trim(), stderr.trim())
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VideoConfig {
        VideoConfig {
            mode: VideoMode::On,
            ffmpeg_path: "ffmpeg".into(),
            output_path: "recording.mp4".into(),
            viewport_width: 1280,
            viewport_height: 720,
        }
    }

    #[test]
    fn video_mode_parses_and_deserializes() {
        assert_eq!("off".parse::<VideoMode>().unwrap(), VideoMode::Off);
        assert_eq!("on".parse::<VideoMode>().unwrap(), VideoMode::On);
        assert_eq!(
            "retain-on-failure".parse::<VideoMode>().unwrap(),
            VideoMode::RetainOnFailure
        );
        assert_eq!(
            serde_json::from_str::<VideoMode>("\"retain-on-failure\"").unwrap(),
            VideoMode::RetainOnFailure
        );
        assert!("always".parse::<VideoMode>().is_err());
    }

    #[test]
    fn enabled_video_requires_nonzero_even_dimensions() {
        let mut value = config();
        assert!(value.validate().is_ok());
        value.viewport_width = 1279;
        assert!(value.validate().is_err());
        value.viewport_width = 0;
        assert!(value.validate().is_err());
        value.mode = VideoMode::Off;
        assert!(value.validate().is_ok());
    }

    #[test]
    fn pacing_uses_a_cumulative_fifteen_fps_grid() {
        let start = Instant::now();
        assert_eq!(frames_due(start, start), 1);
        assert_eq!(frames_due(start, start + Duration::from_millis(66)), 1);
        assert_eq!(frames_due(start, start + Duration::from_millis(67)), 2);
        assert_eq!(frames_due(start, start + Duration::from_millis(999)), 15);
        assert_eq!(frames_due(start, start + Duration::from_secs(1)), 15);
        assert_eq!(frames_due(start, start + Duration::from_millis(1001)), 16);
    }

    #[tokio::test]
    async fn pacing_repeats_the_previous_frame_until_the_next_receipt() {
        let start = Instant::now();
        let mut output = Vec::new();
        let mut pacer = FramePacer::default();

        pacer.receive(vec![1], start, &mut output).await.unwrap();
        pacer
            .receive(vec![2], start + Duration::from_millis(140), &mut output)
            .await
            .unwrap();
        pacer
            .finish(start + Duration::from_millis(210), &mut output)
            .await
            .unwrap();

        assert_eq!(output, vec![1, 1, 1, 2]);
    }

    #[tokio::test]
    async fn pacing_emits_a_frame_received_at_the_stop_boundary() {
        let start = Instant::now();
        let mut output = Vec::new();
        let mut pacer = FramePacer::default();

        pacer.receive(vec![1], start, &mut output).await.unwrap();
        pacer
            .receive(vec![2], start + Duration::from_millis(10), &mut output)
            .await
            .unwrap();
        pacer
            .finish(start + Duration::from_millis(10), &mut output)
            .await
            .unwrap();

        assert_eq!(output, vec![1, 2]);
    }

    #[tokio::test]
    async fn stderr_capture_keeps_the_prefix_and_drains_the_rest() {
        let input = vec![b'x'; MAX_STDERR_BYTES as usize + 1024];
        let captured = capture_stderr(std::io::Cursor::new(input)).await;

        assert_eq!(captured.len(), MAX_STDERR_BYTES as usize);
    }

    #[tokio::test]
    async fn publication_can_remove_an_existing_windows_destination() {
        let directory = tempfile::tempdir().unwrap();
        let partial = directory.path().join("recording.partial.mp4");
        let output = directory.path().join("recording.mp4");

        tokio::fs::write(&partial, b"new").await.unwrap();
        tokio::fs::write(&output, b"old").await.unwrap();
        publish(&partial, &output).await.unwrap();

        assert_eq!(tokio::fs::read(&output).await.unwrap(), b"new");
        assert!(!partial.exists());
    }

    #[tokio::test]
    async fn failed_publication_retains_the_partial_recording() {
        let directory = tempfile::tempdir().unwrap();
        let partial = directory.path().join("recording.partial.mp4");
        let output = directory.path().join("missing/recording.mp4");

        tokio::fs::write(&partial, b"partial").await.unwrap();

        assert!(publish(&partial, &output).await.is_err());
        assert_eq!(tokio::fs::read(&partial).await.unwrap(), b"partial");
    }

    #[test]
    fn finalization_signal_is_idempotent_and_rejects_late_frames() {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let sink = VideoFrameSink {
            shared: shared.clone(),
            writer_notify: Arc::new(Notify::new()),
            first_frame_notify: Arc::new(Notify::new()),
        };
        let first_stop = Instant::now();

        sink.begin_finalization(first_stop);
        sink.begin_finalization(first_stop + Duration::from_secs(1));
        sink.push_frame_at(vec![1], first_stop);

        let state = shared.lock().unwrap();
        assert_eq!(state.stop_at, Some(first_stop));
        assert!(state.latest.is_none());
        assert!(!state.first_frame_received);
    }

    #[test]
    fn ffmpeg_arguments_pipe_mjpeg_and_publish_compatible_h264() {
        let value = config();
        let partial = value.partial_path();
        let arguments: Vec<_> = ffmpeg_arguments(&value, &partial)
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert_eq!(partial, PathBuf::from("recording.partial.mp4"));
        assert_eq!(
            arguments,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "image2pipe",
                "-framerate",
                "15",
                "-vcodec",
                "mjpeg",
                "-i",
                "pipe:0",
                "-vf",
                "scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
                "-an",
                "-c:v",
                "libx264",
                "-crf",
                "20",
                "-preset",
                "veryfast",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                "-y",
                "recording.partial.mp4",
            ]
        );
    }
}
