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
use super::super::{deadline_timeout_ms, operation_locator, operation_name, settle_video};
use super::RunOptions;
use super::overlays::{
    deactivate_presentation_overlay, pause_until, remove_presentation_overlay,
    step_captures_screenshot, update_presentation_overlay,
};
use super::recorder::{
    VideoStartup, apply_video_finish, capture_failure_screenshot, capture_screenshot, start_video,
    step_failure,
};
use super::runtime::SessionRuntime;
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

pub(crate) async fn execute_flow(
    host: &BrowserHost,
    session: &mut SessionRuntime,
    flow: &CompiledFlow,
    options: &RunOptions,
) -> FlowReport {
    let started = Instant::now();
    let mut artifacts = ArtifactPaths {
        directory: path_text(&options.artifact_directory),
        ..ArtifactPaths::default()
    };
    if is_cancelled(options.cancellation.as_ref()) {
        return report(flow, started, artifacts, Vec::new(), true);
    }
    let context_id = session
        .context
        .as_ref()
        // Invariant: SessionRuntime::open (which produced `session`) stores an
        // open browser context; context is only taken by close(), which runs
        // after execute_flow. execute_flow runs on a live session, so context
        // is Some.
        .expect("open session context during execute_flow")
        .id()
        .clone();
    let placeholder = ActiveContext::new(session.active.page.clone());
    let mut active = std::mem::replace(&mut session.active, placeholder);
    let page = active.page.clone();

    let mut primary = None;
    let mut additional_failures = Vec::new();
    let mut redactor = std::mem::take(&mut session.redactor);
    redactor.extend(&flow.redactor);
    let mut runtime = RuntimeState {
        outputs: std::mem::take(&mut session.outputs),
        redactor,
        page_settings: session.page_settings,
        guard_results: BTreeMap::new(),
        stopped_loops: BTreeSet::new(),
        expects_dialog: flow
            .steps
            .iter()
            .any(|step| matches!(step.operation, Operation::Dialog { .. })),
        dialog_listener: None,
        presentation_overlays: flow.settings.overlays,
        presentation_overlay_recording: false,
    };
    let mut interrupted = is_cancelled(options.cancellation.as_ref());
    let mut browser_status = host.subscribe_status();
    let mut recording_error = None;
    let mut video = None;
    let manual_recording = flow.manual_recording;
    if !interrupted && !manual_recording {
        if let Some(deadline) = Instant::now().checked_add(flow.settings.timeout) {
            match start_video(&page, flow, options, deadline).await {
                Ok(VideoStartup::Ready(session)) => {
                    video = session;
                    runtime.presentation_overlay_recording = video.is_some();
                    interrupted = is_cancelled(options.cancellation.as_ref());
                }
                Ok(VideoStartup::Cancelled(finish)) => {
                    interrupted = true;
                    if let Some(finish) = finish {
                        apply_video_finish(finish, &mut artifacts, &mut recording_error);
                    }
                }
                Err(error) => {
                    recording_error = Some(error);
                    interrupted = is_cancelled(options.cancellation.as_ref());
                }
            }
        } else {
            recording_error = Some("recording timeout is too large".to_owned());
        }
    }

    let mut video_stop_at = None;
    'steps: for step in &flow.steps {
        if interrupted {
            break;
        }
        if let Operation::Recording(control) = step.operation {
            let Some(deadline) = Instant::now().checked_add(step.timeout) else {
                primary = Some(
                    step_failure(
                        host,
                        flow,
                        &runtime.redactor,
                        &active,
                        step,
                        StepError::new(FailureCategory::Protocol, "step timeout is too large"),
                    )
                    .await,
                );
                break;
            };
            let matches = tokio::select! {
                biased;
                _ = wait_for_cancellation(options.cancellation.as_ref()) => {
                    interrupted = true;
                    video_stop_at = Some(Instant::now());
                    break;
                }
                error = browser_unavailable(&mut browser_status) => {
                    primary = Some(step_failure(
                        host,
                        flow,
                        &runtime.redactor,
                        &active,
                        step,
                        protocol(error),
                    ).await);
                    break;
                }
                result = tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    step_matches(&active, step, &mut runtime),
                ) => result,
            };
            match matches {
                Ok(Ok(false)) => continue,
                Ok(Ok(true)) => {}
                Ok(Err(error)) => {
                    primary = Some(
                        step_failure(host, flow, &runtime.redactor, &active, step, error).await,
                    );
                    break;
                }
                Err(_) => {
                    primary = Some(
                        step_failure(
                            host,
                            flow,
                            &runtime.redactor,
                            &active,
                            step,
                            StepError::new(FailureCategory::Timeout, "step deadline expired")
                                .deadline(),
                        )
                        .await,
                    );
                    break;
                }
            }
            match control {
                RecordingControl::Start => {
                    match start_video(&active.page, flow, options, deadline).await {
                        Ok(VideoStartup::Ready(session)) => {
                            video = session;
                            runtime.presentation_overlay_recording = video.is_some();
                        }
                        Ok(VideoStartup::Cancelled(finish)) => {
                            interrupted = true;
                            if let Some(finish) = finish {
                                apply_video_finish(finish, &mut artifacts, &mut recording_error);
                            }
                        }
                        Err(error) => {
                            primary = Some(failure(
                                flow,
                                FailureCategory::Recording,
                                error,
                                Some(step_context(flow, step)),
                            ));
                        }
                    }
                }
                RecordingControl::Stop => {
                    settle_video(&active.page).await;
                    video_stop_at = Some(Instant::now());
                    if let Some(session) = video.take() {
                        let finish = session
                            .finish(
                                &active.page,
                                true,
                                video_stop_at.expect("recording stop set"),
                            )
                            .await;
                        apply_video_finish(finish, &mut artifacts, &mut recording_error);
                    }
                    deactivate_presentation_overlay(&active, &mut runtime).await;
                }
            }
            interrupted |= is_cancelled(options.cancellation.as_ref());
            if primary.is_some() || interrupted {
                break;
            }
            if let Some(error) = recording_error.take() {
                primary = Some(failure(
                    flow,
                    FailureCategory::Recording,
                    error,
                    Some(step_context(flow, step)),
                ));
                break;
            }
            continue;
        }
        let mut error = None;
        for attempt in 0..=step.retries {
            if runtime.presentation_overlays_active() {
                if step_captures_screenshot(step) {
                    let _ = remove_presentation_overlay(&active).await;
                } else {
                    let _ = update_presentation_overlay(
                        &active,
                        step,
                        &runtime.presentation_overlays,
                        &runtime.redactor,
                    )
                    .await;
                }
            }
            let Some(deadline) = Instant::now().checked_add(step.timeout) else {
                error = Some(StepError::new(
                    FailureCategory::Protocol,
                    "step timeout is too large",
                ));
                break;
            };
            let result = tokio::select! {
                biased;
                _ = wait_for_cancellation(options.cancellation.as_ref()) => {
                    interrupted = true;
                    video_stop_at = Some(Instant::now());
                    break 'steps;
                }
                browser_error = browser_unavailable(&mut browser_status) => {
                    error = Some(protocol(browser_error));
                    break;
                }
                result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                    #[cfg(test)]
                    if let Some(observer) = &options.step_started_observer {
                        (observer.0)(operation_name(&step.operation));
                    }
                    execute_step(
                        host,
                        &context_id,
                        &mut active,
                        step,
                        deadline,
                        &options.artifact_directory,
                        &mut runtime,
                    ).await
                }) => result,
            };
            match result {
                Ok(Ok(screenshot)) => {
                    if runtime.presentation_overlays_active() {
                        let _ = update_presentation_overlay(
                            &active,
                            step,
                            &runtime.presentation_overlays,
                            &runtime.redactor,
                        )
                        .await;
                        settle_video(&active.page).await;
                    }
                    if let Some(path) = screenshot {
                        artifacts.screenshots.push(path_text(&path));
                    }
                    continue 'steps;
                }
                Ok(Err(attempt_error)) => error = Some(attempt_error),
                Err(_) => {
                    error = Some(
                        StepError::new(FailureCategory::Timeout, "step deadline expired")
                            .deadline(),
                    )
                }
            }
            if attempt < step.retries {
                continue;
            }
        }
        video_stop_at = Some(Instant::now());
        let error = error.expect("failed attempt records an error");
        if let Some(visual) = &error.visual_artifacts {
            match publish_visual_artifacts(&options.artifact_directory, visual).await {
                Ok(()) => {
                    artifacts.visual_actual = Some(path_text(&visual.actual_path));
                    artifacts.visual_diff = Some(path_text(&visual.diff_path));
                }
                Err(publication_error) => additional_failures.push(
                    step_failure(
                        host,
                        flow,
                        &runtime.redactor,
                        &active,
                        step,
                        publication_error,
                    )
                    .await,
                ),
            }
        }
        if runtime.presentation_overlays_active() {
            let _ = remove_presentation_overlay(&active).await;
        }
        artifacts.failure_screenshot =
            capture_failure_screenshot(&active, &options.artifact_directory)
                .await
                .map(|path| path_text(&path));
        primary = Some(step_failure(host, flow, &runtime.redactor, &active, step, error).await);
        break;
    }

    if primary.is_none() && !interrupted && video.is_some() {
        settle_video(&active.page).await;
        video_stop_at = Some(Instant::now());
    }

    if let Some(session) = video.take() {
        let flow_failed = primary.is_some() || interrupted;
        let finish = session
            .finish(
                &active.page,
                flow_failed,
                video_stop_at.unwrap_or_else(Instant::now),
            )
            .await;
        apply_video_finish(finish, &mut artifacts, &mut recording_error);
    }
    deactivate_presentation_overlay(&active, &mut runtime).await;

    let mut failures = primary.into_iter().collect::<Vec<_>>();
    failures.append(&mut additional_failures);
    if let Some(error) = recording_error {
        failures.push(failure(flow, FailureCategory::Recording, error, None));
    }
    if !failures.is_empty() && artifacts.failure_screenshot.is_none() {
        artifacts.failure_screenshot =
            capture_failure_screenshot(&active, &options.artifact_directory)
                .await
                .map(|path| path_text(&path));
    }

    if manual_recording
        && flow.settings.video == VideoMode::RetainOnFailure
        && failures.is_empty()
        && !interrupted
        && let Some(path) = artifacts.recording.take()
        && let Err(error) = tokio::fs::remove_file(&path).await
    {
        artifacts.recording = Some(path.clone());
        failures.push(failure(
            flow,
            FailureCategory::Recording,
            format!("remove passing recording {path}: {error}"),
            None,
        ));
    }

    session.active = active;
    session.outputs = runtime.outputs;
    session.redactor = runtime.redactor;
    report(flow, started, artifacts, failures, interrupted)
}

