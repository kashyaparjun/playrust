use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::browser::BrowserHost;
use crate::browser_session::BrowserSession;
use crate::flow::{VideoMode, compile_inline_yaml};
use crate::report::{
    AggregateReport, ChromiumInfo, ExitCode, FlowReport, FlowStatus, RunnerInfo,
    write_aggregate_report,
};
use crate::runner::{CancellationToken, RunOptions};

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
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut session = None;
    let mut close_session = false;

    while !close_session {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                eprintln!("error: read session command: {error}");
                break;
            }
        };
        let request = match decode_request(&line) {
            Ok(request) => request,
            Err((id, message)) => {
                let response = state.error(id, "invalid_command", message, None);
                if write_response(&mut stdout, &response).await.is_err() {
                    break;
                }
                close_session = true;
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
                            let _ = write_response(&mut stdout, &response).await;
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
                let mut requested_close = false;
                let report = loop {
                    tokio::select! {
                        report = &mut execution => break report,
                        command = lines.next_line() => {
                            let response = match command {
                                Ok(Some(line)) => match decode_request(&line) {
                                    Ok(Request { id, command: SessionCommand::Cancel }) => {
                                        cancellation.cancel();
                                        state.response(id, json!({ "cancelling": true }))
                                    }
                                    Ok(Request { id, command: SessionCommand::Close }) => {
                                        cancellation.cancel();
                                        requested_close = true;
                                        state.response(id, json!({ "closing": true }))
                                    }
                                    Ok(Request { id, .. }) => state.error(
                                        id,
                                        "busy",
                                        "one mutating submission is already active",
                                        None,
                                    ),
                                    Err((id, message)) => {
                                        cancellation.cancel();
                                        requested_close = true;
                                        state.error(id, "invalid_command", message, None)
                                    }
                                },
                                Ok(None) => {
                                    cancellation.cancel();
                                    requested_close = true;
                                    continue;
                                }
                                Err(error) => {
                                    eprintln!("error: read session command: {error}");
                                    cancellation.cancel();
                                    requested_close = true;
                                    continue;
                                }
                            };
                            if write_response(&mut stdout, &response).await.is_err() {
                                cancellation.cancel();
                                requested_close = true;
                            }
                        }
                    }
                };
                let report = match report {
                    Ok(report) => report,
                    Err(error) => {
                        let response =
                            state.error(request.id, "settings_conflict", error.to_string(), None);
                        if write_response(&mut stdout, &response).await.is_err() {
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
                    close_session = true;
                    continue;
                }
                let fatal = report.status == FlowStatus::Interrupted
                    || report.failures.iter().any(|failure| {
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
                        } else {
                            "submission_failed"
                        },
                        "submission did not pass",
                        Some(json!(report)),
                    )
                };
                if write_response(&mut stdout, &response).await.is_err() {
                    close_session = true;
                }
                close_session |= fatal || requested_close;
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
                            break;
                        }
                    }
                    Err(error) => {
                        let response = state.error(request.id, "browser", error.to_string(), None);
                        let _ = write_response(&mut stdout, &response).await;
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
                    break;
                }
            }
            SessionCommand::Cancel => {
                let response =
                    state.error(request.id, "not_active", "no submission is active", None);
                if write_response(&mut stdout, &response).await.is_err() {
                    break;
                }
            }
            SessionCommand::Close => {
                if let Some(opened) = session.take()
                    && let Err(error) = opened.close(&host).await
                {
                    let response = state.error(request.id, "browser", error.to_string(), None);
                    let _ = write_response(&mut stdout, &response).await;
                    close_session = true;
                    continue;
                }
                let response = state.response(request.id, json!({ "closed": true }));
                let _ = write_response(&mut stdout, &response).await;
                close_session = true;
            }
        }
    }

    if let Some(opened) = session
        && let Err(error) = opened.close(&host).await
    {
        eprintln!("error: close browser session: {error}");
    }
    if let Err(error) = host.shutdown().await {
        eprintln!("error: shut down Chromium: {error}");
        ExitCode::Infrastructure
    } else {
        ExitCode::Success
    }
}

fn decode_request(line: &str) -> Result<Request, (Value, String)> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| (Value::Null, error.to_string()))?;
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
        let request = decode_request(r#"{"id":"a","command":"output","name":"value"}"#)
            .expect("decode request");
        assert_eq!(request.id, "a");
        let error = decode_request(r#"{"id":7,"command":"unknown"}"#).unwrap_err();
        assert_eq!(error.0, 7);
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
