//! Persistent browser automation session boundary.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::browser::BrowserHost;
use crate::flow::{CompiledFlow, FrameSwitch, Operation, Redactor, VideoMode};
use crate::report::FlowReport;
use crate::runner::{
    InteractiveStepError, InteractiveStepResult, RunOptions, SessionRecorder,
    SessionRecordingFinish, SessionRuntime, SessionSettings,
};
use crate::session_dialog::{DialogPolicy, NativeDialogState, PendingDialog};
use crate::session_snapshot::{
    DiffError, ElementRef, ReferenceError, SessionSnapshot, SnapshotDiff, SnapshotStore,
};
use crate::video::VideoConfig;

pub use crate::runner::{SessionInspection, SessionPage};

pub struct BrowserSessionOptions {
    pub settings: SessionSettings,
    pub video: VideoMode,
    pub ffmpeg_path: PathBuf,
    pub artifacts: PathBuf,
    pub dialog_policy: DialogPolicy,
}

#[derive(Debug, Serialize)]
pub struct BrowserSessionClose {
    pub recording: SessionRecordingFinish,
    pub warnings: Vec<String>,
}

/// Owns all state whose lifetime is the interactive session rather than one submission.
pub struct BrowserSession {
    runtime: SessionRuntime,
    settings: SessionSettings,
    dialogs: NativeDialogState,
    snapshots: SnapshotStore,
    recorder: Option<SessionRecorder>,
    recording_enabled: bool,
    recording_warnings: Vec<String>,
}

