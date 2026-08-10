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

use crate::browser::{BrowserContext, BrowserHost, BrowserStatus, Geolocation, Viewport};
use crate::flow::{
    Assertion, ClearTarget, CompiledFlow, CompiledStep, Crop, Expression, FrameSwitch, GuardKind,
    Key, Locator, LocatorStrategy, MAX_RUNTIME_VALUE_BYTES, Modifier, NamedKey,
    NativeDialogResponse, Operation, PageSwitch, PresentationOverlays, RecordingControl, Redactor,
    RelationKind, RelativePoint, Resolved, SettleCondition, TextMatch, UrlExpectation, VideoMode,
    VisualExpectation, When,
};
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

pub(crate) const SCREENSHOT_NAME: &str = "failure.png";
pub(crate) const RECORDING_NAME: &str = "recording.mp4";
pub(crate) const SECONDARY_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const VIDEO_FINALIZE_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const FINAL_FRAME_DELAY: Duration = Duration::from_millis(250);
pub(crate) const INSPECT_AX_DEPTH: i64 = 8;
pub(crate) const INSPECT_AX_NODES: usize = 500;
pub(crate) const INSPECT_AX_BYTES: usize = 256 * 1024;
pub(crate) const INSPECT_PAGES: usize = 100;
pub(crate) const INSPECT_TEXT_CHARS: usize = 16 * 1024;
pub(crate) const INSPECTION_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const SNAPSHOT_AX_DEPTH: i64 = 32;
pub(crate) const SNAPSHOT_ELEMENTS: usize = 250;
pub(crate) const SNAPSHOT_TEXT_CHARS: usize = 500;

pub(crate) const SNAPSHOT_NODE_FUNCTION: &str = r#"function() {
    const rect = this.getBoundingClientRect();
    const style = getComputedStyle(this);
    const visible = this.isConnected && rect.width > 0 && rect.height > 0 &&
        style.visibility !== 'hidden' && style.display !== 'none' && Number(style.opacity) !== 0;
    const disabled = this.matches(':disabled') || this.closest('[inert]') !== null ||
        this.closest('[aria-disabled="true"]') !== null;
    const editable = this.isContentEditable ||
        (this instanceof HTMLTextAreaElement && !this.readOnly && !this.disabled) ||
        (this instanceof HTMLInputElement && !this.readOnly && !this.disabled);
    const path = [];
    for (let current = this; current instanceof Element && path.length < 8; current = current.parentElement) {
        if (current.id && !/\d{6,}/u.test(current.id)) {
            path.unshift(`#${CSS.escape(current.id)}`);
            break;
        }
        const tag = current.tagName.toLowerCase();
        const siblings = current.parentElement
            ? Array.from(current.parentElement.children).filter(child => child.tagName === current.tagName)
            : [];
        path.unshift(siblings.length > 1 ? `${tag}:nth-of-type(${siblings.indexOf(current) + 1})` : tag);
    }
    return {
        testId: this.getAttribute('data-testid'), id: this.id || null,
        label: this.labels ? Array.from(this.labels).map(label => label.innerText.trim()).filter(Boolean).join(' ') || null : null,
        ancestorTestId: this.parentElement?.closest('[data-testid]')?.getAttribute('data-testid') || null,
        ancestorId: this.parentElement?.closest('[id]')?.id || null,
        cssPath: path.join(' > '), visible, enabled: !disabled, editable,
        rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
    };
}"#;

pub(crate) const FOCUS_FUNCTION: &str = r#"function() {
    if (!this.isConnected) return false;
    this.focus();
    return document.activeElement === this;
}"#;

pub(crate) const PREPARE_FILL_FUNCTION: &str = r#"function() {
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

pub(crate) const ERASE_FUNCTION: &str = r#"function() {
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

pub(crate) const SELECT_FUNCTION: &str = r#"function(value) {
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

pub(crate) const INNER_TEXT_FUNCTION: &str = r#"function() { return this.innerText; }"#;
pub(crate) const CLEAR_STORAGE_EXPRESSION: &str =
    "(() => { localStorage.clear(); sessionStorage.clear(); return true; })()";
pub(crate) const CLEAR_INDEXEDDB_EXPRESSION: &str = r#"indexedDB.databases().then(databases => Promise.all(
    databases.filter(database => database.name).map(database => new Promise((resolve, reject) => {
        const request = indexedDB.deleteDatabase(database.name);
        request.onsuccess = () => resolve();
        request.onerror = () => reject(request.error);
        request.onblocked = () => reject(new Error(`IndexedDB database ${database.name} is blocked`));
    }))
))"#;
pub(crate) const CLEAR_CACHE_STORAGE_EXPRESSION: &str =
    "caches.keys().then(names => Promise.all(names.map(name => caches.delete(name))))";
pub(crate) const FRAME_SIZE_FUNCTION: &str =
    "function() { return [this.clientWidth, this.clientHeight]; }";

mod actions;
mod assert;
mod cancel;
mod context;
mod guards;
mod http;
mod interactive;
mod outputs;
mod session;
mod snapshot;
mod state;

