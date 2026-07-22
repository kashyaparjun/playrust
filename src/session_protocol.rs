use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::browser::BrowserHost;
use crate::browser_session::{BrowserSession, BrowserSessionClose, BrowserSessionOptions};
use crate::flow::{CompiledFlow, VideoMode, compile_inline_yaml, compile_inline_yaml_with_video};
use crate::report::{
    AggregateReport, ChromiumInfo, ExitCode, FlowReport, FlowStatus, RunnerInfo,
    write_aggregate_report,
};
use crate::runner::{CancellationToken, RunOptions, SessionSettings};
use crate::session_dialog::DialogPolicy;
use crate::session_journal::{
    ActionOutcome, JournalEvent, JournalWriter, build_replay_yaml, is_safe_bundle_name,
    publish_replay_atomic,
};
use crate::session_snapshot::{ElementRef, ReferenceError};

/// Maximum bytes before the newline in one NDJSON command envelope.
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct SessionOptions {
    pub browser: PathBuf,
    pub headed: bool,
    pub artifacts: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
    pub settings: SessionSettings,
    pub video: VideoMode,
    pub dialog_policy: DialogPolicy,
}

#[derive(Debug, Deserialize)]
struct Request {
    id: Value,
    #[serde(flatten)]
    command: SessionCommand,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SessionCommand {
    Submit {
        flow: String,
        #[serde(default)]
        variables: BTreeMap<String, String>,
    },
    Inspect {
        #[serde(default)]
        accessibility: bool,
        #[serde(default)]
        screenshot: bool,
    },
    Snapshot {
        #[serde(default)]
        screenshot: SnapshotScreenshot,
        #[serde(default = "default_true")]
        accessibility: bool,
        since: Option<u64>,
    },
    Act {
        action: Value,
    },
    Scroll {
        #[serde(default)]
        x: i64,
        y: i64,
    },
    Dialog {
        action: DialogAction,
        text: Option<String>,
    },
    Export {
        name: String,
    },
    Output {
        name: String,
    },
    Cancel,
    Close,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotScreenshot {
    #[default]
    None,
    Viewport,
    FullPage,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DialogAction {
    Accept,
    Dismiss,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct Response {
    id: Value,
    ok: bool,
    session_id: String,
    revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ProtocolError>,
}

#[derive(Debug, Serialize)]
struct ProtocolError {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

struct ProtocolState {
    id: String,
    revision: u64,
    submissions: u64,
    inspections: u64,
    started: Instant,
    reports: Vec<FlowReport>,
    artifacts: PathBuf,
    snapshot_count: u64,
    journal_path: PathBuf,
    journal: JournalWriter,
    events: Vec<JournalEvent>,
    bundle: Option<PathBuf>,
    artifact_error: Option<String>,
}

impl ProtocolState {
    fn response(&mut self, id: Value, result: Value) -> Response {
        self.revision += 1;
        Response {
            id,
            ok: true,
            session_id: self.id.clone(),
            revision: self.revision,
            result: Some(result),
            error: None,
        }
    }

    fn error(
        &mut self,
        id: Value,
        code: &'static str,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Response {
        self.revision += 1;
        Response {
            id,
            ok: false,
            session_id: self.id.clone(),
            revision: self.revision,
            result: None,
            error: Some(ProtocolError {
                code,
                message: message.into(),
                details,
            }),
        }
    }

    fn write_report(&self, chromium: &ChromiumInfo) -> anyhow::Result<()> {
        if let Some(error) = &self.artifact_error {
            anyhow::bail!("initialize session artifacts: {error}");
        }
        let report = AggregateReport::new(
            RunnerInfo {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            1,
            Some(chromium.clone()),
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            self.reports.clone(),
        );
        write_aggregate_report(&self.artifacts, &report)?;
        Ok(())
    }

    fn append(&mut self, event: JournalEvent) -> anyhow::Result<()> {
        self.journal.append(&event)?;
        self.events.push(event);
        Ok(())
    }
}

pub async fn run(options: SessionOptions) -> ExitCode {
    let host = match BrowserHost::launch_with_window(
        &options.browser,
        options.headed,
        Some((
            // Chromium's outer window includes browser/UI insets even in headless mode.
            // Leave enough surface for the requested page viewport; screencast max
            // dimensions crop the excess back to the exact configured dimensions.
            options.settings.viewport.width.saturating_add(300),
            options.settings.viewport.height.saturating_add(200),
        )),
    )
    .await
    {
        Ok(host) => host,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::Infrastructure;
        }
    };
    let chromium = ChromiumInfo {
        version: host.version().product.clone(),
        executable: options.browser.to_string_lossy().into_owned(),
    };
    let session_id = session_id();
    let session_artifacts = options.artifacts.join(format!("session-{session_id}"));
    let intended_journal = session_artifacts.join("session.ndjson");
    let artifact_setup = fs::create_dir_all(&session_artifacts)
        .and_then(|()| JournalWriter::open(&intended_journal));
    let (journal_path, journal, artifact_error) = match artifact_setup {
        Ok(journal) => (intended_journal, journal, None),
        Err(error) => {
            // Keep the protocol alive long enough to drain active work and return
            // the artifact failure through the command that owns it.
            let fallback =
                std::env::temp_dir().join(format!("playrust-session-{session_id}.ndjson"));
            let journal = match JournalWriter::open(&fallback) {
                Ok(journal) => journal,
                Err(fallback_error) => {
                    eprintln!(
                        "error: initialize session journal: {error}; fallback failed: {fallback_error}"
                    );
                    let _ = host.shutdown().await;
                    return ExitCode::Infrastructure;
                }
            };
            (fallback, journal, Some(error.to_string()))
        }
    };
    let mut state = ProtocolState {
        id: session_id,
        revision: 0,
        submissions: 0,
        inspections: 0,
        started: Instant::now(),
        reports: Vec::new(),
        artifacts: session_artifacts.clone(),
        snapshot_count: 0,
        journal_path,
        journal,
        events: Vec::new(),
        bundle: None,
        artifact_error,
    };
    if let Err(error) = state.append(JournalEvent::Settings {
        settings: json!({
            "viewport": { "width": options.settings.viewport.width, "height": options.settings.viewport.height },
            "timeout_ms": options.settings.timeout.as_millis(),
            "video": options.video.to_string(),
            "dialog_policy": options.dialog_policy,
        }),
    }) {
        eprintln!("error: write session journal: {error}");
        let _ = host.shutdown().await;
        return ExitCode::Infrastructure;
    }
    let mut session = match BrowserSession::open(
        &host,
        BrowserSessionOptions {
            settings: options.settings,
            video: options.video,
            ffmpeg_path: options
                .ffmpeg_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("ffmpeg")),
            artifacts: session_artifacts,
            dialog_policy: options.dialog_policy,
        },
    )
    .await
    {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!("error: open browser session: {error}");
            let _ = host.shutdown().await;
            return ExitCode::Infrastructure;
        }
    };
    if let Some(opened) = session.as_mut() {
        for warning in opened.recording_warnings() {
            if let Err(error) = state.append(JournalEvent::RecorderWarning { warning }) {
                eprintln!("error: write recorder warning: {error}");
                let _ = session.take().expect("eager session").close(&host).await;
                let _ = host.shutdown().await;
                return ExitCode::Infrastructure;
            }
        }
    }
    let stdin = tokio::io::stdin();
    let mut input = EnvelopeReader::new(BufReader::new(stdin));
    let mut stdout = tokio::io::stdout();
    let mut close_session = false;
    let mut exit_code = ExitCode::Success;

    while !close_session {
        let line = match input.read_envelope().await {
            Ok(Envelope::Line(line)) => line,
            Ok(Envelope::TooLarge) => {
                let response = state.error(
                    Value::Null,
                    "envelope_too_large",
                    format!("command envelope exceeds {MAX_ENVELOPE_BYTES} bytes"),
                    Some(json!({ "max_bytes": MAX_ENVELOPE_BYTES })),
                );
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                continue;
            }
            Ok(Envelope::Eof) => break,
            Err(error) => {
                eprintln!("error: read session command: {error}");
                exit_code = ExitCode::Infrastructure;
                break;
            }
        };
        let request = match decode_request(&line) {
            Ok(request) => request,
            Err((id, message)) => {
                let response = state.error(id, "invalid_command", message, None);
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                continue;
            }
        };

        match request.command {
            SessionCommand::Submit { flow, variables } => {
                let outputs = session
                    .as_ref()
                    .map(BrowserSession::output_names)
                    .unwrap_or_default();
                if let Some(pending) = session.as_ref().and_then(BrowserSession::pending_dialog) {
                    let response = state.error(
                        request.id,
                        "dialog_pending",
                        "handle the pending native dialog before submitting",
                        Some(json!(pending)),
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                let mut compiled = match compile_inline_yaml_with_video(
                    &flow,
                    format!("submission-{:06}.yaml", state.submissions + 1),
                    &variables,
                    &outputs,
                    Some(VideoMode::Off),
                ) {
                    Ok(flow) => flow,
                    Err(error) => {
                        let response =
                            state.error(request.id, "validation", error.to_string(), None);
                        if write_response(&mut stdout, &response).await.is_err() {
                            exit_code = ExitCode::Infrastructure;
                            break;
                        }
                        continue;
                    }
                };
                if let Some(existing) = &session
                    && !existing.settings_match(&compiled)
                {
                    let response = state.error(
                        request.id,
                        "settings_conflict",
                        "viewport and geolocation must match the first submission",
                        None,
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                compiled.settings.video = VideoMode::Off;
                register_flow_secrets(&mut state.journal, &compiled);

                state.submissions += 1;
                let directory = state
                    .artifacts
                    .join(format!("submission-{:06}", state.submissions));
                let cancellation = CancellationToken::new();
                let mut run_options =
                    RunOptions::new(directory).with_cancellation(cancellation.clone());
                if compiled.settings.video != VideoMode::Off {
                    run_options = run_options.with_ffmpeg(
                        options
                            .ffmpeg_path
                            .clone()
                            .unwrap_or_else(|| PathBuf::from("ffmpeg")),
                    );
                }
                let mut execution = Box::pin(session.as_mut().expect("session opened").execute(
                    &host,
                    &compiled,
                    &run_options,
                ));
                let mut requested_close = None;
                let mut input_closed = false;
                let mut cancellation_requested = false;
                let report = loop {
                    tokio::select! {
                        report = &mut execution => break report,
                        command = input.read_envelope(), if requested_close.is_none() && !input_closed => {
                            let response = match command {
                                Ok(Envelope::Line(line)) => match decode_request(&line) {
                                    Ok(Request { id, command: SessionCommand::Cancel }) => {
                                        cancellation.cancel();
                                        cancellation_requested = true;
                                        Some(state.response(id, json!({ "cancelling": true })))
                                    }
                                    Ok(Request { id, command: SessionCommand::Close }) => {
                                        cancellation.cancel();
                                        cancellation_requested = true;
                                        requested_close = Some(id);
                                        None
                                    }
                                    Ok(Request { id, .. }) => Some(state.error(
                                        id,
                                        "busy",
                                        "one mutating submission is already active",
                                        None,
                                    )),
                                    Err((id, message)) => Some(state.error(
                                        id,
                                        "invalid_command",
                                        message,
                                        None,
                                    )),
                                },
                                Ok(Envelope::TooLarge) => Some(state.error(
                                    Value::Null,
                                    "envelope_too_large",
                                    format!("command envelope exceeds {MAX_ENVELOPE_BYTES} bytes"),
                                    Some(json!({ "max_bytes": MAX_ENVELOPE_BYTES })),
                                )),
                                Ok(Envelope::Eof) => {
                                    cancellation.cancel();
                                    cancellation_requested = true;
                                    input_closed = true;
                                    None
                                }
                                Err(error) => {
                                    eprintln!("error: read session command: {error}");
                                    cancellation.cancel();
                                    cancellation_requested = true;
                                    exit_code = ExitCode::Infrastructure;
                                    input_closed = true;
                                    None
                                }
                            };
                            if let Some(response) = response
                                && write_response(&mut stdout, &response).await.is_err()
                            {
                                cancellation.cancel();
                                cancellation_requested = true;
                                exit_code = ExitCode::Infrastructure;
                            }
                        }
                    }
                };
                drop(execution);
                let mut report = match report {
                    Ok(report) => report,
                    Err(error) => {
                        let response =
                            state.error(request.id, "settings_conflict", error.to_string(), None);
                        if write_response(&mut stdout, &response).await.is_err() {
                            exit_code = ExitCode::Infrastructure;
                            close_session = true;
                        }
                        continue;
                    }
                };
                let fatal = report.failures.iter().any(|failure| {
                    matches!(
                        failure.category,
                        crate::report::FailureCategory::BrowserLaunch
                            | crate::report::FailureCategory::BrowserCrash
                            | crate::report::FailureCategory::Protocol
                            | crate::report::FailureCategory::Recording
                    )
                });
                if cancellation_requested && !fatal {
                    report.status = FlowStatus::Interrupted;
                }
                state.reports.push(report.clone());
                if let Err(error) = state.append(JournalEvent::Submission {
                    outcome: if report.status == FlowStatus::Passed {
                        ActionOutcome::Success
                    } else {
                        ActionOutcome::Failed {
                            error: "submission did not pass".to_owned(),
                        }
                    },
                }) {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    close_session = true;
                    continue;
                }
                if let Err(error) = state.write_report(&chromium) {
                    let response = state.error(
                        request.id,
                        "artifacts",
                        error.to_string(),
                        Some(json!(report)),
                    );
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    if let Some(close_id) = requested_close {
                        let response = match session.take() {
                            Some(opened) => match opened.close(&host).await {
                                Ok(result) => state.response(close_id, json!({ "closed": true, "recording": result.recording, "warnings": result.warnings })),
                                Err(close_error) => {
                                    state.error(close_id, "browser", close_error.to_string(), None)
                                }
                            },
                            None => state.response(close_id, json!({ "closed": true })),
                        };
                        let _ = write_response(&mut stdout, &response).await;
                    }
                    close_session = true;
                    continue;
                }
                let response = if report.status == FlowStatus::Passed {
                    state.response(request.id, json!(report))
                } else {
                    state.error(
                        request.id,
                        if fatal {
                            "browser"
                        } else if report.status == FlowStatus::Interrupted {
                            "cancelled"
                        } else {
                            "submission_failed"
                        },
                        "submission did not pass",
                        Some(json!(report)),
                    )
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    close_session = true;
                }
                if fatal {
                    exit_code = ExitCode::Infrastructure;
                } else if report.status == FlowStatus::Interrupted
                    && exit_code != ExitCode::Infrastructure
                {
                    exit_code = ExitCode::Interrupted;
                }
                if let Some(close_id) = requested_close {
                    let response = match session.take() {
                        Some(opened) => match opened.close(&host).await {
                            Ok(mut result) => match publish_close_artifacts(
                                &mut state,
                                &chromium,
                                &mut result,
                            ) {
                                Ok(()) => state.response(close_id, json!({ "closed": true, "bundle": state.bundle, "recording": result.recording, "warnings": result.warnings })),
                                Err(error) => {
                                    exit_code = ExitCode::Infrastructure;
                                    state.error(close_id, "artifacts", error.to_string(), None)
                                }
                            },
                            Err(error) => {
                                exit_code = ExitCode::Infrastructure;
                                state.error(close_id, "browser", error.to_string(), None)
                            }
                        },
                        None => state.response(close_id, json!({ "closed": true })),
                    };
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                    }
                    close_session = true;
                } else {
                    close_session |= fatal || report.status == FlowStatus::Interrupted;
                }
            }
            SessionCommand::Inspect {
                accessibility,
                screenshot,
            } => {
                let Some(opened) = &session else {
                    let response = state.error(
                        request.id,
                        "not_started",
                        "submit a valid flow before inspecting the session",
                        None,
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                };
                state.inspections += 1;
                let directory = screenshot.then(|| {
                    state
                        .artifacts
                        .join(format!("inspection-{:06}", state.inspections))
                });
                match opened
                    .inspect(&host, accessibility, directory.as_deref())
                    .await
                {
                    Ok(inspection) => {
                        let response = state.response(request.id, json!(inspection));
                        if write_response(&mut stdout, &response).await.is_err() {
                            exit_code = ExitCode::Infrastructure;
                            break;
                        }
                    }
                    Err(error) => {
                        let response = state.error(request.id, "browser", error.to_string(), None);
                        let _ = write_response(&mut stdout, &response).await;
                        exit_code = ExitCode::Infrastructure;
                        close_session = true;
                    }
                }
            }
            SessionCommand::Snapshot {
                screenshot,
                accessibility,
                since,
            } => {
                let opened = session.as_mut().expect("eager session");
                if let Some(dialog) = opened.pending_dialog() {
                    let response = state.response(
                        request.id,
                        json!({
                            "snapshot_revision": opened.snapshot_revision(),
                            "pending_dialog": dialog,
                            "capture_status": "blocked_by_dialog",
                        }),
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                if let Some(from) = since
                    && let Err(error) = opened.validate_snapshot_baseline(from)
                {
                    let response = state.error(
                        request.id,
                        "snapshot_unavailable",
                        error.to_string(),
                        Some(json!({ "latest_snapshot_revision": opened.snapshot_revision() })),
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                let inspection = match opened.inspect(&host, false, None).await {
                    Ok(inspection) => inspection,
                    Err(error) => {
                        let response = state.error(request.id, "browser", error.to_string(), None);
                        let _ = write_response(&mut stdout, &response).await;
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                };
                let snapshot = match opened.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        let response = state.error(request.id, "browser", error.to_string(), None);
                        let _ = write_response(&mut stdout, &response).await;
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                };
                state.snapshot_count += 1;
                let screenshot_path = match screenshot {
                    SnapshotScreenshot::None => None,
                    SnapshotScreenshot::Viewport | SnapshotScreenshot::FullPage => {
                        let file_name = format!("snapshot-{:06}.png", state.snapshot_count);
                        match opened
                            .snapshot_screenshot(
                                &state.artifacts,
                                &file_name,
                                matches!(screenshot, SnapshotScreenshot::FullPage),
                            )
                            .await
                        {
                            Ok(path) => Some(path.to_string_lossy().into_owned()),
                            Err(error) => {
                                let response =
                                    state.error(request.id, "artifacts", error.to_string(), None);
                                let _ = write_response(&mut stdout, &response).await;
                                exit_code = ExitCode::Infrastructure;
                                break;
                            }
                        }
                    }
                };
                let diff = since.map(|from| {
                    opened
                        .snapshot_diff(from, snapshot.generation)
                        .expect("validated baseline is adjacent to the new snapshot")
                });
                let event = JournalEvent::Snapshot {
                    snapshot_revision: snapshot.generation,
                    summary: json!({ "url": inspection.url, "elements": snapshot.elements.len() }),
                };
                if let Err(error) = state.append(event) {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                let response = state.response(
                    request.id,
                    json!({
                        "snapshot_revision": snapshot.generation,
                        "url": inspection.url,
                        "title": inspection.title,
                        "pages": inspection.pages,
                        "active_frame": inspection.active_frame,
                        "viewport": snapshot.viewport,
                        "scroll": snapshot.scroll,
                        "pending_dialog": Value::Null,
                        "capture_status": "complete",
                        "screenshot": screenshot_path,
                        "tree": if accessibility { json!(snapshot.elements) } else { Value::Null },
                        "elements": snapshot.elements,
                        "truncation": { "truncated": snapshot.truncated },
                        "diff": diff,
                    }),
                );
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Act { action } => {
                let opened = session.as_mut().expect("eager session");
                if let Some(dialog) = opened.pending_dialog() {
                    let response = state.error(
                        request.id,
                        "dialog_pending",
                        "handle the pending native dialog before acting",
                        Some(json!(dialog)),
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                let (step, durable_locator, action_name, backend_node_id) =
                    match prepare_action(&action, opened) {
                        Ok(value) => value,
                        Err(CommandError {
                            code,
                            message,
                            details,
                        }) => {
                            let response = state.error(request.id, code, message, details);
                            if write_response(&mut stdout, &response).await.is_err() {
                                exit_code = ExitCode::Infrastructure;
                                break;
                            }
                            continue;
                        }
                    };
                let flow = match compile_interactive(&step, opened) {
                    Ok(flow) => flow,
                    Err(error) => {
                        let response = state.error(request.id, "validation", error, None);
                        if write_response(&mut stdout, &response).await.is_err() {
                            exit_code = ExitCode::Infrastructure;
                            break;
                        }
                        continue;
                    }
                };
                register_flow_secrets(&mut state.journal, &flow);
                if let Some(backend_node_id) = backend_node_id {
                    match opened.reference_matches(&flow, backend_node_id).await {
                        Ok(true) => {}
                        Ok(false) => {
                            let response = state.error(
                                request.id,
                                "stale_reference",
                                "the referenced element detached or no longer uniquely matches",
                                Some(json!({ "latest_snapshot_revision": opened.snapshot_revision() })),
                            );
                            if write_response(&mut stdout, &response).await.is_err() {
                                exit_code = ExitCode::Infrastructure;
                                break;
                            }
                            continue;
                        }
                        Err(error) => {
                            let response =
                                state.error(request.id, "action_failed", error.to_string(), None);
                            if write_response(&mut stdout, &response).await.is_err() {
                                exit_code = ExitCode::Infrastructure;
                                break;
                            }
                            continue;
                        }
                    }
                }
                let started = Instant::now();
                let url_before = opened.current_url().await.ok();
                let result = opened
                    .execute_interactive(&host, &flow, &state.artifacts)
                    .await;
                let event = JournalEvent::ReplayStep {
                    step: step.clone(),
                    outcome: match &result {
                        Ok(_) => ActionOutcome::Success,
                        Err(error) => ActionOutcome::Failed {
                            error: error.message.clone(),
                        },
                    },
                    durable_locator: durable_locator.clone(),
                };
                if let Err(error) = state.append(event) {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                if let Ok(result) = &result
                    && !result.url.is_empty()
                    && url_before.as_deref() != Some(result.url.as_str())
                    && let Err(error) = state.append(JournalEvent::ObservedNavigation {
                        url: result.url.clone(),
                    })
                {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                let response = match result {
                    Ok(result) => state.response(
                        request.id,
                        json!({
                            "action": action_name,
                            "locator": durable_locator,
                            "url": result.url,
                            "title": result.title,
                            "pending_dialog": opened.pending_dialog(),
                            "outputs": result.outputs,
                            "artifacts": result.artifact.into_iter().collect::<Vec<_>>(),
                            "elapsed_ms": duration_ms(started.elapsed()),
                        }),
                    ),
                    Err(error) => state.error(
                        request.id,
                        "action_failed",
                        error.message,
                        Some(json!({
                            "category": format!("{:?}", error.category).to_lowercase(),
                            "last_observed": error.last_observed,
                        })),
                    ),
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Scroll { x, y } => {
                if x == 0 && y == 0 {
                    let response = state.error(
                        request.id,
                        "validation",
                        "scroll requires a non-zero x or y delta",
                        None,
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                let opened = session.as_mut().expect("eager session");
                if let Some(dialog) = opened.pending_dialog() {
                    let response = state.error(
                        request.id,
                        "dialog_pending",
                        "handle the pending native dialog before scrolling",
                        Some(json!(dialog)),
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                let step = json!({ "scroll": { "x": x, "y": y } });
                let flow = compile_interactive(&step, opened).expect("validated scroll compiles");
                let result = opened
                    .execute_interactive(&host, &flow, &state.artifacts)
                    .await;
                let event = JournalEvent::ReplayStep {
                    step: step.clone(),
                    outcome: match &result {
                        Ok(_) => ActionOutcome::Success,
                        Err(error) => ActionOutcome::Failed {
                            error: error.message.clone(),
                        },
                    },
                    durable_locator: None,
                };
                if let Err(error) = state.append(event) {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                let response = match result {
                    Ok(_) => match opened.scroll_position().await {
                        Ok(scroll) => state.response(request.id, json!({ "scroll": scroll })),
                        Err(error) => state.error(request.id, "browser", error.to_string(), None),
                    },
                    Err(error) => state.error(request.id, "action_failed", error.message, None),
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Dialog { action, text } => {
                let opened = session.as_ref().expect("eager session");
                let invalid_text = text.is_some() && !matches!(action, DialogAction::Accept);
                if invalid_text {
                    let response = state.error(
                        request.id,
                        "validation",
                        "dialog text is only valid with action accept for a prompt",
                        None,
                    );
                    if write_response(&mut stdout, &response).await.is_err() {
                        exit_code = ExitCode::Infrastructure;
                        break;
                    }
                    continue;
                }
                let result = match action {
                    DialogAction::Accept => opened.accept_dialog(text.as_deref()).await,
                    DialogAction::Dismiss => opened.dismiss_dialog().await,
                };
                let response = match result {
                    Ok(dialog) => {
                        if let Err(error) = state.append(JournalEvent::DialogHandled {
                            action: match action {
                                DialogAction::Accept => "accept".to_owned(),
                                DialogAction::Dismiss => "dismiss".to_owned(),
                            },
                            text: text.map(Value::String),
                        }) {
                            state.error(request.id, "artifacts", error.to_string(), None)
                        } else {
                            state.response(request.id, json!({ "dialog": dialog, "handled": true }))
                        }
                    }
                    Err(error) => state.error(request.id, "validation", error.to_string(), None),
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Export { name } => {
                if let Err(error) = state.write_report(&chromium) {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
                let response = match export_bundle(
                    &options.artifacts,
                    &mut state,
                    &name,
                    session.as_ref().expect("eager session"),
                ) {
                    Ok(result) => state.response(request.id, result),
                    Err(CommandError {
                        code,
                        message,
                        details,
                    }) => state.error(request.id, code, message, details),
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Output { name } => {
                let value = session.as_ref().and_then(|session| session.output(&name));
                let response = match value {
                    Some(value) => {
                        state.response(request.id, json!({ "name": name, "value": value }))
                    }
                    None => state.error(
                        request.id,
                        "output_not_found",
                        format!("runtime output {name:?} is unavailable"),
                        None,
                    ),
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Cancel => {
                let response =
                    state.error(request.id, "not_active", "no submission is active", None);
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                    break;
                }
            }
            SessionCommand::Close => {
                let mut close_result =
                    match session.take().expect("eager session").close(&host).await {
                        Ok(result) => result,
                        Err(error) => {
                            let response =
                                state.error(request.id, "browser", error.to_string(), None);
                            let _ = write_response(&mut stdout, &response).await;
                            exit_code = ExitCode::Infrastructure;
                            close_session = true;
                            continue;
                        }
                    };
                if let Err(error) =
                    publish_close_artifacts(&mut state, &chromium, &mut close_result)
                {
                    let response = state.error(request.id, "artifacts", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    close_session = true;
                    continue;
                }
                let response = state.response(
                    request.id,
                    json!({
                        "closed": true,
                        "bundle": state.bundle,
                        "recording": close_result.recording,
                        "warnings": close_result.warnings,
                    }),
                );
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                }
                close_session = true;
            }
        }
    }

    if let Some(opened) = session {
        match opened.close(&host).await {
            Ok(mut result) => {
                if let Err(error) = publish_close_artifacts(&mut state, &chromium, &mut result) {
                    eprintln!("error: finalize session artifacts: {error}");
                    exit_code = ExitCode::Infrastructure;
                }
            }
            Err(error) => {
                eprintln!("error: close browser session: {error}");
                exit_code = ExitCode::Infrastructure;
            }
        }
    }
    if let Err(error) = host.shutdown().await {
        eprintln!("error: shut down Chromium: {error}");
        exit_code = ExitCode::Infrastructure;
    }
    exit_code
}

struct CommandError {
    code: &'static str,
    message: String,
    details: Option<Value>,
}

fn prepare_action(
    action: &Value,
    session: &BrowserSession,
) -> Result<(Value, Option<Value>, String, Option<i64>), CommandError> {
    let object = action
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or_else(|| {
            validation_error("action must be an object containing exactly one operation")
        })?;
    let (name, payload) = object.iter().next().expect("one action");
    if !matches!(
        name.as_str(),
        "open"
            | "click"
            | "double_click"
            | "fill"
            | "erase"
            | "select"
            | "press"
            | "back"
            | "switch_page"
            | "switch_frame"
            | "wait_until_visible"
            | "wait_until_stable"
            | "pause"
            | "assert"
    ) {
        return Err(validation_error(format!(
            "unsupported interactive action {name:?}"
        )));
    }
    let mut durable = None;
    let mut backend_node_id = None;
    let step_payload = match name.as_str() {
        "open" => payload
            .as_object()
            .and_then(|object| object.get("url"))
            .cloned()
            .ok_or_else(|| validation_error("act.open requires url"))?,
        "click" | "double_click" | "fill" | "erase" | "select" | "press" | "wait_until_visible"
        | "wait_until_stable" => {
            let mut object = payload
                .as_object()
                .cloned()
                .ok_or_else(|| validation_error(format!("act.{name} must be an object")))?;
            let reference = take_reference(&mut object, session, &mut backend_node_id)?;
            durable = Some(reference.clone());
            object.insert("target".into(), reference);
            Value::Object(object)
        }
        "switch_frame" if payload.get("ref").is_some() => {
            let mut object = payload.as_object().cloned().expect("ref requires object");
            let reference = take_reference(&mut object, session, &mut backend_node_id)?;
            durable = Some(reference.clone());
            json!({ "target": reference })
        }
        "assert" => {
            replace_assertion_refs(payload.clone(), session, &mut durable, &mut backend_node_id)?
        }
        _ => payload.clone(),
    };
    let mut step = serde_json::Map::new();
    step.insert(name.clone(), step_payload);
    Ok((Value::Object(step), durable, name.clone(), backend_node_id))
}

fn take_reference(
    object: &mut serde_json::Map<String, Value>,
    session: &BrowserSession,
    backend_node_id: &mut Option<i64>,
) -> Result<Value, CommandError> {
    let value = object
        .remove("ref")
        .ok_or_else(|| validation_error("ref is required"))?;
    let reference = value
        .as_str()
        .ok_or_else(|| validation_error("ref must be a string"))?
        .parse::<ElementRef>()
        .map_err(|error| validation_error(error.to_string()))?;
    session
        .resolve_ref(reference)
        .map(|(locator, backend)| {
            *backend_node_id = Some(backend);
            locator
        })
        .map_err(|error| match error {
            ReferenceError::Unknown { .. } => CommandError {
                code: "unknown_reference",
                message: error.to_string(),
                details: Some(json!({ "latest_snapshot_revision": session.snapshot_revision() })),
            },
            ReferenceError::Stale { .. } => CommandError {
                code: "stale_reference",
                message: error.to_string(),
                details: Some(json!({ "latest_snapshot_revision": session.snapshot_revision() })),
            },
        })
}

fn replace_assertion_refs(
    mut value: Value,
    session: &BrowserSession,
    durable: &mut Option<Value>,
    backend_node_id: &mut Option<i64>,
) -> Result<Value, CommandError> {
    fn visit(
        value: &mut Value,
        session: &BrowserSession,
        durable: &mut Option<Value>,
        backend_node_id: &mut Option<i64>,
    ) -> Result<(), CommandError> {
        match value {
            Value::Object(object) if object.contains_key("ref") => {
                let locator = take_reference(object, session, backend_node_id)?;
                *durable = Some(locator.clone());
                *value = locator;
            }
            Value::Object(object) => {
                for child in object.values_mut() {
                    visit(child, session, durable, backend_node_id)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    visit(child, session, durable, backend_node_id)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut value, session, durable, backend_node_id)?;
    Ok(value)
}

fn compile_interactive(step: &Value, session: &BrowserSession) -> Result<CompiledFlow, String> {
    let mut executable = step.clone();
    let mut secrets = serde_json::Map::new();
    replace_environment_values(&mut executable, &mut secrets)?;
    let settings = session.settings();
    let source = serde_json::to_string(&json!({
        "version": 1,
        "name": "interactive-action",
        "settings": {
            "timeout": format!("{}ms", settings.timeout.as_millis()),
            "viewport": { "width": settings.viewport.width, "height": settings.viewport.height },
            "video": "off",
        },
        "secrets": secrets,
        "steps": [executable],
    }))
    .expect("interactive flow serializes");
    compile_inline_yaml(
        &source,
        "interactive-action.yaml",
        &BTreeMap::new(),
        &session.output_names(),
    )
    .map_err(|error| error.to_string())
}

fn register_flow_secrets(journal: &mut JournalWriter, flow: &CompiledFlow) {
    for value in flow.inputs.values().filter(|value| value.is_secret()) {
        journal.register_secret(value.expose().clone());
    }
}

fn replace_environment_values(
    value: &mut Value,
    secrets: &mut serde_json::Map<String, Value>,
) -> Result<(), String> {
    if let Some(environment) = value
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("env"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if !valid_environment_name(&environment) {
            return Err("environment name must match [A-Za-z_][A-Za-z0-9_]*".to_owned());
        }
        std::env::var(&environment)
            .map_err(|_| format!("environment variable {environment:?} is unavailable"))?;
        let name = format!("interactive_secret_{}", secrets.len() + 1);
        secrets.insert(name.clone(), json!({ "env": environment }));
        *value = Value::String(format!("${{{name}}}"));
        return Ok(());
    }
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                replace_environment_values(child, secrets)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                replace_environment_values(child, secrets)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validation_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: "validation",
        message: message.into(),
        details: None,
    }
}

fn export_bundle(
    artifacts_root: &Path,
    state: &mut ProtocolState,
    name: &str,
    session: &BrowserSession,
) -> Result<Value, CommandError> {
    if !is_safe_bundle_name(name) {
        return Err(CommandError {
            code: "export_invalid",
            message: "bundle name must be a safe path component".to_owned(),
            details: None,
        });
    }
    let bundle = artifacts_root.join(name);
    if bundle.exists() && state.bundle.as_ref() != Some(&bundle) {
        return Err(CommandError {
            code: "export_invalid",
            message: format!("bundle {} already exists", bundle.display()),
            details: None,
        });
    }
    let replay = build_replay_yaml(name, &state.events).map_err(|error| CommandError {
        code: "export_invalid",
        message: error.to_string(),
        details: None,
    })?;
    let replay_flow = compile_inline_yaml(
        &replay.yaml,
        bundle.join("replay.yaml"),
        &BTreeMap::new(),
        &session.output_names(),
    )
    .map_err(|error| CommandError {
        code: "export_invalid",
        message: error.to_string(),
        details: None,
    })?;
    register_flow_secrets(&mut state.journal, &replay_flow);
    if state.bundle.as_ref() != Some(&bundle) {
        match fs::create_dir(&bundle) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CommandError {
                    code: "export_invalid",
                    message: format!("bundle {} already exists", bundle.display()),
                    details: None,
                });
            }
            Err(error) => return Err(artifact_command_error(error)),
        }
    }
    publish_replay_atomic(bundle.join("replay.yaml"), &replay.yaml).map_err(|error| {
        CommandError {
            code: "artifacts",
            message: error.to_string(),
            details: None,
        }
    })?;
    state
        .append(JournalEvent::Export {
            name: name.to_owned(),
        })
        .map_err(|error| CommandError {
            code: "artifacts",
            message: error.to_string(),
            details: None,
        })?;
    state.journal.flush().map_err(artifact_command_error)?;
    copy_file_atomic(&state.journal_path, &bundle.join("session.ndjson"))
        .map_err(artifact_command_error)?;
    let report = state.artifacts.join("report.json");
    if report.exists() {
        copy_file_atomic(&report, &bundle.join("report.json")).map_err(artifact_command_error)?;
    }
    let screenshots =
        copy_screenshots(&state.artifacts, &bundle).map_err(artifact_command_error)?;
    state.bundle = Some(bundle.clone());
    Ok(json!({
        "name": name,
        "bundle": bundle,
        "replay": bundle.join("replay.yaml"),
        "journal": bundle.join("session.ndjson"),
        "report": bundle.join("report.json"),
        "screenshots": screenshots,
        "recording_pending": true,
        "step_count": replay.step_count,
    }))
}

fn artifact_command_error(error: std::io::Error) -> CommandError {
    CommandError {
        code: "artifacts",
        message: error.to_string(),
        details: None,
    }
}

fn copy_screenshots(from: &Path, to: &Path) -> std::io::Result<Vec<String>> {
    let mut copied = Vec::new();
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "png")
        {
            let destination = to.join(entry.file_name());
            copy_file_atomic(&entry.path(), &destination)?;
            copied.push(destination.to_string_lossy().into_owned());
        }
    }
    copied.sort();
    Ok(copied)
}

fn publish_close_artifacts(
    state: &mut ProtocolState,
    chromium: &ChromiumInfo,
    close: &mut BrowserSessionClose,
) -> anyhow::Result<()> {
    state.write_report(chromium)?;
    let mut warnings = close.warnings.clone();
    warnings.extend(close.recording.warnings.clone());
    warnings.sort();
    warnings.dedup();
    for warning in warnings {
        state.append(JournalEvent::RecorderWarning { warning })?;
    }
    state.append(JournalEvent::Close {
        summary: json!({ "recording": close.recording }),
    })?;
    finalize_bundle(state, &mut close.recording)
}

fn finalize_bundle(
    state: &mut ProtocolState,
    recording: &mut crate::runner::SessionRecordingFinish,
) -> anyhow::Result<()> {
    let Some(bundle) = &state.bundle else {
        return Ok(());
    };
    state.journal.flush()?;
    copy_file_atomic(&state.journal_path, &bundle.join("session.ndjson"))?;
    let report = state.artifacts.join("report.json");
    if report.exists() {
        copy_file_atomic(&report, &bundle.join("report.json"))?;
    }
    if let Some(path) = recording
        .path
        .as_deref()
        .or(recording.partial_path.as_deref())
    {
        let source = Path::new(path);
        if source.exists() {
            let destination = bundle.join(source.file_name().unwrap_or_default());
            copy_file_atomic(source, &destination)?;
            let published = destination.to_string_lossy().into_owned();
            if recording.path.as_deref() == Some(path) {
                recording.path = Some(published);
            } else {
                recording.partial_path = Some(published);
            }
        }
    }
    let _ = copy_screenshots(&state.artifacts, bundle)?;
    Ok(())
}

fn copy_file_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut source = fs::File::open(source)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    io::copy(&mut source, &mut temporary)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

enum Envelope {
    Line(Vec<u8>),
    TooLarge,
    Eof,
}

struct EnvelopeReader<R> {
    reader: R,
    line: Vec<u8>,
    too_large: bool,
}

impl<R: AsyncBufRead + Unpin> EnvelopeReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::with_capacity(8192),
            too_large: false,
        }
    }

    async fn read_envelope(&mut self) -> io::Result<Envelope> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.line.is_empty() && !self.too_large {
                    return Ok(Envelope::Eof);
                }
                return Ok(self.take_envelope());
            }
            let end = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            let complete = available[end - 1] == b'\n';
            let content_end = end - usize::from(complete);
            if !self.too_large {
                let remaining = MAX_ENVELOPE_BYTES - self.line.len();
                if content_end > remaining {
                    self.too_large = true;
                } else {
                    self.line.extend_from_slice(&available[..content_end]);
                }
            }
            self.reader.consume(end);
            if complete {
                return Ok(self.take_envelope());
            }
        }
    }

    fn take_envelope(&mut self) -> Envelope {
        if std::mem::take(&mut self.too_large) {
            self.line.clear();
            Envelope::TooLarge
        } else {
            Envelope::Line(std::mem::replace(&mut self.line, Vec::with_capacity(8192)))
        }
    }
}

fn decode_request(line: &[u8]) -> Result<Request, (Value, String)> {
    let value: Value =
        serde_json::from_slice(line).map_err(|error| (Value::Null, error.to_string()))?;
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|error| (id, error.to_string()))
}

async fn write_response(
    stdout: &mut tokio::io::Stdout,
    response: &Response,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    stdout.write_all(&bytes).await?;
    stdout.flush().await
}

fn session_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", std::process::id(), timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_preserve_ids_and_reject_unknown_commands() {
        let request = decode_request(br#"{"id":"a","command":"output","name":"value"}"#)
            .expect("decode request");
        assert_eq!(request.id, "a");
        let error = decode_request(br#"{"id":7,"command":"unknown"}"#).unwrap_err();
        assert_eq!(error.0, 7);
    }

    #[test]
    fn interactive_commands_decode_with_documented_defaults() {
        let snapshot = decode_request(br#"{"id":1,"command":"snapshot"}"#).unwrap();
        assert!(matches!(
            snapshot.command,
            SessionCommand::Snapshot {
                screenshot: SnapshotScreenshot::None,
                accessibility: true,
                since: None
            }
        ));
        let scroll = decode_request(br#"{"id":2,"command":"scroll","y":700}"#).unwrap();
        assert!(matches!(
            scroll.command,
            SessionCommand::Scroll { x: 0, y: 700 }
        ));
        assert!(
            decode_request(br#"{"id":3,"command":"dialog","action":"dismiss","text":"no"}"#)
                .is_ok()
        );
    }

    #[test]
    fn environment_value_replacement_is_strict_and_secret_backed() {
        unsafe { std::env::set_var("PLAYRUST_TEST_SECRET", "canary") };
        let mut value = json!({ "fill": { "value": { "env": "PLAYRUST_TEST_SECRET" } } });
        let mut secrets = serde_json::Map::new();
        replace_environment_values(&mut value, &mut secrets).unwrap();
        unsafe { std::env::remove_var("PLAYRUST_TEST_SECRET") };

        assert_eq!(value["fill"]["value"], "${interactive_secret_1}");
        assert_eq!(
            secrets["interactive_secret_1"],
            json!({ "env": "PLAYRUST_TEST_SECRET" })
        );
        assert!(!value.to_string().contains("canary"));
        assert!(!valid_environment_name("BAD-NAME"));
    }

    #[test]
    fn artifact_copy_replaces_only_with_a_complete_file() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, "new complete value").unwrap();
        fs::write(&destination, "old").unwrap();

        copy_file_atomic(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination).unwrap(),
            "new complete value"
        );
    }

    #[tokio::test]
    async fn envelope_reader_bounds_and_recovers_at_the_next_line() {
        let mut bytes = vec![b'x'; MAX_ENVELOPE_BYTES + 1];
        bytes.extend_from_slice(b"\n{}\n");
        let mut reader = EnvelopeReader::new(BufReader::new(bytes.as_slice()));

        assert!(matches!(
            reader.read_envelope().await.unwrap(),
            Envelope::TooLarge
        ));
        let Envelope::Line(line) = reader.read_envelope().await.unwrap() else {
            panic!("expected line after oversized envelope");
        };
        assert_eq!(line, b"{}");
    }

    #[tokio::test]
    async fn envelope_reader_accepts_the_documented_limit() {
        let mut bytes = vec![b' '; MAX_ENVELOPE_BYTES];
        bytes.push(b'\n');
        let mut reader = EnvelopeReader::new(BufReader::new(bytes.as_slice()));
        let Envelope::Line(line) = reader.read_envelope().await.unwrap() else {
            panic!("expected maximum-sized line");
        };
        assert_eq!(line.len(), MAX_ENVELOPE_BYTES);
    }

    #[tokio::test]
    async fn envelope_reader_counts_cr_and_accepts_crlf() {
        let mut accepted = vec![b' '; MAX_ENVELOPE_BYTES - 1];
        accepted.extend_from_slice(b"\r\n");
        let mut reader = EnvelopeReader::new(BufReader::new(accepted.as_slice()));
        let Envelope::Line(line) = reader.read_envelope().await.unwrap() else {
            panic!("expected CRLF line at the limit");
        };
        assert_eq!(line.len(), MAX_ENVELOPE_BYTES);
        assert_eq!(line.last(), Some(&b'\r'));

        let mut rejected = vec![b' '; MAX_ENVELOPE_BYTES];
        rejected.extend_from_slice(b"\r\n");
        let mut reader = EnvelopeReader::new(BufReader::new(rejected.as_slice()));
        assert!(matches!(
            reader.read_envelope().await.unwrap(),
            Envelope::TooLarge
        ));
    }

    #[tokio::test]
    async fn envelope_reader_preserves_partial_input_when_a_read_is_cancelled() {
        let (read, mut write) = tokio::io::duplex(256);
        let mut reader = EnvelopeReader::new(BufReader::new(read));
        write
            .write_all(b"{\"id\":\"cancel\",\"command\":\"")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), reader.read_envelope())
                .await
                .is_err()
        );

        write.write_all(b"cancel\"}\n").await.unwrap();
        let Envelope::Line(line) = reader.read_envelope().await.unwrap() else {
            panic!("expected completed envelope");
        };
        let request = decode_request(&line).unwrap();
        assert!(matches!(request.command, SessionCommand::Cancel));
        assert_eq!(request.id, "cancel");
    }

    #[test]
    fn compiled_secrets_are_registered_with_the_journal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.ndjson");
        let flow = crate::flow::compile_yaml_with_env(
            "version: 1\nname: test\nsecrets: { password: { env: TEST_PASSWORD } }\nsteps: [{ open: https://example.test }]\n",
            "test.yaml",
            &BTreeMap::new(),
            &BTreeMap::from([("TEST_PASSWORD".to_owned(), "secret-canary".to_owned())]),
        )
        .unwrap();
        let mut journal = JournalWriter::open(&path).unwrap();
        register_flow_secrets(&mut journal, &flow);
        journal
            .append(&JournalEvent::RecorderWarning {
                warning: "value=secret-canary".to_owned(),
            })
            .unwrap();

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("[REDACTED]"));
        assert!(!contents.contains("secret-canary"));
    }

    #[test]
    fn invalid_utf8_is_an_invalid_command_with_a_null_id() {
        let error =
            decode_request(b"{\"id\":\"lost\",\"command\":\"cancel\",\"x\":\xff}").unwrap_err();
        assert_eq!(error.0, Value::Null);
    }

    #[test]
    fn every_response_advances_the_revision() {
        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("session.ndjson");
        let mut state = ProtocolState {
            id: "session".to_owned(),
            revision: 0,
            submissions: 0,
            inspections: 0,
            started: Instant::now(),
            reports: Vec::new(),
            artifacts: std::path::Path::new("artifacts").to_owned(),
            snapshot_count: 0,
            journal: JournalWriter::open(&journal_path).unwrap(),
            journal_path,
            events: Vec::new(),
            bundle: None,
            artifact_error: None,
        };
        assert_eq!(state.response(json!(1), json!({})).revision, 1);
        assert_eq!(state.error(json!(2), "test", "failed", None).revision, 2);
    }
}