pub(crate) async fn execute_step(
    host: &BrowserHost,
    context_id: &chromiumoxide::cdp::browser_protocol::browser::BrowserContextId,
    active: &mut ActiveContext,
    step: &CompiledStep,
    deadline: Instant,
    artifact_directory: &Path,
    runtime: &mut RuntimeState,
) -> Result<Option<PathBuf>, StepError> {
    if !step_matches(active, step, runtime).await? {
        return Ok(None);
    }
    match &step.operation {
        Operation::Open { url, settle } => {
            navigate(active, url.expose().as_str(), deadline).await?;
            if let Some(settle) = settle {
                let settle_by = prepare_open_settle(deadline)?;
                settle_after_open(active, settle, settle_by).await?;
            }
            Ok(None)
        }
        Operation::Click { target, position } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, *position, deadline).await?;
            let (x, y) = page_point(active, element.center.x, element.center.y).await?;
            dispatch_flow_click(&active.page, x, y, 1, runtime)
                .await
                .map(|_| None)
        }
        Operation::ClickPoint { point } => dispatch_flow_click(
            &active.page,
            f64::from(point.x),
            f64::from(point.y),
            1,
            runtime,
        )
        .await
        .map_err(|mut error| {
            error.message = format!(
                "viewport click at ({}, {}) failed: {}",
                point.x, point.y, error.message
            );
            error
        })
        .map(|_| None),
        Operation::DoubleClick { target, position } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, *position, deadline).await?;
            let (x, y) = page_point(active, element.center.x, element.center.y).await?;
            dispatch_flow_click(&active.page, x, y, 2, runtime)
                .await
                .map(|_| None)
        }
        Operation::Fill { target, value } => {
            let value = resolve_runtime(value, &runtime.outputs)?;
            let element =
                wait_actionable(active, target, Actionability::EDITABLE, None, deadline).await?;
            prepare_fill(active.target(), element.backend_node_id).await?;
            active
                .page
                .execute(InsertTextParams::new(value.expose()))
                .await
                .map_err(protocol)?;
            Ok(None)
        }
        Operation::Erase { target } => {
            let element =
                wait_actionable(active, target, Actionability::EDITABLE, None, deadline).await?;
            erase(active.target(), element.backend_node_id)
                .await
                .map(|_| None)
        }
        Operation::Select { target, value } => {
            let value = resolve_runtime(value, &runtime.outputs)?;
            let element =
                wait_actionable(active, target, Actionability::CLICK, None, deadline).await?;
            select(active.target(), element.backend_node_id, value.expose())
                .await
                .map(|_| None)
        }
        Operation::Scroll { x, y } => dispatch_scroll(active, *x, *y).await.map(|_| None),
        Operation::ScrollUntilVisible { target, x, y } => {
            scroll_until_visible(active, target, *x, *y, deadline)
                .await
                .map(|_| None)
        }
        Operation::Swipe {
            target,
            x,
            y,
            duration,
        } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, None, deadline).await?;
            dispatch_swipe(active, &element, *x, *y, *duration, deadline)
                .await
                .map(|_| None)
        }
        Operation::LongPress { target, duration } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, None, deadline).await?;
            dispatch_long_press(active, &element, *duration, deadline)
                .await
                .map(|_| None)
        }
        Operation::WaitUntilVisible { target } => {
            wait_actionable(active, target, Actionability::VISIBLE, None, deadline)
                .await
                .map(|_| None)
        }
        Operation::WaitUntilStable { target } => {
            wait_actionable(active, target, Actionability::STABLE, None, deadline)
                .await
                .map(|_| None)
        }
        Operation::Pause { duration } => pause_until(*duration, deadline).await.map(|_| None),
        Operation::Back if active.frame().is_some() => Err(StepError::new(
            FailureCategory::Navigation,
            "back navigation is unsupported inside a frame; switch_frame to main first",
        )),
        Operation::Back => navigate_back(&active.page, deadline).await.map(|_| None),
        Operation::SwitchPage(page) => switch_page(
            host,
            context_id,
            active,
            page,
            deadline,
            runtime.page_settings.viewport,
            runtime.page_settings.geolocation,
        )
        .await
        .map(|_| None),
        Operation::SwitchFrame(frame) => switch_frame(active, frame, deadline).await.map(|_| None),
        Operation::Press {
            target,
            key,
            modifiers,
        } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, None, deadline).await?;
            focus(active.target(), element.backend_node_id).await?;
            dispatch_key(&active.page, key, modifiers)
                .await
                .map(|_| None)
        }
        Operation::Screenshot { name, crop } => {
            capture_screenshot(active, artifact_directory, &format!("{name}.png"), *crop)
                .await
                .map(Some)
        }
        Operation::Recording(_) => Err(StepError::new(
            FailureCategory::Protocol,
            "recording controls must be handled by the flow runner",
        )),
        Operation::Dialog { action, text } => {
            let mut params =
                HandleJavaScriptDialogParams::new(matches!(action, NativeDialogResponse::Accept));
            params.prompt_text = text
                .as_ref()
                .map(|text| resolve_runtime(text, &runtime.outputs))
                .transpose()?
                .map(|text| text.expose().to_owned());
            active.target().execute(params).await.map_err(|error| {
                StepError::new(
                    FailureCategory::Actionability,
                    format!("handle native dialog: {error}"),
                )
            })?;
            Ok(None)
        }
        Operation::Clear(ClearTarget::Cookies) => {
            host.browser()
                .execute(
                    ClearCookiesParams::builder()
                        .browser_context_id(context_id.clone())
                        .build(),
                )
                .await
                .map_err(protocol)?;
            Ok(None)
        }
        Operation::Clear(ClearTarget::Storage) => {
            evaluate(active, CLEAR_STORAGE_EXPRESSION).await?;
            Ok(None)
        }
        Operation::Assert(Assertion::Screenshot(expectation)) => {
            assert_screenshot(active, expectation, step.index, artifact_directory)
                .await
                .map(|_| None)
        }
        Operation::Clear(ClearTarget::Indexeddb) => {
            evaluate(active, CLEAR_INDEXEDDB_EXPRESSION).await?;
            Ok(None)
        }
        Operation::Clear(ClearTarget::CacheStorage) => {
            evaluate(active, CLEAR_CACHE_STORAGE_EXPRESSION).await?;
            Ok(None)
        }
        Operation::Clear(ClearTarget::ServiceWorkers) => {
            let url = active
                .url()
                .await
                .map_err(protocol)?
                .ok_or_else(|| protocol("active page has no URL"))?;
            let origin = url::Url::parse(&url)
                .map_err(protocol)?
                .origin()
                .ascii_serialization();
            active
                .page
                .execute(ClearDataForOriginParams::new(origin, "service_workers"))
                .await
                .map_err(protocol)?;
            Ok(None)
        }
        Operation::Evaluate {
            script,
            args,
            save_as,
        } => {
            let args = args
                .iter()
                .map(|value| {
                    resolve_runtime(value, &runtime.outputs)
                        .map(|value| Value::String(value.expose().clone()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let value = evaluate_page(active, script, &args, save_as.is_some()).await?;
            if let Some(name) = save_as {
                let value = value.ok_or_else(|| {
                    StepError::new(
                        FailureCategory::Protocol,
                        "page script returned no JSON-serializable value",
                    )
                })?;
                store_output(&mut runtime.outputs, &mut runtime.redactor, name, value)?;
            }
            Ok(None)
        }
        Operation::Request {
            method,
            url,
            headers,
            body,
            expected_status,
            save_as,
        } => {
            let url = resolve_runtime(url, &runtime.outputs)?;
            let value = http_request(
                method,
                url.expose(),
                headers,
                body.as_ref(),
                *expected_status,
                save_as.is_some(),
                &runtime.outputs,
            )
            .await?;
            if let (Some(name), Some(value)) = (save_as, value) {
                store_output(&mut runtime.outputs, &mut runtime.redactor, name, value)?;
            }
            Ok(None)
        }
        Operation::Assert(assertion) => assert(active, assertion, deadline).await.map(|_| None),
    }
}
