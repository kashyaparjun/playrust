use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::browser::BrowserHost;
use crate::browser_session::BrowserSession;
use crate::flow::{VideoMode, compile_inline_yaml};
use crate::report::{
    AggregateReport, ChromiumInfo, ExitCode, FlowReport, FlowStatus, RunnerInfo,
    write_aggregate_report,
};
use crate::runner::{CancellationToken, RunOptions};

/// Maximum bytes before the newline in one NDJSON command envelope.
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct SessionOptions {
    pub browser: PathBuf,
    pub headed: bool,
    pub artifacts: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
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
    Output {
        name: String,
    },
    Cancel,
    Close,
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
}

pub async fn run(options: SessionOptions) -> ExitCode {
    let host = match BrowserHost::launch(&options.browser, options.headed).await {
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
    let mut state = ProtocolState {
        id: session_id,
        revision: 0,
        submissions: 0,
        inspections: 0,
        started: Instant::now(),
        reports: Vec::new(),
        artifacts: session_artifacts,
    };
    let stdin = tokio::io::stdin();
    let mut input = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut session = None;
    let mut close_session = false;
    let mut exit_code = ExitCode::Success;

    while !close_session {
        let line = match read_envelope(&mut input).await {
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
                let compiled = match compile_inline_yaml(
                    &flow,
                    format!("submission-{:06}.yaml", state.submissions + 1),
                    &variables,
                    &outputs,
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
                if session.is_none() {
                    match BrowserSession::open(&host, &compiled).await {
                        Ok(opened) => session = Some(opened),
                        Err(error) => {
                            let response =
                                state.error(request.id, "browser", error.to_string(), None);
                            if write_response(&mut stdout, &response).await.is_err() {
                                eprintln!("error: write session response");
                            }
                            exit_code = ExitCode::Infrastructure;
                            close_session = true;
                            continue;
                        }
                    }
                }

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
                let report = loop {
                    tokio::select! {
                        report = &mut execution => break report,
                        command = read_envelope(&mut input), if requested_close.is_none() && !input_closed => {
                            let response = match command {
                                Ok(Envelope::Line(line)) => match decode_request(&line) {
                                    Ok(Request { id, command: SessionCommand::Cancel }) => {
                                        cancellation.cancel();
                                        Some(state.response(id, json!({ "cancelling": true })))
                                    }
                                    Ok(Request { id, command: SessionCommand::Close }) => {
                                        cancellation.cancel();
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
                                    input_closed = true;
                                    None
                                }
                                Err(error) => {
                                    eprintln!("error: read session command: {error}");
                                    cancellation.cancel();
                                    exit_code = ExitCode::Infrastructure;
                                    input_closed = true;
                                    None
                                }
                            };
                            if let Some(response) = response
                                && write_response(&mut stdout, &response).await.is_err()
                            {
                                cancellation.cancel();
                                exit_code = ExitCode::Infrastructure;
                            }
                        }
                    }
                };
                drop(execution);
                let report = match report {
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
                state.reports.push(report.clone());
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
                                Ok(()) => state.response(close_id, json!({ "closed": true })),
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
                let fatal = report.failures.iter().any(|failure| {
                    matches!(
                        failure.category,
                        crate::report::FailureCategory::BrowserLaunch
                            | crate::report::FailureCategory::BrowserCrash
                            | crate::report::FailureCategory::Protocol
                            | crate::report::FailureCategory::Recording
                    )
                });
                let response = if report.status == FlowStatus::Passed {
                    state.response(request.id, json!(report))
                } else {
                    state.error(
                        request.id,
                        if report.status == FlowStatus::Interrupted {
                            "cancelled"
                        } else if fatal {
                            "browser"
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
                            Ok(()) => state.response(close_id, json!({ "closed": true })),
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
                if let Some(opened) = session.take()
                    && let Err(error) = opened.close(&host).await
                {
                    let response = state.error(request.id, "browser", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    exit_code = ExitCode::Infrastructure;
                    close_session = true;
                    continue;
                }
                let response = state.response(request.id, json!({ "closed": true }));
                if write_response(&mut stdout, &response).await.is_err() {
                    exit_code = ExitCode::Infrastructure;
                }
                close_session = true;
            }
        }
    }

    if let Some(opened) = session
        && let Err(error) = opened.close(&host).await
    {
        eprintln!("error: close browser session: {error}");
        exit_code = ExitCode::Infrastructure;
    }
    if let Err(error) = host.shutdown().await {
        eprintln!("error: shut down Chromium: {error}");
        exit_code = ExitCode::Infrastructure;
    }
    exit_code
}

enum Envelope {
    Line(Vec<u8>),
    TooLarge,
    Eof,
}

async fn read_envelope(reader: &mut (impl AsyncBufRead + Unpin)) -> std::io::Result<Envelope> {
    let mut line = Vec::with_capacity(8192);
    let mut too_large = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() && !too_large {
                Envelope::Eof
            } else if too_large {
                Envelope::TooLarge
            } else {
                Envelope::Line(line)
            });
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let content_end = end - usize::from(available[end - 1] == b'\n');
        if !too_large {
            let remaining = MAX_ENVELOPE_BYTES - line.len();
            if content_end > remaining {
                too_large = true;
            } else {
                line.extend_from_slice(&available[..content_end]);
            }
        }
        let complete = available[end - 1] == b'\n';
        reader.consume(end);
        if complete {
            return Ok(if too_large {
                Envelope::TooLarge
            } else {
                Envelope::Line(line)
            });
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

    #[tokio::test]
    async fn envelope_reader_bounds_and_recovers_at_the_next_line() {
        let mut bytes = vec![b'x'; MAX_ENVELOPE_BYTES + 1];
        bytes.extend_from_slice(b"\n{}\n");
        let mut reader = BufReader::new(bytes.as_slice());

        assert!(matches!(
            read_envelope(&mut reader).await.unwrap(),
            Envelope::TooLarge
        ));
        let Envelope::Line(line) = read_envelope(&mut reader).await.unwrap() else {
            panic!("expected line after oversized envelope");
        };
        assert_eq!(line, b"{}");
    }

    #[tokio::test]
    async fn envelope_reader_accepts_the_documented_limit() {
        let bytes = vec![b' '; MAX_ENVELOPE_BYTES];
        let mut reader = BufReader::new(bytes.as_slice());
        let Envelope::Line(line) = read_envelope(&mut reader).await.unwrap() else {
            panic!("expected maximum-sized line");
        };
        assert_eq!(line.len(), MAX_ENVELOPE_BYTES);
    }

    #[test]
    fn every_response_advances_the_revision() {
        let mut state = ProtocolState {
            id: "session".to_owned(),
            revision: 0,
            submissions: 0,
            inspections: 0,
            started: Instant::now(),
            reports: Vec::new(),
            artifacts: std::path::Path::new("artifacts").to_owned(),
        };
        assert_eq!(state.response(json!(1), json!({})).revision, 1);
        assert_eq!(state.error(json!(2), "test", "failed", None).revision, 2);
    }
}