pub(crate) use actions::{
    OPEN_SETTLE_DEADLINE_SLACK, call_on_target, character_text, map_frame_point, prepare_fill,
    prepare_open_settle, previous_history_index,
};
pub(crate) use assert::{publish_visual_artifacts, url_matches};
pub use cancel::CancellationToken;
pub(crate) use cancel::{browser_unavailable, is_cancelled, wait_for_cancellation};
pub(crate) use context::PageSettings;
pub(crate) use guards::{evaluate_expression, guards_match};
pub(crate) use http::http_request;
pub use interactive::{InteractiveStepError, InteractiveStepResult, SessionSettings};
pub(crate) use outputs::{resolve_runtime, store_output};
#[cfg(test)]
pub(crate) use session::StepStartedObserver;
pub use session::{RunOptions, SessionInspection, SessionPage, SessionRecordingFinish, run_flow};
pub(crate) use session::{
    SessionRecorder, SessionRuntime, VideoStartAwait, await_video_start, execute_step, pause_until,
    should_retain_video,
};
pub(crate) use snapshot::{SnapshotTransform, simple_locator};
pub(crate) use state::RuntimeState;

pub(crate) async fn settle_video(page: &Page) {
    let _ = tokio::time::timeout(
        SECONDARY_TIMEOUT,
        page.evaluate(
            "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
        ),
    )
    .await;
    tokio::time::sleep(FINAL_FRAME_DELAY).await;
}
use actions::{evaluate_value, page_point};
use context::ActiveContext;

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
pub(crate) fn step_context(flow: &CompiledFlow, step: &CompiledStep) -> StepContext {
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

pub(crate) fn operation_name(operation: &Operation) -> &'static str {
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
        Operation::Pause { .. } => "pause",
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
        Operation::Dialog {
            action: NativeDialogResponse::Accept,
            ..
        } => "dialog.accept",
        Operation::Dialog {
            action: NativeDialogResponse::Dismiss,
            ..
        } => "dialog.dismiss",
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

pub(crate) fn operation_locator(operation: &Operation) -> Option<&Locator> {
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
        Operation::Open {
            settle: Some(SettleCondition::Visible(target) | SettleCondition::Stable(target)),
            ..
        } => Some(target),
        Operation::Open { .. }
        | Operation::ClickPoint { .. }
        | Operation::Scroll { .. }
        | Operation::Back
        | Operation::SwitchPage(_)
        | Operation::SwitchFrame(FrameSwitch::Main | FrameSwitch::Parent)
        | Operation::Screenshot { .. }
        | Operation::Pause { .. }
        | Operation::Recording(_)
        | Operation::Dialog { .. }
        | Operation::Clear(_)
        | Operation::Evaluate { .. }
        | Operation::Request { .. }
        | Operation::Assert(Assertion::Url(_) | Assertion::Screenshot(_)) => None,
    }
}

pub(crate) fn locator_text(locator: &Locator) -> SafeText {
    locator_text_inner(locator).map_or_else(SafeText::secret, SafeText::public)
}

pub(crate) fn locator_text_inner(locator: &Locator) -> Option<String> {
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

pub(crate) fn relation_name(relation: RelationKind) -> &'static str {
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

pub(crate) fn report(
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
        warnings: recording_secret_warnings(flow),
        artifacts,
    }
}

pub(crate) fn recording_secret_warnings(flow: &CompiledFlow) -> Vec<crate::report::SafeText> {
    flow.recording_secret_warning()
        .into_iter()
        .map(crate::report::SafeText::public)
        .collect()
}

pub(crate) fn failure(
    flow: &CompiledFlow,
    category: FailureCategory,
    message: impl AsRef<str>,
    step: Option<StepContext>,
) -> Failure {
    let mut failure = Failure::new(category, safe(flow, message.as_ref()));
    failure.step = step;
    failure
}

pub(crate) fn safe(flow: &CompiledFlow, value: impl AsRef<str>) -> SafeText {
    SafeText::public(flow.redactor.redact(value.as_ref()))
}

pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn deadline_timeout_ms(error: &StepError, timeout: Duration) -> Option<u64> {
    error.deadline_based.then(|| duration_ms(timeout))
}

pub(crate) fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(crate) fn browser_error_category(host: &BrowserHost) -> FailureCategory {
    match host.status() {
        BrowserStatus::Failed(_) | BrowserStatus::Closed => FailureCategory::BrowserCrash,
        BrowserStatus::Running => FailureCategory::Protocol,
    }
}

pub(crate) fn protocol(error: impl fmt::Display) -> StepError {
    StepError::new(FailureCategory::Protocol, error.to_string())
}

pub(crate) fn locator_error(error: LocatorError) -> StepError {
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

pub(crate) fn assertion_locator_error(error: LocatorError) -> StepError {
    match error {
        LocatorError::Timeout { last } => StepError::assertion("assertion deadline expired")
            .deadline()
            .observed(last.to_string()),
        LocatorError::Protocol(message) | LocatorError::InvalidResponse(message) => {
            StepError::new(FailureCategory::Protocol, message)
        }
    }
}

pub(crate) struct StepError {
    category: FailureCategory,
    message: String,
    last_observed: Option<String>,
    deadline_based: bool,
    visual_artifacts: Option<Box<VisualArtifacts>>,
}

pub(crate) struct VisualArtifacts {
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

pub(crate) fn modifier_mask(modifiers: &[Modifier]) -> i64 {
    modifiers.iter().fold(0, |mask, modifier| {
        mask | match modifier {
            Modifier::Alt => 1,
            Modifier::Control => 2,
            Modifier::Meta => 4,
            Modifier::Shift => 8,
        }
    })
}

pub(crate) fn key_name(key: &Key) -> String {
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
mod tests;