impl BrowserSession {
    pub async fn open(host: &BrowserHost, options: BrowserSessionOptions) -> anyhow::Result<Self> {
        let runtime =
            SessionRuntime::open_settings(host, options.settings, Redactor::default()).await?;
        let mut dialogs = NativeDialogState::new(options.dialog_policy);
        dialogs.bind(runtime.page()).await?;
        let mut recording_warnings = Vec::new();
        let recording_enabled = options.video.enabled();
        let recorder = if recording_enabled {
            let viewport = runtime.viewport();
            let config = VideoConfig {
                mode: VideoMode::On,
                ffmpeg_path: options.ffmpeg_path,
                output_path: options.artifacts.join("recording.mp4"),
                viewport_width: viewport.width,
                viewport_height: viewport.height,
            };
            match SessionRecorder::start(config, runtime.page()).await {
                Ok(recorder) => Some(recorder),
                Err(error) => {
                    recording_warnings.push(error);
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            runtime,
            settings: options.settings,
            dialogs,
            snapshots: SnapshotStore::default(),
            recorder,
            recording_enabled,
            recording_warnings,
        })
    }

    pub fn settings(&self) -> SessionSettings {
        self.settings
    }

    pub fn settings_match(&self, flow: &CompiledFlow) -> bool {
        self.runtime.settings_match(flow)
    }

    pub fn output(&self, name: &str) -> Option<&Value> {
        self.runtime.output(name)
    }

    pub fn output_names(&self) -> BTreeSet<String> {
        self.runtime.output_names()
    }

    pub fn pending_dialog(&self) -> Option<PendingDialog> {
        self.dialogs.pending().map(|mut dialog| {
            dialog.message = self.runtime.redact(&dialog.message);
            dialog.default_prompt = dialog
                .default_prompt
                .map(|value| self.runtime.redact(&value));
            dialog
        })
    }

    pub fn recording_warnings(&mut self) -> Vec<String> {
        if let Some(error) = self.dialogs.take_error() {
            self.recording_warnings
                .push(format!("automatic dialog response failed: {error}"));
        }
        self.recording_warnings.clone()
    }

    pub async fn accept_dialog(&self, text: Option<&str>) -> anyhow::Result<PendingDialog> {
        let pending = self
            .pending_dialog()
            .ok_or_else(|| anyhow::anyhow!("no native dialog is pending"))?;
        self.dialogs.accept(text).await?;
        Ok(pending)
    }

    pub async fn dismiss_dialog(&self) -> anyhow::Result<PendingDialog> {
        let pending = self
            .pending_dialog()
            .ok_or_else(|| anyhow::anyhow!("no native dialog is pending"))?;
        self.dialogs.dismiss().await?;
        Ok(pending)
    }

    pub async fn inspect(
        &self,
        host: &BrowserHost,
        accessibility: bool,
        screenshot_directory: Option<&Path>,
    ) -> anyhow::Result<SessionInspection> {
        let mut inspection = self
            .runtime
            .inspect(host, accessibility, screenshot_directory)
            .await?;
        inspection.url = self.runtime.redact(&inspection.url);
        inspection.title = self.runtime.redact(&inspection.title);
        inspection.active_frame = inspection
            .active_frame
            .map(|value| self.runtime.redact(&value));
        for page in &mut inspection.pages {
            page.url = self.runtime.redact(&page.url);
            page.title = self.runtime.redact(&page.title);
        }
        if let Some(accessibility) = &mut inspection.accessibility {
            redact_json(accessibility, &self.runtime);
        }
        Ok(inspection)
    }

    pub async fn snapshot(&mut self) -> anyhow::Result<SessionSnapshot> {
        let capture = self.runtime.capture_agent_snapshot().await?;
        Ok(self.snapshots.publish(capture)?)
    }

    pub async fn snapshot_screenshot(
        &self,
        directory: &Path,
        file_name: &str,
        full_page: bool,
    ) -> anyhow::Result<PathBuf> {
        self.runtime
            .capture_agent_screenshot(directory, file_name, full_page)
            .await
    }

    pub fn snapshot_diff(&self, from: u64, to: u64) -> Result<SnapshotDiff, DiffError> {
        self.snapshots.diff(from, to)
    }

    pub fn resolve_ref(&self, reference: ElementRef) -> Result<(Value, i64), ReferenceError> {
        let (identity, backend_node_id) = self.snapshots.resolve(reference)?;
        let locator =
            serde_json::from_str(&identity.0).map_err(|_| ReferenceError::Stale { reference })?;
        Ok((locator, backend_node_id))
    }

    pub fn snapshot_revision(&self) -> u64 {
        self.snapshots.generation()
    }

    pub fn validate_snapshot_baseline(&self, from: u64) -> Result<(), DiffError> {
        self.snapshots.validate_diff_from(from)
    }

    pub async fn scroll_position(&self) -> anyhow::Result<crate::session_snapshot::Scroll> {
        self.runtime.scroll_position().await
    }

    pub async fn current_url(&self) -> anyhow::Result<String> {
        self.runtime
            .current_url_title()
            .await
            .map(|(url, _)| self.runtime.redact(&url))
    }

    pub async fn reference_matches(
        &self,
        flow: &CompiledFlow,
        backend_node_id: i64,
    ) -> anyhow::Result<bool> {
        self.runtime.reference_matches(flow, backend_node_id).await
    }

    pub async fn execute_interactive(
        &mut self,
        host: &BrowserHost,
        flow: &CompiledFlow,
        artifacts: &Path,
    ) -> Result<InteractiveStepResult, InteractiveStepError> {
        let page_before = self.runtime.page().target_id().clone();
        if interactive_mutates(flow) {
            self.snapshots.invalidate_for_mutation();
        }
        let mut result = self
            .runtime
            .execute_interactive(host, flow, artifacts, self.dialogs.wait_for_pending())
            .await;
        if result.is_ok() {
            if self.runtime.page().target_id() != &page_before {
                if let Err(error) = self.dialogs.bind(self.runtime.page()).await {
                    self.recording_warnings
                        .push(format!("rebind dialog listener: {error}"));
                }
                if let Some(recorder) = &mut self.recorder
                    && let Err(error) = recorder.bind(self.runtime.page()).await
                {
                    self.recording_warnings.push(error);
                }
            }
            if let Ok(step_result) = &mut result {
                let metadata = tokio::time::timeout(Duration::from_secs(2), async {
                    tokio::select! {
                        result = self.runtime.current_url_title() => Some(result),
                        _ = self.dialogs.wait_for_pending() => None,
                    }
                })
                .await;
                match metadata {
                    Ok(Some(Ok((url, title)))) => {
                        step_result.url = url;
                        step_result.title = title;
                    }
                    Ok(Some(Err(error))) => self
                        .recording_warnings
                        .push(format!("read action page metadata: {error}")),
                    Ok(None) => {}
                    Err(_) if self.pending_dialog().is_none() => self
                        .recording_warnings
                        .push("read action page metadata timed out".to_owned()),
                    Err(_) => {}
                }
            }
        }
        result
    }

    pub async fn execute(
        &mut self,
        host: &BrowserHost,
        flow: &CompiledFlow,
        options: &RunOptions,
    ) -> anyhow::Result<FlowReport> {
        anyhow::ensure!(
            self.settings_match(flow),
            "viewport and geolocation must match the session settings"
        );
        self.snapshots.invalidate_for_mutation();
        let page_before = self.runtime.page().target_id().clone();
        let report = self.runtime.execute(host, flow, options).await;
        if self.runtime.page().target_id() != &page_before {
            if let Err(error) = self.dialogs.bind(self.runtime.page()).await {
                self.recording_warnings
                    .push(format!("rebind dialog listener: {error}"));
            }
            if let Some(recorder) = &mut self.recorder
                && let Err(error) = recorder.bind(self.runtime.page()).await
            {
                self.recording_warnings.push(error);
            }
        }
        Ok(report)
    }

    pub async fn close(mut self, host: &BrowserHost) -> anyhow::Result<BrowserSessionClose> {
        if self.pending_dialog().is_some() {
            let _ = self.dialogs.dismiss().await;
        }
        self.dialogs.shutdown().await;
        if let Some(error) = self.dialogs.take_error() {
            self.recording_warnings
                .push(format!("automatic dialog response failed: {error}"));
        }
        let recording = match self.recorder.take() {
            Some(recorder) => recorder.finalize().await,
            None => {
                missing_recording_finish(self.recording_enabled, self.recording_warnings.clone())
            }
        };
        self.runtime.close(host).await?;
        Ok(BrowserSessionClose {
            warnings: self.recording_warnings,
            recording,
        })
    }
}

fn missing_recording_finish(
    recording_enabled: bool,
    warnings: Vec<String>,
) -> SessionRecordingFinish {
    SessionRecordingFinish {
        status: if recording_enabled { "failed" } else { "off" },
        path: None,
        partial_path: None,
        warnings,
    }
}

fn interactive_mutates(flow: &CompiledFlow) -> bool {
    matches!(
        flow.steps[0].operation,
        Operation::Open { .. }
            | Operation::Click { .. }
            | Operation::DoubleClick { .. }
            | Operation::Fill { .. }
            | Operation::Erase { .. }
            | Operation::Select { .. }
            | Operation::Press { .. }
            | Operation::Back
            | Operation::SwitchPage(_)
            | Operation::SwitchFrame(
                FrameSwitch::Target(_) | FrameSwitch::Main | FrameSwitch::Parent
            )
            | Operation::Scroll { .. }
    )
}

fn redact_json(value: &mut Value, runtime: &SessionRuntime) {
    match value {
        Value::String(text) => *text = runtime.redact(text),
        Value::Array(values) => {
            for value in values {
                redact_json(value, runtime);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_json(value, runtime);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_off_stays_off_when_unrelated_warnings_exist() {
        let finish = missing_recording_finish(false, vec!["dialog warning".to_owned()]);
        assert_eq!(finish.status, "off");
        assert_eq!(finish.warnings, ["dialog warning"]);
        assert_eq!(missing_recording_finish(true, Vec::new()).status, "failed");
    }
}
