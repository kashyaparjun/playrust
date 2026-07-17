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
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetFrameOwnerParams, ResolveNodeParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EventFrameStartedNavigating, EventLifecycleEvent,
    EventScreencastFrame, FrameId, GetFrameTreeParams, GetNavigationHistoryParams, NavigateParams,
    NavigateToHistoryEntryParams, ScreencastFrameAckParams, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams, Viewport as ScreenshotViewport,
};
use chromiumoxide::cdp::browser_protocol::storage::{ClearCookiesParams, ClearDataForOriginParams};
use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, ReleaseObjectParams,
};
use chromiumoxide::keys::get_key_definition;
use chromiumoxide::page::ScreenshotParams;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, oneshot};

use crate::browser::{BrowserHost, BrowserStatus, Geolocation, Viewport};
use crate::flow::{
    Assertion, ClearTarget, CompiledFlow, CompiledStep, Crop, Expression, FrameSwitch, GuardKind,
    Key, Locator, LocatorStrategy, MAX_RUNTIME_VALUE_BYTES, Modifier, NamedKey, Operation,
    PageSwitch, RecordingControl, Redactor, RelationKind, RelativePoint, Resolved, TextMatch,
    UrlExpectation, VideoMode, VisualExpectation, When,
};
use crate::locator::{
    Actionability, LocatorEngine, LocatorError, Observation, POLL_INTERVAL, ResolvedElement,
    retryable, retryable_cdp_message, text_matches,
};
use crate::report::{
    ArtifactPaths, Failure, FailureCategory, FlowReport, FlowStatus, SafeText, StepContext,
};
use crate::video::{VideoConfig, VideoRecorder};
use crate::visual;

const SCREENSHOT_NAME: &str = "failure.png";
const RECORDING_NAME: &str = "recording.webm";
const SECONDARY_TIMEOUT: Duration = Duration::from_secs(2);
const VIDEO_FINALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const FINAL_FRAME_DELAY: Duration = Duration::from_millis(250);

const FOCUS_FUNCTION: &str = r#"function() {
    if (!this.isConnected) return false;
    this.focus();
    return document.activeElement === this;
}"#;

const PREPARE_FILL_FUNCTION: &str = r#"function() {
    if (!this.isConnected) return false;
    this.focus();
    if (this instanceof HTMLInputElement) {
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(this, '');
    } else if (this instanceof HTMLTextAreaElement) {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(this, '');
    } else if (this.isContentEditable) {
        const range = document.createRange();
        range.selectNodeContents(this);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
    } else {
        return false;
    }
    return document.activeElement === this;
}"#;

const ERASE_FUNCTION: &str = r#"function() {
    if (!this.isConnected) return 'detached';
    this.focus();
    if (document.activeElement !== this) return 'focus';
    if (this instanceof HTMLInputElement) {
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(this, '');
    } else if (this instanceof HTMLTextAreaElement) {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(this, '');
    } else if (this.isContentEditable) {
        this.replaceChildren();
    } else {
        return 'editable';
    }
    this.dispatchEvent(new InputEvent('input', {
        bubbles: true, inputType: 'deleteContentBackward', data: null
    }));
    this.dispatchEvent(new Event('change', { bubbles: true }));
    return 'ok';
}"#;

const SELECT_FUNCTION: &str = r#"function(value) {
    if (!this.isConnected) return 'detached';
    if (!(this instanceof HTMLSelectElement) || this.multiple) return 'select';
    if (!Array.from(this.options).some(option => option.value === value)) return 'option';
    this.focus();
    if (document.activeElement !== this) return 'focus';
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(this, value);
    this.dispatchEvent(new Event('input', { bubbles: true }));
    this.dispatchEvent(new Event('change', { bubbles: true }));
    return 'ok';
}"#;

const INNER_TEXT_FUNCTION: &str = r#"function() { return this.innerText; }"#;
const CLEAR_STORAGE_EXPRESSION: &str =
    "(() => { localStorage.clear(); sessionStorage.clear(); return true; })()";
const CLEAR_INDEXEDDB_EXPRESSION: &str = r#"indexedDB.databases().then(databases => Promise.all(
    databases.filter(database => database.name).map(database => new Promise((resolve, reject) => {
        const request = indexedDB.deleteDatabase(database.name);
        request.onsuccess = () => resolve();
        request.onerror = () => reject(request.error);
        request.onblocked = () => reject(new Error(`IndexedDB database ${database.name} is blocked`));
    }))
))"#;
const CLEAR_CACHE_STORAGE_EXPRESSION: &str =
    "caches.keys().then(names => Promise.all(names.map(name => caches.delete(name))))";
const FRAME_OFFSET_FUNCTION: &str = r#"function() {
    const rect = this.getBoundingClientRect();
    return [rect.left + this.clientLeft, rect.top + this.clientTop];
}"#;

struct ActiveContext {
    page: Page,
    frames: Vec<FrameId>,
}

#[derive(Clone, Copy)]
struct PageSettings {
    viewport: Viewport,
    geolocation: Option<Geolocation>,
}

impl ActiveContext {
    fn new(page: Page) -> Self {
        Self {
            page,
            frames: Vec::new(),
        }
    }

    fn frame(&self) -> Option<&FrameId> {
        self.frames.last()
    }

    fn locator(&self) -> LocatorEngine<'_> {
        LocatorEngine::in_frame(&self.page, self.frame())
    }

    async fn url(&self) -> chromiumoxide::error::Result<Option<String>> {
        match self.frame() {
            Some(frame) => self.page.frame_url(frame.clone()).await,
            None => self.page.url().await,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Per-flow resources. Give each concurrent flow a distinct artifact directory.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub artifact_directory: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
    pub cancellation: Option<CancellationToken>,
}

impl RunOptions {
    pub fn new(artifact_directory: impl Into<PathBuf>) -> Self {
        Self {
            artifact_directory: artifact_directory.into(),
            ffmpeg_path: None,
            cancellation: None,
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

/// Runs one compiled flow in a fresh incognito browser context.
pub async fn run_flow(host: &BrowserHost, flow: &CompiledFlow, options: &RunOptions) -> FlowReport {
    let started = Instant::now();
    let mut artifacts = ArtifactPaths {
        directory: path_text(&options.artifact_directory),
        ..ArtifactPaths::default()
    };
    let viewport = match Viewport::new(flow.settings.viewport.width, flow.settings.viewport.height)
    {
        Ok(viewport) => viewport,
        Err(error) => {
            return report(
                flow,
                started,
                artifacts,
                vec![failure(
                    flow,
                    FailureCategory::Protocol,
                    error.to_string(),
                    None,
                )],
                false,
            );
        }
    };
    if is_cancelled(options.cancellation.as_ref()) {
        return report(flow, started, artifacts, Vec::new(), true);
    }
    // Context creation must run to completion so a late response cannot orphan its context.
    let context = match host
        .create_context(viewport, flow.settings.geolocation)
        .await
    {
        Ok(context) => context,
        Err(error) => {
            if is_cancelled(options.cancellation.as_ref()) {
                return report(flow, started, artifacts, Vec::new(), true);
            }
            let category = browser_error_category(host);
            return report(
                flow,
                started,
                artifacts,
                vec![failure(flow, category, error.to_string(), None)],
                false,
            );
        }
    };
    let page = context.page().clone();
    let mut active = ActiveContext::new(page.clone());
    let page_settings = PageSettings {
        viewport,
        geolocation: flow.settings.geolocation,
    };

    let mut primary = None;
    let mut runtime = RuntimeState {
        outputs: BTreeMap::new(),
        redactor: flow.redactor.clone(),
        page_settings,
        guard_results: BTreeMap::new(),
        stopped_loops: BTreeSet::new(),
    };
    let mut interrupted = is_cancelled(options.cancellation.as_ref());
    let mut recording_error = None;
    let mut video = None;
    let manual_recording = flow.manual_recording;
    if !interrupted && !manual_recording {
        if let Some(deadline) = Instant::now().checked_add(flow.settings.timeout) {
            match start_video(&page, flow, options, deadline).await {
                Ok(VideoStartup::Ready(session)) => {
                    video = session;
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
                        Ok(VideoStartup::Ready(session)) => video = session,
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
                result = tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    execute_step(
                        host,
                        context.id(),
                        &mut active,
                        step,
                        deadline,
                        &options.artifact_directory,
                        &mut runtime,
                    ),
                ) => result,
            };
            match result {
                Ok(Ok(screenshot)) => {
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
        let mut error = error.expect("failed attempt records an error");
        if let Some(visual) = &error.visual_artifacts {
            match publish_visual_artifacts(&options.artifact_directory, visual).await {
                Ok(()) => {
                    artifacts.visual_actual = Some(path_text(&visual.actual_path));
                    artifacts.visual_diff = Some(path_text(&visual.diff_path));
                }
                Err(publication_error) => error = publication_error,
            }
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

    let mut failures = primary.into_iter().collect::<Vec<_>>();
    if let Some(error) = recording_error {
        failures.push(failure(flow, FailureCategory::Recording, error, None));
    }
    if !failures.is_empty() && artifacts.failure_screenshot.is_none() {
        artifacts.failure_screenshot =
            capture_failure_screenshot(&active, &options.artifact_directory)
                .await
                .map(|path| path_text(&path));
    }

    match tokio::time::timeout(SECONDARY_TIMEOUT, host.dispose_context(context)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => failures.push(failure(
            flow,
            browser_error_category(host),
            error.to_string(),
            None,
        )),
        Err(_) => failures.push(failure(
            flow,
            FailureCategory::Protocol,
            "dispose browser context timed out",
            None,
        )),
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

    report(flow, started, artifacts, failures, interrupted)
}

async fn settle_video(page: &Page) {
    let _ = tokio::time::timeout(
        SECONDARY_TIMEOUT,
        page.evaluate(
            "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
        ),
    )
    .await;
    tokio::time::sleep(FINAL_FRAME_DELAY).await;
}

async fn wait_for_cancellation(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

fn is_cancelled(cancellation: Option<&CancellationToken>) -> bool {
    cancellation.is_some_and(CancellationToken::is_cancelled)
}

struct RuntimeState {
    outputs: BTreeMap<String, Resolved<Value>>,
    redactor: Redactor,
    page_settings: PageSettings,
    guard_results: BTreeMap<usize, bool>,
    stopped_loops: BTreeSet<usize>,
}

async fn execute_step(
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
        Operation::Open { url } => navigate(active, url.expose().as_str(), deadline)
            .await
            .map(|_| None),
        Operation::Click { target, position } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, *position, deadline).await?;
            let (x, y) = page_point(active, element.center.x, element.center.y).await?;
            dispatch_click(&active.page, x, y, 1).await.map(|_| None)
        }
        Operation::ClickPoint { point } => {
            dispatch_click(&active.page, f64::from(point.x), f64::from(point.y), 1)
                .await
                .map_err(|mut error| {
                    error.message = format!(
                        "viewport click at ({}, {}) failed: {}",
                        point.x, point.y, error.message
                    );
                    error
                })
                .map(|_| None)
        }
        Operation::DoubleClick { target, position } => {
            let element =
                wait_actionable(active, target, Actionability::CLICK, *position, deadline).await?;
            let (x, y) = page_point(active, element.center.x, element.center.y).await?;
            dispatch_click(&active.page, x, y, 2).await.map(|_| None)
        }
        Operation::Fill { target, value } => {
            let value = resolve_runtime(value, &runtime.outputs)?;
            let element =
                wait_actionable(active, target, Actionability::EDITABLE, None, deadline).await?;
            prepare_fill(&active.page, element.backend_node_id).await?;
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
            erase(&active.page, element.backend_node_id)
                .await
                .map(|_| None)
        }
        Operation::Select { target, value } => {
            let value = resolve_runtime(value, &runtime.outputs)?;
            let element =
                wait_actionable(active, target, Actionability::CLICK, None, deadline).await?;
            select(&active.page, element.backend_node_id, value.expose())
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
            focus(&active.page, element.backend_node_id).await?;
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
            let value = evaluate_page(&active.page, script, &args, save_as.is_some()).await?;
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

async fn step_matches(
    active: &ActiveContext,
    step: &CompiledStep,
    runtime: &mut RuntimeState,
) -> Result<bool, StepError> {
    if active.frame().is_some()
        && !matches!(
            step.operation,
            Operation::SwitchFrame(FrameSwitch::Main | FrameSwitch::Parent)
        )
    {
        verify_frame_origin(active).await?;
    }
    if !guards_match(
        &step.guards,
        &runtime.outputs,
        &mut runtime.guard_results,
        &mut runtime.stopped_loops,
    )? {
        return Ok(false);
    }
    match &step.when {
        Some(predicate) => when_matches(active, predicate, &runtime.outputs).await,
        None => Ok(true),
    }
}

fn guards_match(
    guards: &[crate::flow::Guard],
    outputs: &BTreeMap<String, Resolved<Value>>,
    results: &mut BTreeMap<usize, bool>,
    stopped_loops: &mut BTreeSet<usize>,
) -> Result<bool, StepError> {
    for guard in guards {
        let loop_id = match guard.kind {
            GuardKind::While { loop_id, .. } => Some(loop_id),
            GuardKind::When(_) => None,
        };
        if loop_id.is_some_and(|id| stopped_loops.contains(&id)) {
            return Ok(false);
        }
        let matches = if let Some(matches) = results.get(&guard.id) {
            *matches
        } else {
            let matches = evaluate_expression(guard_expression(&guard.kind), outputs)?;
            results.insert(guard.id, matches);
            matches
        };
        if !matches {
            if let Some(loop_id) = loop_id {
                stopped_loops.insert(loop_id);
            }
            return Ok(false);
        }
    }
    Ok(true)
}

fn guard_expression(guard: &GuardKind) -> &Expression {
    match guard {
        GuardKind::When(expression) | GuardKind::While { expression, .. } => expression,
    }
}

async fn when_matches(
    active: &ActiveContext,
    predicate: &When,
    outputs: &BTreeMap<String, Resolved<Value>>,
) -> Result<bool, StepError> {
    if let When::Expression(expression) = predicate {
        return evaluate_expression(expression, outputs);
    }
    let observation = match predicate {
        When::Visible(locator) | When::Hidden(locator) => active
            .locator()
            .observe_any_visible(locator)
            .await
            .map_err(locator_error)?,
        When::Expression(_) => unreachable!("handled above"),
    };
    let visible = matches!(observation, Observation::Ready(_));
    Ok(match predicate {
        When::Visible(_) => visible,
        When::Hidden(_) => !visible,
        When::Expression(_) => unreachable!("handled above"),
    })
}

fn evaluate_expression(
    expression: &Expression,
    outputs: &BTreeMap<String, Resolved<Value>>,
) -> Result<bool, StepError> {
    match expression {
        Expression::All(children) => {
            for child in children {
                if !evaluate_expression(child, outputs)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expression::Any(children) => {
            for child in children {
                if evaluate_expression(child, outputs)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Expression::Not(child) => Ok(!evaluate_expression(child, outputs)?),
        Expression::Equals(left, right) | Expression::NotEquals(left, right) => {
            let equals = resolve_runtime(left, outputs)?.expose()
                == resolve_runtime(right, outputs)?.expose();
            Ok(if matches!(expression, Expression::Equals(_, _)) {
                equals
            } else {
                !equals
            })
        }
        Expression::Boolean(value) => match resolve_runtime(value, outputs)?.expose().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(StepError::new(
                FailureCategory::Protocol,
                "expression.boolean must resolve to true or false",
            )),
        },
    }
}

fn resolve_runtime(
    value: &crate::flow::RuntimeValue,
    outputs: &BTreeMap<String, Resolved<Value>>,
) -> Result<Resolved<String>, StepError> {
    value
        .resolve(outputs)
        .map_err(|error| StepError::new(FailureCategory::Protocol, error.to_string()))
}

async fn evaluate_page(
    page: &Page,
    script: &str,
    arguments: &[Value],
    capture_result: bool,
) -> Result<Option<Value>, StepError> {
    let function = if capture_result {
        format!("function(...args) {{\n{script}\n}}")
    } else {
        format!(
            "async function(...args) {{ await (async function(...args) {{\n{script}\n}})(...args); }}"
        )
    };
    let params = CallFunctionOnParams::builder()
        .function_declaration(function)
        .arguments(arguments.iter().cloned().map(|value| {
            chromiumoxide::cdp::js_protocol::runtime::CallArgument::builder()
                .value(value)
                .build()
        }))
        .return_by_value(true)
        .await_promise(true)
        .build()
        .map_err(protocol)?;
    match page.evaluate_function(params).await {
        Ok(result) => match result.into_value::<Value>() {
            Ok(value) => Ok(Some(value)),
            Err(_) => Ok(None),
        },
        Err(_) => Err(StepError::new(
            FailureCategory::Protocol,
            "page script failed",
        )),
    }
}

async fn http_request(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, crate::flow::RuntimeValue>,
    body: Option<&crate::flow::RuntimeValue>,
    expected_status: u16,
    save_body: bool,
    outputs: &BTreeMap<String, Resolved<Value>>,
) -> Result<Option<Value>, StepError> {
    let url = url::Url::parse(url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        .ok_or_else(|| StepError::new(FailureCategory::Protocol, "request URL is invalid"))?;
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("compiled HTTP method");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static HTTP client configuration");
    let mut request = client.request(method, url);
    for (name, value) in headers {
        let value = resolve_runtime(value, outputs)?;
        request = request.header(name, value.expose());
    }
    if let Some(body) = body {
        request = request.body(resolve_runtime(body, outputs)?.expose().clone());
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| StepError::new(FailureCategory::Protocol, "HTTP request failed"))?;
    if response.status().as_u16() != expected_status {
        return Err(StepError::assertion(format!(
            "HTTP status was {}, expected {expected_status}",
            response.status().as_u16()
        )));
    }
    if !save_body {
        return Ok(None);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RUNTIME_VALUE_BYTES as u64)
    {
        return Err(StepError::new(
            FailureCategory::Protocol,
            "HTTP response body exceeds the runtime value size limit",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| StepError::new(FailureCategory::Protocol, "HTTP response body failed"))?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_RUNTIME_VALUE_BYTES)
        {
            return Err(StepError::new(
                FailureCategory::Protocol,
                "HTTP response body exceeds the runtime value size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Ok(Some(Value::Null));
    }
    Ok(Some(serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    })))
}

fn store_output(
    outputs: &mut BTreeMap<String, Resolved<Value>>,
    redactor: &mut Redactor,
    name: &str,
    value: Value,
) -> Result<(), StepError> {
    let serialized = serde_json::to_string(&value).map_err(|_| {
        StepError::new(
            FailureCategory::Protocol,
            "runtime output is not JSON-serializable",
        )
    })?;
    if serialized.len() > MAX_RUNTIME_VALUE_BYTES {
        return Err(StepError::new(
            FailureCategory::Protocol,
            "runtime output exceeds the runtime value size limit",
        ));
    }
    redactor.add_secret(serialized);
    register_string_secrets(redactor, &value);
    outputs.insert(name.to_owned(), Resolved::new(value, true));
    Ok(())
}

fn register_string_secrets(redactor: &mut Redactor, value: &Value) {
    match value {
        Value::String(value) => redactor.add_secret(value.clone()),
        Value::Array(values) => {
            for value in values {
                register_string_secrets(redactor, value);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                register_string_secrets(redactor, value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

async fn navigate(active: &ActiveContext, url: &str, deadline: Instant) -> Result<(), StepError> {
    if active.frame().is_some() {
        let parent = parent_frame_url(active).await?;
        if !same_origin_or_inherited(&parent, url) {
            return Err(StepError::new(
                FailureCategory::Navigation,
                "cross-origin iframe navigation is unsupported by chromiumoxide 0.9.1",
            ));
        }
    }
    let tree = active
        .page
        .execute(GetFrameTreeParams::default())
        .await
        .map_err(protocol)?
        .result
        .frame_tree;
    let frame = match active.frame() {
        None => &tree.frame,
        Some(id) => find_frame(&tree, id).ok_or_else(|| {
            StepError::new(FailureCategory::Protocol, "active frame no longer exists")
        })?,
    };
    let target_frame_id = frame.id.clone();
    let previous_loader_id = frame.loader_id.clone();
    let mut started = active
        .page
        .event_listener::<EventFrameStartedNavigating>()
        .await
        .map_err(protocol)?;
    let mut events = active
        .page
        .event_listener::<EventLifecycleEvent>()
        .await
        .map_err(protocol)?;
    let mut params = NavigateParams::new(url);
    params.frame_id = active.frame().cloned();
    let navigation = active
        .page
        .command_future(params)
        .map_err(|error| StepError::new(FailureCategory::Navigation, error.to_string()))?;
    tokio::pin!(navigation);
    let mut navigation_loader = None;
    let mut dom_content_loaded = std::collections::HashSet::new();
    loop {
        let selected = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
            tokio::select! {
                response = &mut navigation => NavigationCompletion::Intercepted(response),
                event = started.next() => NavigationCompletion::Started(event),
                event = events.next() => NavigationCompletion::Lifecycle(event),
            }
        })
        .await
        .map_err(|_| {
            StepError::new(FailureCategory::Timeout, "navigation deadline expired").deadline()
        })?;
        match selected {
            NavigationCompletion::Intercepted(response) => {
                let response = response
                    .map_err(|error| {
                        StepError::new(FailureCategory::Navigation, error.to_string())
                    })?
                    .result;
                if let Some(error) = response.error_text {
                    return Err(StepError::new(FailureCategory::Navigation, error));
                }
                if response.is_download == Some(true) {
                    return Err(StepError::new(
                        FailureCategory::Navigation,
                        "navigation resulted in a download",
                    ));
                }
                if response.frame_id != target_frame_id {
                    return Err(StepError::new(
                        FailureCategory::Protocol,
                        "navigation completed for an unexpected frame",
                    ));
                }
                // chromiumoxide releases this future only for same-document navigation
                // or after its full-load watcher completes.
                return verify_frame_origin(active).await;
            }
            NavigationCompletion::Started(Some(event)) => {
                if event.frame_id == target_frame_id
                    && event.url == url
                    && event.loader_id != previous_loader_id
                {
                    navigation_loader = Some(event.loader_id.clone());
                    if dom_content_loaded.contains(&event.loader_id) {
                        return verify_frame_origin(active).await;
                    }
                }
            }
            NavigationCompletion::Started(None) => {
                return Err(StepError::new(
                    FailureCategory::Protocol,
                    "navigation start event stream closed",
                ));
            }
            NavigationCompletion::Lifecycle(Some(event)) => {
                if event.frame_id == target_frame_id
                    && event.loader_id != previous_loader_id
                    && event.name == "DOMContentLoaded"
                {
                    dom_content_loaded.insert(event.loader_id.clone());
                    if navigation_loader.as_ref() == Some(&event.loader_id) {
                        return verify_frame_origin(active).await;
                    }
                }
            }
            NavigationCompletion::Lifecycle(None) => {
                return Err(StepError::new(
                    FailureCategory::Protocol,
                    "navigation event stream closed",
                ));
            }
        }
    }
}

async fn parent_frame_url(active: &ActiveContext) -> Result<String, StepError> {
    match active.frames.len() {
        0 | 1 => active
            .page
            .url()
            .await
            .map_err(protocol)
            .map(|url| url.unwrap_or_default()),
        length => active
            .page
            .frame_url(active.frames[length - 2].clone())
            .await
            .map_err(protocol)
            .map(|url| url.unwrap_or_default()),
    }
}

async fn verify_frame_origin(active: &ActiveContext) -> Result<(), StepError> {
    let Some(frame) = active.frame() else {
        return Ok(());
    };
    let parent = parent_frame_url(active).await?;
    let child = active
        .page
        .frame_url(frame.clone())
        .await
        .map_err(protocol)?
        .unwrap_or_default();
    if same_origin_or_inherited(&parent, &child) {
        Ok(())
    } else {
        Err(StepError::new(
            FailureCategory::Navigation,
            "cross-origin iframe navigation is unsupported by chromiumoxide 0.9.1",
        ))
    }
}

fn find_frame<'a>(
    tree: &'a chromiumoxide::cdp::browser_protocol::page::FrameTree,
    id: &FrameId,
) -> Option<&'a chromiumoxide::cdp::browser_protocol::page::Frame> {
    if &tree.frame.id == id {
        return Some(&tree.frame);
    }
    tree.child_frames
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find_map(|child| find_frame(child, id))
}

enum NavigationCompletion<C, S, L> {
    Intercepted(C),
    Started(Option<Arc<S>>),
    Lifecycle(Option<Arc<L>>),
}

async fn switch_page(
    host: &BrowserHost,
    context_id: &chromiumoxide::cdp::browser_protocol::browser::BrowserContextId,
    active: &mut ActiveContext,
    destination: &PageSwitch,
    deadline: Instant,
    viewport: Viewport,
    geolocation: Option<Geolocation>,
) -> Result<(), StepError> {
    let page = match destination {
        PageSwitch::Opener => {
            let opener = active.page.opener_id().clone().ok_or_else(|| {
                StepError::new(FailureCategory::Navigation, "active page has no opener")
            })?;
            host.browser().get_page(opener).await.map_err(protocol)?
        }
        PageSwitch::Popup | PageSwitch::Name(_) | PageSwitch::Url(_) => loop {
            let pages = host.browser().pages().await.map_err(protocol)?;
            let targets = host
                .browser()
                .execute(GetTargetsParams::default())
                .await
                .map_err(protocol)?
                .result
                .target_infos;
            let pages = pages
                .into_iter()
                .filter(|page| {
                    targets.iter().any(|target| {
                        target.target_id == *page.target_id()
                            && target.r#type == "page"
                            && target.browser_context_id.as_ref() == Some(context_id)
                    })
                })
                .collect::<Vec<_>>();
            let mut candidates = Vec::new();
            for page in pages {
                let matches = match destination {
                    PageSwitch::Popup => page.opener_id().as_ref() == Some(active.page.target_id()),
                    PageSwitch::Name(expected) => {
                        page.evaluate("window.name")
                            .await
                            .map_err(protocol)?
                            .into_value::<String>()
                            .map_err(protocol)?
                            == *expected.expose()
                    }
                    PageSwitch::Url(expected) => {
                        page.url().await.map_err(protocol)?.as_deref()
                            == Some(expected.expose().as_str())
                    }
                    PageSwitch::Opener => unreachable!("opener handled above"),
                };
                if matches {
                    candidates.push(page);
                }
            }
            match candidates.as_slice() {
                [page] => break page.clone(),
                pages if pages.len() > 1 => {
                    return Err(StepError::new(
                        FailureCategory::Navigation,
                        match destination {
                            PageSwitch::Popup => "active page has multiple popup pages".to_owned(),
                            PageSwitch::Name(name) => {
                                format!("multiple pages match switch_page name {:?}", name.expose())
                            }
                            PageSwitch::Url(url) => format!(
                                "multiple pages match switch_page URL {:?}",
                                url.expose().as_str()
                            ),
                            PageSwitch::Opener => unreachable!("opener handled above"),
                        },
                    ));
                }
                _ if Instant::now() >= deadline => {
                    return Err(StepError::new(
                        FailureCategory::Timeout,
                        match destination {
                            PageSwitch::Popup => {
                                "popup did not open before the step deadline".to_owned()
                            }
                            PageSwitch::Name(name) => format!(
                                "no page named {:?} appeared before the step deadline",
                                name.expose()
                            ),
                            PageSwitch::Url(url) => format!(
                                "no page with URL {:?} appeared before the step deadline",
                                url.expose().as_str()
                            ),
                            PageSwitch::Opener => unreachable!("opener handled above"),
                        },
                    )
                    .deadline());
                }
                _ => sleep_until_poll(deadline).await,
            }
        },
    };
    host.configure_page(&page, viewport, geolocation)
        .await
        .map_err(protocol)?;
    page.activate().await.map_err(protocol)?;
    active.page = page;
    active.frames.clear();
    Ok(())
}

async fn switch_frame(
    active: &mut ActiveContext,
    destination: &FrameSwitch,
    deadline: Instant,
) -> Result<(), StepError> {
    match destination {
        FrameSwitch::Main => active.frames.clear(),
        FrameSwitch::Parent => {
            if active.frames.pop().is_none() {
                return Err(StepError::new(
                    FailureCategory::Navigation,
                    "active frame is already the main frame",
                ));
            }
        }
        FrameSwitch::Target(locator) => {
            let element =
                wait_actionable(active, locator, Actionability::ATTACHED, None, deadline).await?;
            let node = active
                .page
                .execute(
                    DescribeNodeParams::builder()
                        .backend_node_id(element.backend_node_id)
                        .depth(1)
                        .build(),
                )
                .await
                .map_err(protocol)?
                .result
                .node;
            let frame = node.frame_id.ok_or_else(|| {
                StepError::new(
                    FailureCategory::Actionability,
                    "switch_frame target is not an iframe or frame element",
                )
            })?;
            let parent_url = active.url().await.map_err(protocol)?.unwrap_or_default();
            let child_url = active
                .page
                .frame_url(frame.clone())
                .await
                .map_err(protocol)?
                .unwrap_or_default();
            if !same_origin_or_inherited(&parent_url, &child_url) {
                return Err(StepError::new(
                    FailureCategory::Navigation,
                    "cross-origin iframe switching is unsupported by chromiumoxide 0.9.1",
                ));
            }
            if node.content_document.is_none() {
                return Err(StepError::new(
                    FailureCategory::Navigation,
                    "iframe is not available in the page CDP session; cross-origin OOPIF switching is unsupported by chromiumoxide 0.9.1",
                ));
            }
            active.frames.push(frame);
        }
    }
    Ok(())
}

fn same_origin_or_inherited(parent: &str, child: &str) -> bool {
    if matches!(child, "about:blank" | "about:srcdoc") {
        return true;
    }
    match (url::Url::parse(parent), url::Url::parse(child)) {
        (Ok(parent), Ok(child)) => parent.origin() == child.origin(),
        _ => false,
    }
}

async fn wait_actionable(
    active: &ActiveContext,
    locator: &Locator,
    requirements: Actionability,
    action_point: Option<RelativePoint>,
    deadline: Instant,
) -> Result<ResolvedElement, StepError> {
    active
        .locator()
        .wait_unique(locator, requirements, action_point, deadline)
        .await
        .map_err(locator_error)
}

async fn focus(page: &Page, node: BackendNodeId) -> Result<(), StepError> {
    let focused: bool = call_on_node(page, node, FOCUS_FUNCTION, &[]).await?;
    if !focused {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "target could not receive focus",
        ));
    }
    Ok(())
}

async fn prepare_fill(page: &Page, node: BackendNodeId) -> Result<(), StepError> {
    let focused: bool = call_on_node(page, node, PREPARE_FILL_FUNCTION, &[]).await?;
    if !focused {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "target could not be prepared for fill",
        ));
    }
    Ok(())
}

async fn erase(page: &Page, node: BackendNodeId) -> Result<(), StepError> {
    match call_on_node::<String>(page, node, ERASE_FUNCTION, &[])
        .await?
        .as_str()
    {
        "ok" => Ok(()),
        "detached" => Err(StepError::new(
            FailureCategory::Actionability,
            "erase target detached before input dispatch",
        )),
        "focus" => Err(StepError::new(
            FailureCategory::Actionability,
            "erase target could not receive focus",
        )),
        _ => Err(StepError::new(
            FailureCategory::Actionability,
            "erase target is not editable",
        )),
    }
}

async fn select(page: &Page, node: BackendNodeId, value: &str) -> Result<(), StepError> {
    match call_on_node::<String>(
        page,
        node,
        SELECT_FUNCTION,
        &[serde_json::Value::String(value.to_owned())],
    )
    .await?
    .as_str()
    {
        "ok" => Ok(()),
        "detached" => Err(StepError::new(
            FailureCategory::Actionability,
            "select target detached before input dispatch",
        )),
        "focus" => Err(StepError::new(
            FailureCategory::Actionability,
            "select target could not receive focus",
        )),
        "option" => Err(StepError::new(
            FailureCategory::Actionability,
            "select value did not match an option",
        )),
        _ => Err(StepError::new(
            FailureCategory::Actionability,
            "select target is not a native single-value select",
        )),
    }
}

async fn dispatch_scroll(active: &ActiveContext, x: i64, y: i64) -> Result<(), StepError> {
    let [width, height]: [f64; 2] = evaluate_value(active, "[innerWidth, innerHeight]").await?;
    let (center_x, center_y) = page_point(active, width / 2.0, height / 2.0).await?;
    let event = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(center_x)
        .y(center_y)
        .delta_x(x as f64)
        .delta_y(y as f64)
        .build()
        .expect("all mandatory wheel event fields are set");
    active.page.execute(event).await.map_err(protocol)?;
    Ok(())
}

async fn scroll_until_visible(
    active: &ActiveContext,
    target: &Locator,
    x: i32,
    y: i32,
    deadline: Instant,
) -> Result<(), StepError> {
    let engine = active.locator();
    loop {
        let observation = match engine.observe_unique(target, Actionability::VISIBLE).await {
            Ok(Observation::Ready(_)) => return Ok(()),
            Ok(observation) => observation,
            Err(error) if retryable(&error) => Observation::Unavailable {
                message: error.to_string(),
            },
            Err(error) => return Err(locator_error(error)),
        };
        if Instant::now() >= deadline {
            return Err(StepError::new(
                FailureCategory::Timeout,
                "scroll_until_visible deadline expired",
            )
            .deadline()
            .observed(observation.to_string()));
        }
        dispatch_scroll(active, i64::from(x), i64::from(y)).await?;
        sleep_until_poll(deadline).await;
    }
}

async fn dispatch_swipe(
    active: &ActiveContext,
    element: &ResolvedElement,
    x: i32,
    y: i32,
    duration: Duration,
    deadline: Instant,
) -> Result<(), StepError> {
    let end_x = element.center.x + f64::from(x);
    let end_y = element.center.y + f64::from(y);
    let [width, height]: [f64; 2] = evaluate_value(active, "[innerWidth, innerHeight]").await?;
    if end_x < 0.0 || end_y < 0.0 || end_x >= width || end_y >= height {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "swipe endpoint is outside the viewport",
        ));
    }
    require_gesture_time(duration, deadline, "swipe")?;
    let (start_x, start_y) = page_point(active, element.center.x, element.center.y).await?;
    let (end_x, end_y) = page_point(active, end_x, end_y).await?;
    dispatch_pointer(
        &active.page,
        DispatchMouseEventType::MousePressed,
        start_x,
        start_y,
        1,
    )
    .await?;
    tokio::time::sleep(duration).await;
    let moved = dispatch_pointer(
        &active.page,
        DispatchMouseEventType::MouseMoved,
        end_x,
        end_y,
        1,
    )
    .await;
    let released = dispatch_pointer(
        &active.page,
        DispatchMouseEventType::MouseReleased,
        end_x,
        end_y,
        0,
    )
    .await;
    moved.and(released)
}

async fn dispatch_long_press(
    active: &ActiveContext,
    element: &ResolvedElement,
    duration: Duration,
    deadline: Instant,
) -> Result<(), StepError> {
    require_gesture_time(duration, deadline, "long_press")?;
    let (x, y) = page_point(active, element.center.x, element.center.y).await?;
    dispatch_pointer(&active.page, DispatchMouseEventType::MousePressed, x, y, 1).await?;
    tokio::time::sleep(duration).await;
    dispatch_pointer(&active.page, DispatchMouseEventType::MouseReleased, x, y, 0).await
}

fn require_gesture_time(
    duration: Duration,
    deadline: Instant,
    operation: &str,
) -> Result<(), StepError> {
    if Instant::now()
        .checked_add(duration)
        .is_none_or(|finished| finished >= deadline)
    {
        return Err(StepError::new(
            FailureCategory::Timeout,
            format!("{operation} duration exceeds the remaining step deadline"),
        )
        .deadline());
    }
    Ok(())
}

async fn dispatch_pointer(
    page: &Page,
    event_type: DispatchMouseEventType,
    x: f64,
    y: f64,
    buttons: i64,
) -> Result<(), StepError> {
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(event_type)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .buttons(buttons)
            .build()
            .expect("all mandatory pointer event fields are set"),
    )
    .await
    .map_err(protocol)?;
    Ok(())
}

async fn page_point(
    active: &ActiveContext,
    mut x: f64,
    mut y: f64,
) -> Result<(f64, f64), StepError> {
    for frame in &active.frames {
        let owner = active
            .page
            .execute(GetFrameOwnerParams::new(frame.clone()))
            .await
            .map_err(protocol)?
            .result
            .backend_node_id;
        let [offset_x, offset_y]: [f64; 2] =
            call_on_node(&active.page, owner, FRAME_OFFSET_FUNCTION, &[]).await?;
        x += offset_x;
        y += offset_y;
    }
    Ok((x, y))
}

async fn evaluate(active: &ActiveContext, expression: &str) -> Result<(), StepError> {
    evaluate_value::<serde_json::Value>(active, expression)
        .await
        .map(|_| ())
}

async fn evaluate_value<T: DeserializeOwned>(
    active: &ActiveContext,
    expression: &str,
) -> Result<T, StepError> {
    if active.frame().is_none() {
        return active
            .page
            .evaluate(expression)
            .await
            .map_err(protocol)?
            .into_value()
            .map_err(protocol);
    }
    let context = active
        .page
        .frame_execution_context(active.frame().expect("frame checked").clone())
        .await
        .map_err(protocol)?
        .ok_or_else(|| {
            StepError::new(
                FailureCategory::Protocol,
                "active frame has no executable context",
            )
        })?;
    let params = EvaluateParams::builder()
        .expression(expression)
        .context_id(context)
        .return_by_value(true)
        .await_promise(true)
        .build()
        .map_err(protocol)?;
    let response = active.page.execute(params).await.map_err(protocol)?.result;
    if let Some(exception) = response.exception_details {
        return Err(StepError::new(
            FailureCategory::Protocol,
            format!("page expression threw: {}", exception.text),
        ));
    }
    serde_json::from_value(response.result.value.ok_or_else(|| {
        StepError::new(
            FailureCategory::Protocol,
            "page expression returned no value",
        )
    })?)
    .map_err(protocol)
}

async fn navigate_back(page: &Page, deadline: Instant) -> Result<(), StepError> {
    let history = page
        .execute(GetNavigationHistoryParams::default())
        .await
        .map_err(protocol)?
        .result;
    let target_index = history
        .current_index
        .checked_sub(1)
        .ok_or_else(|| StepError::new(FailureCategory::Navigation, "no previous history entry"))?;
    let target = history.entries.get(target_index as usize).ok_or_else(|| {
        StepError::new(
            FailureCategory::Protocol,
            "Chromium navigation history omitted the previous entry",
        )
    })?;

    page.execute(NavigateToHistoryEntryParams::new(target.id))
        .await
        .map_err(|error| StepError::new(FailureCategory::Navigation, error.to_string()))?;

    loop {
        match page.execute(GetNavigationHistoryParams::default()).await {
            Ok(response) if response.result.current_index == target_index => {
                match page.evaluate("document.readyState !== 'loading'").await {
                    Ok(value) => {
                        if value.into_value::<bool>().unwrap_or(false) {
                            return Ok(());
                        }
                    }
                    Err(error) if retryable_cdp_message(&error.to_string()) => {}
                    Err(error) => return Err(protocol(error)),
                }
            }
            Ok(_) => {}
            Err(error) if retryable_cdp_message(&error.to_string()) => {}
            Err(error) => return Err(protocol(error)),
        }
        if Instant::now() >= deadline {
            return Err(StepError::new(
                FailureCategory::Timeout,
                "back navigation deadline expired",
            )
            .deadline());
        }
        sleep_until_poll(deadline).await;
    }
}

async fn dispatch_key(page: &Page, key: &Key, modifiers: &[Modifier]) -> Result<(), StepError> {
    let character = match key {
        Key::Character(character) => Some(character_text(*character, modifiers)),
        Key::Named(_) => None,
    };
    let name = character
        .as_ref()
        .map_or_else(|| key_name(key), |(text, _)| text.clone());
    let definition = get_key_definition(&name);
    if definition.is_none() && !matches!(key, Key::Character(_)) {
        return Err(StepError::new(
            FailureCategory::Protocol,
            format!("Chromium has no key definition for {name:?}"),
        ));
    }
    let code = definition.map_or("", |definition| definition.code);
    let key_code = definition.map_or(0, |definition| definition.key_code);
    let modifier_bits = modifier_mask(modifiers);
    let command = |event_type| {
        DispatchKeyEventParams::builder()
            .r#type(event_type)
            .modifiers(modifier_bits)
            .key(&name)
            .code(code)
            .windows_virtual_key_code(key_code)
            .native_virtual_key_code(key_code)
            .build()
            .expect("all mandatory key event fields are set")
    };
    page.execute(command(DispatchKeyEventType::RawKeyDown))
        .await
        .map_err(protocol)?;

    let character_result = if let Some((text, unmodified_text)) = character
        && !modifiers
            .iter()
            .any(|value| matches!(value, Modifier::Alt | Modifier::Control | Modifier::Meta))
    {
        let mut character_event = command(DispatchKeyEventType::Char);
        character_event.text = Some(text);
        character_event.unmodified_text = Some(unmodified_text);
        page.execute(character_event)
            .await
            .map(|_| ())
            .map_err(protocol)
    } else {
        Ok(())
    };

    let release_result = page
        .execute(command(DispatchKeyEventType::KeyUp))
        .await
        .map(|_| ())
        .map_err(protocol);
    character_result.and(release_result)
}

fn character_text(character: char, modifiers: &[Modifier]) -> (String, String) {
    let unmodified = character.to_string();
    if !modifiers.contains(&Modifier::Shift) {
        return (unmodified.clone(), unmodified);
    }
    let shifted = match character {
        'a'..='z' => character.to_ascii_uppercase(),
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => character,
    };
    (shifted.to_string(), unmodified)
}

async fn dispatch_click(page: &Page, x: f64, y: f64, clicks: i64) -> Result<(), StepError> {
    page.move_mouse(chromiumoxide::layout::Point::new(x, y))
        .await
        .map_err(protocol)?;
    for click_count in 1..=clicks {
        let event = |event_type| {
            DispatchMouseEventParams::builder()
                .r#type(event_type)
                .x(x)
                .y(y)
                .button(MouseButton::Left)
                .click_count(click_count)
                .build()
                .expect("all mandatory mouse event fields are set")
        };
        let press_result = page
            .execute(event(DispatchMouseEventType::MousePressed))
            .await
            .map(|_| ())
            .map_err(protocol);
        let release_result = page
            .execute(event(DispatchMouseEventType::MouseReleased))
            .await
            .map(|_| ())
            .map_err(protocol);
        press_result.and(release_result)?;
    }
    Ok(())
}

async fn assert(
    active: &ActiveContext,
    assertion: &Assertion,
    deadline: Instant,
) -> Result<(), StepError> {
    match assertion {
        Assertion::Visible(locator) => active
            .locator()
            .wait_unique(locator, Actionability::VISIBLE, None, deadline)
            .await
            .map(|_| ())
            .map_err(assertion_locator_error),
        Assertion::Hidden(locator) => assert_hidden(active, locator, deadline).await,
        Assertion::Text {
            target,
            expected,
            match_kind,
        } => assert_text(active, target, expected.expose(), *match_kind, deadline).await,
        Assertion::Url(expectation) => assert_url(active, expectation, deadline).await,
        Assertion::Screenshot(_) => unreachable!("visual assertions are executed with artifacts"),
    }
}

async fn assert_screenshot(
    active: &ActiveContext,
    expectation: &VisualExpectation,
    step: usize,
    artifact_directory: &Path,
) -> Result<(), StepError> {
    let actual_png = screenshot_bytes(active, expectation.crop).await?;
    let baseline = expectation.baseline.clone();
    let comparison_png = actual_png.clone();
    let tolerance = expectation.channel_tolerance;
    let comparison =
        tokio::task::spawn_blocking(move || visual::compare(&baseline, &comparison_png, tolerance))
            .await
            .map_err(|_| protocol("visual comparison task failed"))?
            .map_err(|error| match error {
                visual::VisualError::ActualDecode => protocol(error),
                _ => StepError::assertion(error.to_string()),
            })?;
    if comparison.dimensions_match && comparison.ratio() <= expectation.max_changed_ratio {
        return Ok(());
    }

    let diff_png = visual::encode_png(&comparison.diff).map_err(protocol)?;
    let actual_path = artifact_directory.join(format!("__visual-{step}-actual.png"));
    let diff_path = artifact_directory.join(format!("__visual-{step}-diff.png"));
    let observed = if comparison.dimensions_match {
        format!(
            "{} of {} pixels changed ({:.6}); maximum changed ratio is {:.6}",
            comparison.changed_pixels,
            comparison.total_pixels,
            comparison.ratio(),
            expectation.max_changed_ratio
        )
    } else {
        "baseline and actual dimensions differ".to_owned()
    };
    Err(
        StepError::assertion("visual screenshot assertion did not match")
            .observed(observed)
            .visual_artifacts(actual_path, diff_path, actual_png, diff_png),
    )
}

async fn publish_visual_artifacts(
    artifact_directory: &Path,
    artifacts: &VisualArtifacts,
) -> Result<(), StepError> {
    publish_bytes(
        artifact_directory,
        &artifacts.actual_path,
        &artifacts.actual_png,
    )
    .await?;
    publish_bytes(
        artifact_directory,
        &artifacts.diff_path,
        &artifacts.diff_png,
    )
    .await
}

async fn assert_hidden(
    active: &ActiveContext,
    locator: &Locator,
    deadline: Instant,
) -> Result<(), StepError> {
    loop {
        let observation = match active.locator().observe_any_visible(locator).await {
            Ok(observation) => observation,
            Err(error) if retryable(&error) => Observation::Unavailable {
                message: error.to_string(),
            },
            Err(error) => return Err(assertion_locator_error(error)),
        };
        match observation {
            Observation::NoMatch | Observation::Detached | Observation::Hidden => return Ok(()),
            other => {
                if Instant::now() >= deadline {
                    return Err(StepError::assertion("target remained visible")
                        .deadline()
                        .observed(other.to_string()));
                }
            }
        }
        sleep_until_poll(deadline).await;
    }
}

async fn assert_text(
    active: &ActiveContext,
    locator: &Locator,
    expected: &str,
    match_kind: TextMatch,
    deadline: Instant,
) -> Result<(), StepError> {
    let engine = active.locator();
    loop {
        let observation = match engine.observe_unique(locator, Actionability::VISIBLE).await {
            Ok(observation) => observation,
            Err(error) if retryable(&error) => Observation::Unavailable {
                message: error.to_string(),
            },
            Err(error) => return Err(assertion_locator_error(error)),
        };
        match observation {
            Observation::Ready(element) => {
                let actual: String = match call_on_node(
                    &active.page,
                    element.backend_node_id,
                    INNER_TEXT_FUNCTION,
                    &[],
                )
                .await
                {
                    Ok(actual) => actual,
                    Err(error)
                        if error.category == FailureCategory::Protocol
                            && retryable_cdp_message(&error.message) =>
                    {
                        if Instant::now() >= deadline {
                            return Err(StepError::assertion("text target was unavailable")
                                .deadline()
                                .observed(error.message));
                        }
                        sleep_until_poll(deadline).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if text_matches(&actual, expected, match_kind) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(StepError::assertion("text assertion did not match")
                        .deadline()
                        .observed(format!("expected {expected:?}; text was {actual:?}")));
                }
            }
            observation => {
                if Instant::now() >= deadline {
                    return Err(StepError::assertion("text target was not visible")
                        .deadline()
                        .observed(observation.to_string()));
                }
            }
        }
        sleep_until_poll(deadline).await;
    }
}

async fn assert_url(
    active: &ActiveContext,
    expectation: &UrlExpectation,
    deadline: Instant,
) -> Result<(), StepError> {
    loop {
        let actual = active.url().await.map_err(protocol)?.unwrap_or_default();
        if url_matches(&actual, expectation) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(StepError::assertion("URL assertion did not match")
                .deadline()
                .observed(format!("expected {expectation:?}; URL was {actual:?}")));
        }
        sleep_until_poll(deadline).await;
    }
}

fn url_matches(actual: &str, expectation: &UrlExpectation) -> bool {
    match expectation {
        UrlExpectation::Equals(expected) => actual == expected.expose().as_str(),
        UrlExpectation::Path(expected) => url::Url::parse(actual).is_ok_and(|actual| {
            let mut path = actual.path().to_owned();
            if let Some(query) = actual.query() {
                path.push('?');
                path.push_str(query);
            }
            path == *expected.expose()
        }),
    }
}

async fn sleep_until_poll(deadline: Instant) {
    tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))).await;
}

async fn call_on_node<T: DeserializeOwned>(
    page: &Page,
    node: BackendNodeId,
    function: &str,
    arguments: &[serde_json::Value],
) -> Result<T, StepError> {
    let object = page
        .execute(ResolveNodeParams::builder().backend_node_id(node).build())
        .await
        .map_err(protocol)?
        .result
        .object;
    let object_id = object.object_id.ok_or_else(|| {
        StepError::new(
            FailureCategory::Protocol,
            "resolved DOM node had no object id",
        )
    })?;
    let params = CallFunctionOnParams::builder()
        .function_declaration(function)
        .object_id(object_id.clone())
        .arguments(arguments.iter().cloned().map(|value| {
            chromiumoxide::cdp::js_protocol::runtime::CallArgument::builder()
                .value(value)
                .build()
        }))
        .return_by_value(true)
        .await_promise(false)
        .build()
        .map_err(|error| StepError::new(FailureCategory::Protocol, error))?;
    let response = page.execute(params).await.map_err(protocol)?.result;
    let _ = page.execute(ReleaseObjectParams::new(object_id)).await;
    if let Some(exception) = response.exception_details {
        return Err(StepError::new(
            FailureCategory::Protocol,
            format!("page function threw: {}", exception.text),
        ));
    }
    let value = response.result.value.ok_or_else(|| {
        StepError::new(FailureCategory::Protocol, "page function returned no value")
    })?;
    serde_json::from_value(value)
        .map_err(|error| StepError::new(FailureCategory::Protocol, error.to_string()))
}

struct VideoSession {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<(VideoRecorder, Result<(), String>)>,
    partial_path: PathBuf,
}

enum VideoStartup {
    Ready(Option<VideoSession>),
    Cancelled(Option<Result<Option<PathBuf>, VideoFinishError>>),
}

impl VideoSession {
    async fn finish(
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

fn should_retain_video(flow_failed: bool, screencast_errors: &[String]) -> bool {
    flow_failed || !screencast_errors.is_empty()
}

enum VideoFinishError {
    Complete {
        error: String,
        partial: PathBuf,
        recording: Option<PathBuf>,
    },
}

fn apply_video_finish(
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

async fn start_video(
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
            .quality(80)
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

enum VideoStartAwait<T> {
    Ready(T),
    Cancelled,
    Deadline,
}

async fn await_video_start<T>(
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

fn video_start_cleanup_error(message: &str, cleanup: Option<String>) -> String {
    cleanup.map_or_else(
        || message.to_owned(),
        |cleanup| format!("{message}; cleanup failed: {cleanup}"),
    )
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

async fn capture_failure_screenshot(active: &ActiveContext, directory: &Path) -> Option<PathBuf> {
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

async fn capture_screenshot(
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

async fn screenshot_bytes(
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

async fn publish_bytes(directory: &Path, path: &Path, bytes: &[u8]) -> Result<(), StepError> {
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

async fn step_failure(
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

fn step_context(flow: &CompiledFlow, step: &CompiledStep) -> StepContext {
    let included = step.source != flow.source;
    StepContext {
        number: step.index,
        source: included.then(|| path_text(&step.source)),
        source_step: included.then_some(step.source_index),
        id: step.id.clone(),
        operation: operation_name(&step.operation).to_owned(),
        locator: operation_locator(&step.operation).map(locator_text),
    }
}

fn operation_name(operation: &Operation) -> &'static str {
    match operation {
        Operation::Open { .. } => "open",
        Operation::Click { .. } => "click",
        Operation::ClickPoint { .. } => "click.point",
        Operation::DoubleClick { .. } => "double_click",
        Operation::Fill { .. } => "fill",
        Operation::Erase { .. } => "erase",
        Operation::Select { .. } => "select",
        Operation::Scroll { .. } => "scroll",
        Operation::ScrollUntilVisible { .. } => "scroll_until_visible",
        Operation::Swipe { .. } => "swipe",
        Operation::LongPress { .. } => "long_press",
        Operation::WaitUntilVisible { .. } => "wait_until_visible",
        Operation::WaitUntilStable { .. } => "wait_until_stable",
        Operation::Back => "back",
        Operation::SwitchPage(PageSwitch::Popup) => "switch_page.popup",
        Operation::SwitchPage(PageSwitch::Opener) => "switch_page.opener",
        Operation::SwitchPage(PageSwitch::Name(_)) => "switch_page.name",
        Operation::SwitchPage(PageSwitch::Url(_)) => "switch_page.url",
        Operation::SwitchFrame(FrameSwitch::Target(_)) => "switch_frame.target",
        Operation::SwitchFrame(FrameSwitch::Main) => "switch_frame.main",
        Operation::SwitchFrame(FrameSwitch::Parent) => "switch_frame.parent",
        Operation::Press { .. } => "press",
        Operation::Screenshot { .. } => "screenshot",
        Operation::Recording(RecordingControl::Start) => "recording.start",
        Operation::Recording(RecordingControl::Stop) => "recording.stop",
        Operation::Clear(ClearTarget::Cookies) => "clear.cookies",
        Operation::Clear(ClearTarget::Storage) => "clear.storage",
        Operation::Clear(ClearTarget::Indexeddb) => "clear.indexeddb",
        Operation::Clear(ClearTarget::CacheStorage) => "clear.cache-storage",
        Operation::Clear(ClearTarget::ServiceWorkers) => "clear.service-workers",
        Operation::Evaluate { .. } => "evaluate",
        Operation::Request { .. } => "request",
        Operation::Assert(Assertion::Visible(_)) => "assert.visible",
        Operation::Assert(Assertion::Hidden(_)) => "assert.hidden",
        Operation::Assert(Assertion::Text { .. }) => "assert.text",
        Operation::Assert(Assertion::Url(_)) => "assert.url",
        Operation::Assert(Assertion::Screenshot(_)) => "assert.screenshot",
    }
}

fn operation_locator(operation: &Operation) -> Option<&Locator> {
    match operation {
        Operation::Click { target, .. }
        | Operation::DoubleClick { target, .. }
        | Operation::Fill { target, .. }
        | Operation::Erase { target }
        | Operation::Select { target, .. }
        | Operation::ScrollUntilVisible { target, .. }
        | Operation::Swipe { target, .. }
        | Operation::LongPress { target, .. }
        | Operation::WaitUntilVisible { target }
        | Operation::WaitUntilStable { target }
        | Operation::Press { target, .. }
        | Operation::SwitchFrame(FrameSwitch::Target(target))
        | Operation::Assert(Assertion::Text { target, .. }) => Some(target),
        Operation::Assert(Assertion::Visible(target) | Assertion::Hidden(target)) => Some(target),
        Operation::Open { .. }
        | Operation::ClickPoint { .. }
        | Operation::Scroll { .. }
        | Operation::Back
        | Operation::SwitchPage(_)
        | Operation::SwitchFrame(FrameSwitch::Main | FrameSwitch::Parent)
        | Operation::Screenshot { .. }
        | Operation::Recording(_)
        | Operation::Clear(_)
        | Operation::Evaluate { .. }
        | Operation::Request { .. }
        | Operation::Assert(Assertion::Url(_) | Assertion::Screenshot(_)) => None,
    }
}

fn locator_text(locator: &Locator) -> SafeText {
    locator_text_inner(locator).map_or_else(SafeText::secret, SafeText::public)
}

fn locator_text_inner(locator: &Locator) -> Option<String> {
    let mut text = match &locator.strategy {
        LocatorStrategy::Css(value) if !value.is_secret() => format!("css={:?}", value.expose()),
        LocatorStrategy::TestId(value) if !value.is_secret() => {
            format!("test_id={:?}", value.expose())
        }
        LocatorStrategy::Text { value, .. } if !value.is_secret() => {
            format!("text={:?}", value.expose())
        }
        LocatorStrategy::Label(value) if !value.is_secret() => {
            format!("label={:?}", value.expose())
        }
        LocatorStrategy::Role { value, name } => {
            if value.is_secret() || name.as_ref().is_some_and(|name| name.is_secret()) {
                return None;
            }
            match name {
                Some(name) => format!("role={:?} name={:?}", value.expose(), name.expose()),
                None => format!("role={:?}", value.expose()),
            }
        }
        _ => return None,
    };
    for (name, value) in [
        ("index", locator.index.map(|value| value.to_string())),
        ("checked", locator.checked.map(|value| value.to_string())),
        ("selected", locator.selected.map(|value| value.to_string())),
        ("focused", locator.focused.map(|value| value.to_string())),
        ("enabled", locator.enabled.map(|value| value.to_string())),
    ] {
        if let Some(value) = value {
            text.push_str(&format!(" {name}={value}"));
        }
    }
    for relation in &locator.relations {
        let nested = locator_text_inner(&relation.locator)?;
        text.push_str(&format!(" {}=({nested})", relation_name(relation.kind)));
    }
    Some(text)
}

fn relation_name(relation: RelationKind) -> &'static str {
    match relation {
        RelationKind::Within => "within",
        RelationKind::ChildOf => "child_of",
        RelationKind::Has => "has",
        RelationKind::Above => "above",
        RelationKind::Below => "below",
        RelationKind::Left => "left",
        RelationKind::Right => "right",
    }
}

fn report(
    flow: &CompiledFlow,
    started: Instant,
    artifacts: ArtifactPaths,
    failures: Vec<Failure>,
    interrupted: bool,
) -> FlowReport {
    FlowReport {
        name: flow.name.clone(),
        path: path_text(&flow.source),
        duration_ms: duration_ms(started.elapsed()),
        status: if interrupted {
            FlowStatus::Interrupted
        } else if failures.is_empty() {
            FlowStatus::Passed
        } else {
            FlowStatus::Failed
        },
        failures,
        artifacts,
    }
}

fn failure(
    flow: &CompiledFlow,
    category: FailureCategory,
    message: impl AsRef<str>,
    step: Option<StepContext>,
) -> Failure {
    let mut failure = Failure::new(category, safe(flow, message.as_ref()));
    failure.step = step;
    failure
}

fn safe(flow: &CompiledFlow, value: impl AsRef<str>) -> SafeText {
    SafeText::public(flow.redactor.redact(value.as_ref()))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn deadline_timeout_ms(error: &StepError, timeout: Duration) -> Option<u64> {
    error.deadline_based.then(|| duration_ms(timeout))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn browser_error_category(host: &BrowserHost) -> FailureCategory {
    match host.status() {
        BrowserStatus::Failed(_) | BrowserStatus::Closed => FailureCategory::BrowserCrash,
        BrowserStatus::Running => FailureCategory::Protocol,
    }
}

fn protocol(error: impl fmt::Display) -> StepError {
    StepError::new(FailureCategory::Protocol, error.to_string())
}

fn locator_error(error: LocatorError) -> StepError {
    match error {
        LocatorError::Timeout { last } => {
            let category = match last {
                Observation::NoMatch | Observation::Multiple { .. } => FailureCategory::Locator,
                Observation::Unavailable { .. } => FailureCategory::Protocol,
                _ => FailureCategory::Actionability,
            };
            StepError::new(category, "target did not become actionable")
                .deadline()
                .observed(last.to_string())
        }
        LocatorError::Protocol(message) | LocatorError::InvalidResponse(message) => {
            StepError::new(FailureCategory::Protocol, message)
        }
    }
}

fn assertion_locator_error(error: LocatorError) -> StepError {
    match error {
        LocatorError::Timeout { last } => StepError::assertion("assertion deadline expired")
            .deadline()
            .observed(last.to_string()),
        LocatorError::Protocol(message) | LocatorError::InvalidResponse(message) => {
            StepError::new(FailureCategory::Protocol, message)
        }
    }
}

struct StepError {
    category: FailureCategory,
    message: String,
    last_observed: Option<String>,
    deadline_based: bool,
    visual_artifacts: Option<Box<VisualArtifacts>>,
}

struct VisualArtifacts {
    actual_path: PathBuf,
    diff_path: PathBuf,
    actual_png: Vec<u8>,
    diff_png: Vec<u8>,
}

impl StepError {
    fn new(category: FailureCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            last_observed: None,
            deadline_based: false,
            visual_artifacts: None,
        }
    }

    fn assertion(message: impl Into<String>) -> Self {
        Self::new(FailureCategory::Assertion, message)
    }

    fn observed(mut self, value: impl Into<String>) -> Self {
        self.last_observed = Some(value.into());
        self
    }

    fn deadline(mut self) -> Self {
        self.deadline_based = true;
        self
    }

    fn visual_artifacts(
        mut self,
        actual_path: PathBuf,
        diff_path: PathBuf,
        actual_png: Vec<u8>,
        diff_png: Vec<u8>,
    ) -> Self {
        self.visual_artifacts = Some(Box::new(VisualArtifacts {
            actual_path,
            diff_path,
            actual_png,
            diff_png,
        }));
        self
    }
}

fn modifier_mask(modifiers: &[Modifier]) -> i64 {
    modifiers.iter().fold(0, |mask, modifier| {
        mask | match modifier {
            Modifier::Alt => 1,
            Modifier::Control => 2,
            Modifier::Meta => 4,
            Modifier::Shift => 8,
        }
    })
}

fn key_name(key: &Key) -> String {
    match key {
        Key::Character(character) => character.to_string(),
        Key::Named(named) => match named {
            NamedKey::Enter => "Enter",
            NamedKey::Tab => "Tab",
            NamedKey::Escape => "Escape",
            NamedKey::Space => " ",
            NamedKey::Backspace => "Backspace",
            NamedKey::Delete => "Delete",
            NamedKey::ArrowUp => "ArrowUp",
            NamedKey::ArrowDown => "ArrowDown",
            NamedKey::ArrowLeft => "ArrowLeft",
            NamedKey::ArrowRight => "ArrowRight",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PageUp",
            NamedKey::PageDown => "PageDown",
        }
        .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::env;
    use std::time::Duration;

    use super::*;
    use crate::flow::{compile_file, compile_yaml_with_env};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn modifier_bits_follow_cdp() {
        assert_eq!(modifier_mask(&[]), 0);
        assert_eq!(
            modifier_mask(&[
                Modifier::Alt,
                Modifier::Control,
                Modifier::Meta,
                Modifier::Shift
            ]),
            15
        );
    }

    #[test]
    fn runtime_json_is_compact_secret_and_size_bounded() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - evaluate: { script: 'return 1', save_as: saved }\n  - fill: { target: { css: input }, value: 'prefix-${saved}' }\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let Operation::Fill { value, .. } = &flow.steps[1].operation else {
            panic!("expected fill");
        };
        let outputs = BTreeMap::from([(
            "saved".to_owned(),
            Resolved::new(serde_json::json!({ "token": "canary" }), true),
        )]);
        let resolved =
            resolve_runtime(value, &outputs).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(resolved.expose(), "prefix-{\"token\":\"canary\"}");
        assert!(resolved.is_secret());

        let mut stored = BTreeMap::new();
        let mut redactor = Redactor::default();
        store_output(
            &mut stored,
            &mut redactor,
            "small",
            Value::String("canary".to_owned()),
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(redactor.redact("value=canary"), "value=[REDACTED]");
        assert!(
            store_output(
                &mut stored,
                &mut redactor,
                "large",
                Value::String("x".repeat(MAX_RUNTIME_VALUE_BYTES + 1)),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_expressions_resolve_runtime_json_without_exposing_values() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - evaluate: { script: 'return true', save_as: saved }\n  - when: { expression: { all: [{ boolean: '${saved}' }, { not_equals: { left: x, right: y } }] } }\n    open: https://x.test\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let Some(When::Expression(expression)) = &flow.steps[1].when else {
            panic!("expected expression");
        };
        let outputs =
            BTreeMap::from([("saved".to_owned(), Resolved::new(Value::Bool(true), true))]);
        assert!(matches!(
            evaluate_expression(expression, &outputs),
            Ok(true)
        ));

        let outputs = BTreeMap::from([(
            "saved".to_owned(),
            Resolved::new(Value::String("canary-secret".to_owned()), true),
        )]);
        let message = evaluate_expression(expression, &outputs)
            .unwrap_err()
            .message;
        assert!(!message.contains("canary-secret"));
    }

    #[test]
    fn while_guards_snapshot_subflow_iterations_and_stop_permanently() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.yaml");
        let child = directory.path().join("child.subflow.yaml");
        std::fs::write(
            &root,
            "version: 1\nname: root\nsteps:\n  - evaluate: { script: 'return true', save_as: state }\n  - while: { expression: { boolean: '${state}' }, max_iterations: 3 }\n    run: ./child.subflow.yaml\n",
        )
        .unwrap();
        std::fs::write(
            &child,
            "version: 1\nname: child\nsteps:\n  - open: https://one.test\n  - open: https://two.test\n",
        )
        .unwrap();
        let flow = compile_file(&root, &BTreeMap::new()).unwrap();
        assert_eq!(flow.steps.len(), 7);
        assert_eq!(flow.steps[1].source, child);

        let mut outputs =
            BTreeMap::from([("state".to_owned(), Resolved::new(Value::Bool(true), true))]);
        let mut results = BTreeMap::new();
        let mut stopped = BTreeSet::new();
        assert!(matches!(
            guards_match(&flow.steps[1].guards, &outputs, &mut results, &mut stopped),
            Ok(true)
        ));
        outputs.insert("state".to_owned(), Resolved::new(Value::Bool(false), true));
        assert!(matches!(
            guards_match(&flow.steps[2].guards, &outputs, &mut results, &mut stopped),
            Ok(true)
        ));
        assert!(matches!(
            guards_match(&flow.steps[3].guards, &outputs, &mut results, &mut stopped),
            Ok(false)
        ));
        outputs.insert("state".to_owned(), Resolved::new(Value::Bool(true), true));
        assert!(matches!(
            guards_match(&flow.steps[5].guards, &outputs, &mut results, &mut stopped),
            Ok(false)
        ));
    }

    #[test]
    fn nested_runtime_json_strings_are_redacted_from_urls_and_diagnostics() {
        let mut stored = BTreeMap::new();
        let mut redactor = Redactor::default();
        store_output(
            &mut stored,
            &mut redactor,
            "secret",
            serde_json::json!({
                "auth": { "token": "object-canary" },
                "items": ["array-canary", { "value": "nested-array-canary" }]
            }),
        )
        .unwrap_or_else(|error| panic!("{}", error.message));

        let url = redactor
            .redact("https://example.test/object-canary/array-canary?nested=nested-array-canary");
        let diagnostic = redactor
            .redact("request failed for object-canary, array-canary, and nested-array-canary");
        for canary in ["object-canary", "array-canary", "nested-array-canary"] {
            assert!(!url.contains(canary), "secret leaked in URL: {url}");
            assert!(
                !diagnostic.contains(canary),
                "secret leaked in diagnostic: {diagnostic}"
            );
        }
    }

    #[tokio::test]
    async fn http_requests_do_not_follow_redirects_with_custom_headers() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_url = format!("http://{}/target", target.local_addr().unwrap());
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let length = stream.read(&mut request).await.unwrap();
            request.truncate(length);
            let _ = request_sender.send(String::from_utf8_lossy(&request).into_owned());
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_url = format!("http://{}/redirect", redirect.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = redirect.accept().await.unwrap();
            let mut request = [0; 4096];
            let length = stream.read(&mut request).await.unwrap();
            assert!(length > 0);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - request:\n      method: GET\n      url: http://example.test\n      headers: { x-api-key: redirect-canary }\n      expected_status: 200\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let Operation::Request { headers, .. } = &flow.steps[0].operation else {
            panic!("expected request");
        };
        let response = http_request(
            "GET",
            &redirect_url,
            headers,
            None,
            302,
            false,
            &BTreeMap::new(),
        )
        .await;
        let redirected_request =
            tokio::time::timeout(Duration::from_millis(100), request_receiver).await;

        assert!(
            response.is_ok(),
            "redirect response was not returned: {}",
            response
                .err()
                .map(|error| error.message)
                .unwrap_or_default()
        );
        assert!(
            redirected_request.is_err(),
            "redirect target received x-api-key: {redirected_request:?}"
        );
    }

    #[test]
    fn every_v1_named_key_has_a_chromium_definition() {
        for key in [
            NamedKey::Enter,
            NamedKey::Tab,
            NamedKey::Escape,
            NamedKey::Space,
            NamedKey::Backspace,
            NamedKey::Delete,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
        ] {
            assert!(get_key_definition(key_name(&Key::Named(key))).is_some());
        }
    }

    #[test]
    fn url_expectations_compare_exact_urls_or_path_and_query() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps: [{ assert: { url: { path: '/a?q=1' } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let Operation::Assert(Assertion::Url(expectation)) = &flow.steps[0].operation else {
            panic!("expected URL assertion");
        };
        assert!(url_matches(
            "https://example.test/a?q=1#fragment",
            expectation
        ));
        assert!(!url_matches("https://example.test/a?q=2", expectation));
    }

    #[test]
    fn secret_locators_are_never_rendered_in_step_context() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ click: { target: { css: button, has: { text: '${token}' } } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]),
        )
        .unwrap();
        let context = step_context(&flow, &flow.steps[0]);
        assert_eq!(context.locator.unwrap().as_str(), "[REDACTED]");
    }

    #[test]
    fn public_locator_diagnostics_include_modifiers() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps: [{ click: { target: { css: button, index: 1, checked: false, focused: true, enabled: true, child_of: { test_id: panel } } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let context = step_context(&flow, &flow.steps[0]);
        assert_eq!(
            context.locator.unwrap().as_str(),
            "css=\"button\" index=1 checked=false focused=true enabled=true child_of=(test_id=\"panel\")"
        );
    }

    #[test]
    fn viewport_click_diagnostics_do_not_claim_a_locator() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsettings: { video: off, viewport: { width: 800, height: 600 } }\nsteps: [{ click: { point: { x: 100, y: 200 } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let context = step_context(&flow, &flow.steps[0]);
        assert_eq!(context.operation, "click.point");
        assert!(context.locator.is_none());
    }

    #[test]
    fn double_click_step_context_uses_its_action_name_and_target() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps: [{ double_click: { target: { css: button } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let context = step_context(&flow, &flow.steps[0]);
        assert_eq!(context.operation, "double_click");
        assert_eq!(context.locator.unwrap().as_str(), "css=\"button\"");
    }

    #[test]
    fn interaction_step_contexts_include_only_targeted_locators() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - erase: { target: { css: input } }\n  - select: { target: { css: select }, value: x }\n  - scroll: { y: 1 }\n  - scroll_until_visible: { target: { css: .item }, y: 100 }\n  - swipe: { target: { css: .card }, x: 1 }\n  - long_press: { target: { css: button } }\n  - wait_until_visible: { target: { css: .late } }\n  - wait_until_stable: { target: { css: .moving } }\n  - back: {}\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        for (step, name, locator) in [
            (&flow.steps[0], "erase", Some("css=\"input\"")),
            (&flow.steps[1], "select", Some("css=\"select\"")),
            (&flow.steps[2], "scroll", None),
            (
                &flow.steps[3],
                "scroll_until_visible",
                Some("css=\".item\""),
            ),
            (&flow.steps[4], "swipe", Some("css=\".card\"")),
            (&flow.steps[5], "long_press", Some("css=\"button\"")),
            (&flow.steps[6], "wait_until_visible", Some("css=\".late\"")),
            (&flow.steps[7], "wait_until_stable", Some("css=\".moving\"")),
            (&flow.steps[8], "back", None),
        ] {
            let context = step_context(&flow, step);
            assert_eq!(context.operation, name);
            assert_eq!(context.locator.as_ref().map(SafeText::as_str), locator);
        }
    }

    #[test]
    fn included_step_context_preserves_child_source_and_local_number() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.yaml");
        let child = directory.path().join("child.subflow.yaml");
        std::fs::write(
            &root,
            "version: 1\nname: root\nsettings: { video: off }\nsteps: [{ run: ./child.subflow.yaml }]\n",
        )
        .unwrap();
        std::fs::write(
            &child,
            "version: 1\nname: child\nsteps:\n  - open: https://example.test\n  - assert: { visible: { css: missing } }\n",
        )
        .unwrap();
        let flow = crate::flow::compile_file(&root, &BTreeMap::new()).unwrap();

        let context = step_context(&flow, &flow.steps[1]);

        assert_eq!(context.number, 2);
        assert_eq!(context.source_step, Some(2));
        assert!(
            context
                .source
                .as_deref()
                .is_some_and(|source| source.ends_with("child.subflow.yaml"))
        );
    }

    #[test]
    fn deadline_based_failures_include_timeout_for_all_automation_categories() {
        for error in [
            locator_error(LocatorError::Timeout {
                last: Observation::NoMatch,
            }),
            locator_error(LocatorError::Timeout {
                last: Observation::Hidden,
            }),
            assertion_locator_error(LocatorError::Timeout {
                last: Observation::NoMatch,
            }),
        ] {
            assert_eq!(
                deadline_timeout_ms(&error, Duration::from_millis(321)),
                Some(321)
            );
        }
    }

    #[test]
    fn report_preserves_failure_order_and_uses_infrastructure_precedence() {
        let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps: [{ open: https://x.test }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let failures = vec![
            failure(&flow, FailureCategory::Assertion, "automation", None),
            failure(&flow, FailureCategory::Recording, "cleanup", None),
        ];
        let report = report(
            &flow,
            Instant::now(),
            ArtifactPaths::default(),
            failures,
            false,
        );

        assert_eq!(report.failures[0].category, FailureCategory::Assertion);
        assert_eq!(report.failures[1].category, FailureCategory::Recording);
        assert_eq!(report.exit_code(), crate::report::ExitCode::Infrastructure);
    }

    #[test]
    fn shifted_character_events_use_shifted_and_unmodified_text() {
        assert_eq!(
            character_text('a', &[Modifier::Shift]),
            ("A".to_owned(), "a".to_owned())
        );
        assert_eq!(
            character_text('1', &[Modifier::Shift]),
            ("!".to_owned(), "1".to_owned())
        );
        assert_eq!(character_text('a', &[]), ("a".to_owned(), "a".to_owned()));
    }

    #[test]
    fn fill_clears_text_controls_without_unsupported_select_or_change_events() {
        assert!(PREPARE_FILL_FUNCTION.contains("HTMLInputElement.prototype, 'value'"));
        assert!(PREPARE_FILL_FUNCTION.contains("HTMLTextAreaElement.prototype, 'value'"));
        assert!(PREPARE_FILL_FUNCTION.contains("this.isContentEditable"));
        assert!(PREPARE_FILL_FUNCTION.contains("range.selectNodeContents(this)"));
        assert!(!PREPARE_FILL_FUNCTION.contains("this.select()"));
        assert!(!PREPARE_FILL_FUNCTION.contains("dispatchEvent"));
    }

    #[test]
    fn erase_and_select_dispatch_native_form_events_once() {
        assert_eq!(ERASE_FUNCTION.matches("dispatchEvent").count(), 2);
        assert!(ERASE_FUNCTION.contains("HTMLInputElement.prototype, 'value'"));
        assert!(ERASE_FUNCTION.contains("this.replaceChildren()"));
        assert_eq!(SELECT_FUNCTION.matches("dispatchEvent").count(), 2);
        assert!(SELECT_FUNCTION.contains("this instanceof HTMLSelectElement"));
        assert!(SELECT_FUNCTION.contains("this.multiple"));
        assert!(SELECT_FUNCTION.contains("option.value === value"));
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
    async fn fill_replaces_all_supported_text_controls_in_chrome() {
        let chrome = env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME");
        let host = BrowserHost::launch(chrome, false).await.unwrap();
        let context = host
            .create_context(Viewport::new(800, 600).unwrap(), None)
            .await
            .unwrap();
        let page = context.page().clone();
        page.set_content(
            r#"<input id="text" value="old"><input id="search" type="search" value="old">
                <input id="email" type="email" value="old@example.test">
                <input id="url" type="url" value="https://old.test"><input id="tel" type="tel" value="old">
                <input id="password" type="password" value="old"><textarea id="textarea">old</textarea>
                <div id="editable" contenteditable>old</div>"#,
        )
        .await
        .unwrap();

        for id in [
            "text", "search", "email", "url", "tel", "password", "textarea", "editable",
        ] {
            let element = page.find_element(format!("#{id}")).await.unwrap();
            prepare_fill(&page, element.backend_node_id)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            page.execute(InsertTextParams::new("replacement"))
                .await
                .unwrap();
            let value: String = call_on_node(
                &page,
                element.backend_node_id,
                "function() { return this.isContentEditable ? this.innerText : this.value; }",
                &[],
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(value, "replacement", "failed to replace #{id}");
        }

        host.dispose_context(context).await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
    async fn labels_resolve_native_wrapping_and_aria_names_in_chrome() {
        let chrome = env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME");
        let host = BrowserHost::launch(chrome, false).await.unwrap();
        let context = host
            .create_context(Viewport::new(800, 600).unwrap(), None)
            .await
            .unwrap();
        let page = context.page().clone();
        page.set_content(
            r#"<style>label { text-transform: uppercase }</style>
                <label>Email <input id="wrapped"></label>
                <label>Ignored aria <input id="aria" aria-label="Alias"></label>
                <span id="account">Account</span><span id="owner"> owner</span>
                <label>Ignored labelled <input id="labelled" aria-labelledby="account owner"></label>"#,
        )
        .await
        .unwrap();
        let flow = compile_yaml_with_env(
            r#"version: 1
name: labels
steps:
  - fill: { target: { label: Email }, value: wrapped }
  - fill: { target: { label: Alias }, value: aria }
  - fill: { target: { label: Account owner }, value: labelled }
  - assert: { hidden: { label: Ignored aria } }
  - assert: { hidden: { label: Ignored labelled } }
  - assert: { hidden: { label: email } }
"#,
            "labels.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let mut active = ActiveContext::new(page.clone());

        let mut runtime = RuntimeState {
            outputs: BTreeMap::new(),
            redactor: flow.redactor.clone(),
            page_settings: PageSettings {
                viewport: Viewport::new(800, 600).unwrap(),
                geolocation: None,
            },
            guard_results: BTreeMap::new(),
            stopped_loops: BTreeSet::new(),
        };
        for step in &flow.steps {
            execute_step(
                &host,
                context.id(),
                &mut active,
                step,
                Instant::now() + Duration::from_secs(2),
                Path::new("."),
                &mut runtime,
            )
            .await
            .unwrap_or_else(|error| panic!("{}: {:?}", error.message, error.last_observed));
        }
        for (id, expected) in [
            ("wrapped", "wrapped"),
            ("aria", "aria"),
            ("labelled", "labelled"),
        ] {
            let element = page.find_element(format!("#{id}")).await.unwrap();
            let value: String = call_on_node(
                &page,
                element.backend_node_id,
                "function() { return this.value; }",
                &[],
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(value, expected);
        }

        host.dispose_context(context).await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_token_wakes_existing_and_future_waiters() {
        let token = CancellationToken::new();
        let waiter = tokio::spawn({
            let token = token.clone();
            async move { token.cancelled().await }
        });

        token.cancel();
        waiter.await.unwrap();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn video_start_await_obeys_cancellation_and_deadline() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            await_video_start(
                Some(&cancellation),
                Instant::now() + Duration::from_secs(1),
                std::future::pending::<()>(),
            )
            .await,
            VideoStartAwait::Cancelled
        ));
        assert!(matches!(
            await_video_start(None, Instant::now(), std::future::pending::<()>()).await,
            VideoStartAwait::Deadline
        ));
    }

    #[test]
    fn screencast_errors_retain_failure_only_video() {
        assert!(should_retain_video(false, &["stream failed".to_owned()]));
        assert!(should_retain_video(true, &[]));
        assert!(!should_retain_video(false, &[]));
    }
}
