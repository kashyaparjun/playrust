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
    RelationKind, RelativePoint, Resolved, TextMatch, UrlExpectation, VideoMode, VisualExpectation,
    When,
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

const SCREENSHOT_NAME: &str = "failure.png";
const RECORDING_NAME: &str = "recording.mp4";
const SECONDARY_TIMEOUT: Duration = Duration::from_secs(2);
const VIDEO_FINALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const FINAL_FRAME_DELAY: Duration = Duration::from_millis(250);
const INSPECT_AX_DEPTH: i64 = 8;
const INSPECT_AX_NODES: usize = 500;
const INSPECT_AX_BYTES: usize = 256 * 1024;
const INSPECT_PAGES: usize = 100;
const INSPECT_TEXT_CHARS: usize = 16 * 1024;
const INSPECTION_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_AX_DEPTH: i64 = 32;
const SNAPSHOT_ELEMENTS: usize = 250;
const SNAPSHOT_TEXT_CHARS: usize = 500;

const SNAPSHOT_NODE_FUNCTION: &str = r#"function() {
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
const FRAME_SIZE_FUNCTION: &str = "function() { return [this.clientWidth, this.clientHeight]; }";

struct ActiveContext {
    page: Page,
    router: Option<Arc<OopifRouter>>,
    frames: Vec<ActiveFrame>,
}

struct ActiveFrame {
    id: FrameId,
}

#[derive(Clone, Copy)]
struct PageSettings {
    viewport: Viewport,
    geolocation: Option<Geolocation>,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionSettings {
    pub timeout: Duration,
    pub viewport: crate::flow::Viewport,
    pub geolocation: Option<Geolocation>,
}

impl SessionSettings {
    pub fn from_flow(flow: &CompiledFlow) -> Self {
        Self {
            timeout: flow.settings.timeout,
            viewport: flow.settings.viewport,
            geolocation: flow.settings.geolocation,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InteractiveStepResult {
    pub url: String,
    pub title: String,
    pub outputs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

#[derive(Debug)]
pub struct InteractiveStepError {
    pub category: FailureCategory,
    pub message: String,
    pub last_observed: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotNodeMetadata {
    test_id: Option<String>,
    id: Option<String>,
    label: Option<String>,
    ancestor_test_id: Option<String>,
    ancestor_id: Option<String>,
    css_path: String,
    visible: bool,
    enabled: bool,
    editable: bool,
    rect: crate::locator::Rect,
}

#[derive(Clone, Copy)]
struct SnapshotTransform {
    origin: (f64, f64),
    horizontal: (f64, f64),
    vertical: (f64, f64),
}

impl ActiveContext {
    fn new(page: Page) -> Self {
        Self {
            page,
            router: None,
            frames: Vec::new(),
        }
    }

    fn with_router(page: Page, router: Arc<OopifRouter>) -> Self {
        let mut active = Self::new(page);
        active.router = Some(router);
        active
    }

    fn frame(&self) -> Option<&FrameId> {
        self.frames.last().map(|frame| &frame.id)
    }

    fn oopif_index(&self) -> Option<usize> {
        let router = self.router.as_deref()?;
        self.frames
            .iter()
            .rposition(|frame| router.has_target(frame.id.as_ref()))
    }

    fn target(&self) -> CdpTarget<'_> {
        self.oopif_index()
            .map_or(CdpTarget::Root(&self.page), |index| {
                CdpTarget::Oopif(
                    self.router.as_deref().expect("OOPIF router missing"),
                    self.frames[index].id.as_ref(),
                )
            })
    }

    fn target_before(&self, frame_index: usize) -> CdpTarget<'_> {
        self.frames[..frame_index]
            .iter()
            .rposition(|frame| {
                self.router
                    .as_deref()
                    .is_some_and(|router| router.has_target(frame.id.as_ref()))
            })
            .map_or(CdpTarget::Root(&self.page), |index| {
                CdpTarget::Oopif(
                    self.router.as_deref().expect("OOPIF router missing"),
                    self.frames[index].id.as_ref(),
                )
            })
    }

    fn local_frame(&self) -> Option<&FrameId> {
        match self.oopif_index() {
            Some(index) if index + 1 == self.frames.len() => None,
            _ => self.frame(),
        }
    }

    fn locator(&self) -> LocatorEngine<'_> {
        LocatorEngine::in_target(self.target(), self.local_frame())
    }

    async fn url(&self) -> anyhow::Result<Option<String>> {
        let tree = self
            .target()
            .execute(GetFrameTreeParams::default())
            .await?
            .frame_tree;
        Ok(match self.local_frame() {
            Some(frame) => find_frame(&tree, frame).map(|frame| frame.url.clone()),
            None => Some(tree.frame.url),
        })
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
    #[cfg(test)]
    step_started_observer: Option<StepStartedObserver>,
}

#[cfg(test)]
#[derive(Clone)]
struct StepStartedObserver(Arc<dyn Fn(&'static str) + Send + Sync>);

#[cfg(test)]
impl fmt::Debug for StepStartedObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StepStartedObserver")
    }
}

impl RunOptions {
    pub fn new(artifact_directory: impl Into<PathBuf>) -> Self {
        Self {
            artifact_directory: artifact_directory.into(),
            ffmpeg_path: None,
            cancellation: None,
            #[cfg(test)]
            step_started_observer: None,
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

/// A persistent isolated browser context with stable page selection and runtime outputs.
pub(crate) struct SessionRuntime {
    context: Option<BrowserContext>,
    active: ActiveContext,
    page_settings: PageSettings,
    outputs: BTreeMap<String, Resolved<Value>>,
    redactor: Redactor,
}

#[derive(Debug, Serialize)]
pub struct SessionPage {
    pub url: String,
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionInspection {
    pub url: String,
    pub title: String,
    pub pages: Vec<SessionPage>,
    pub active_frame: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

impl SessionRuntime {
    pub(crate) async fn open(host: &BrowserHost, flow: &CompiledFlow) -> anyhow::Result<Self> {
        Self::open_settings(
            host,
            SessionSettings::from_flow(flow),
            flow.redactor.clone(),
        )
        .await
    }

    pub(crate) async fn open_settings(
        host: &BrowserHost,
        settings: SessionSettings,
        redactor: Redactor,
    ) -> anyhow::Result<Self> {
        let viewport = Viewport::new(settings.viewport.width, settings.viewport.height)?;
        let context = host.create_context(viewport, settings.geolocation).await?;
        let router =
            match OopifRouter::connect(host.browser().websocket_address(), context.id().as_ref())
                .await
            {
                Ok(router) => router,
                Err(error) => {
                    let _ = host.dispose_context(context).await;
                    return Err(error);
                }
            };
        Ok(Self {
            active: ActiveContext::with_router(context.page().clone(), router),
            context: Some(context),
            page_settings: PageSettings {
                viewport,
                geolocation: settings.geolocation,
            },
            outputs: BTreeMap::new(),
            redactor,
        })
    }

    pub(crate) fn settings_match(&self, flow: &CompiledFlow) -> bool {
        self.page_settings.viewport.width == flow.settings.viewport.width
            && self.page_settings.viewport.height == flow.settings.viewport.height
            && self.page_settings.geolocation == flow.settings.geolocation
    }

    pub(crate) fn page(&self) -> &Page {
        &self.active.page
    }

    pub(crate) fn viewport(&self) -> Viewport {
        self.page_settings.viewport
    }

    pub(crate) async fn capture_agent_snapshot(&self) -> anyhow::Result<CapturedSnapshot> {
        let deadline = Instant::now() + INSPECTION_TIMEOUT;
        loop {
            match self.capture_agent_snapshot_once().await {
                Ok(snapshot) => return Ok(snapshot),
                Err(error)
                    if retryable_cdp_message(&error.to_string()) && Instant::now() < deadline =>
                {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn capture_agent_snapshot_once(&self) -> anyhow::Result<CapturedSnapshot> {
        let mut params = GetFullAxTreeParams::builder().depth(SNAPSHOT_AX_DEPTH);
        if let Some(frame) = self.active.local_frame() {
            params = params.frame_id(frame.clone());
        }
        let mut nodes = self
            .active
            .target()
            .execute(params.build())
            .await?
            .nodes
            .into_iter()
            .filter(snapshot_ax_node)
            .collect::<Vec<_>>();
        let truncated = nodes.len() > SNAPSHOT_ELEMENTS;
        nodes.truncate(SNAPSHOT_ELEMENTS);
        let transform = snapshot_transform(&self.active)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let mut elements = Vec::new();
        for node in nodes {
            let Some(backend) = node.backend_dom_node_id else {
                continue;
            };
            let metadata: SnapshotNodeMetadata =
                match call_on_target(self.active.target(), backend, SNAPSHOT_NODE_FUNCTION, &[])
                    .await
                {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
            let role = ax_text(node.role.as_ref()).unwrap_or("generic");
            let name = bounded_snapshot_text(ax_text(node.name.as_ref()));
            let locator = self
                .snapshot_locator(backend, role, name.as_deref(), &metadata, truncated)
                .await?;
            let identity_value = locator_json(&locator);
            let identity = LocatorIdentity(serde_json::to_string(&identity_value)?);
            elements.push(CapturedElement {
                identity,
                backend_node_id: *backend.inner(),
                parent: None,
                node: SemanticNode {
                    role: role.to_owned(),
                    name: name.map(|value| self.redactor.redact(&value)),
                    value: bounded_snapshot_text(ax_text(node.value.as_ref()))
                        .map(|value| self.redactor.redact(&value)),
                    description: bounded_snapshot_text(ax_text(node.description.as_ref()))
                        .map(|value| self.redactor.redact(&value)),
                    bounds: Some(transform.bounds(metadata.rect)),
                    visible: Some(metadata.visible),
                    state: snapshot_state(&node, &metadata),
                },
            });
        }
        let [x, y, document_width, document_height]: [f64; 4] = evaluate_value(
            &self.active,
            "[scrollX, scrollY, document.documentElement.scrollWidth, document.documentElement.scrollHeight]",
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(CapturedSnapshot {
            viewport: SnapshotViewport {
                width: self.page_settings.viewport.width,
                height: self.page_settings.viewport.height,
            },
            scroll: SnapshotScroll {
                x,
                y,
                document_width,
                document_height,
            },
            elements,
            truncated,
        })
    }

    async fn snapshot_locator(
        &self,
        backend: BackendNodeId,
        role: &str,
        name: Option<&str>,
        metadata: &SnapshotNodeMetadata,
        prefer_css_path: bool,
    ) -> anyhow::Result<Locator> {
        if prefer_css_path
            && !metadata.css_path.is_empty()
            && self.redactor.redact(&metadata.css_path) == metadata.css_path.as_str()
        {
            return Ok(simple_locator(LocatorStrategy::Css(Resolved::new(
                metadata.css_path.clone(),
                false,
            ))));
        }
        let mut candidates = Vec::new();
        if let Some(test_id) = metadata
            .test_id
            .as_deref()
            .filter(|value| !value.is_empty() && self.redactor.redact(value) == *value)
        {
            candidates.push(simple_locator(LocatorStrategy::TestId(Resolved::new(
                test_id.to_owned(),
                false,
            ))));
        }
        if let Some(name) =
            name.filter(|value| !value.is_empty() && self.redactor.redact(value) == *value)
        {
            candidates.push(simple_locator(LocatorStrategy::Role {
                value: Resolved::new(role.to_owned(), false),
                name: Some(Resolved::new(name.to_owned(), false)),
            }));
        }
        if let Some(label) = metadata
            .label
            .as_deref()
            .filter(|value| !value.is_empty() && self.redactor.redact(value) == *value)
        {
            candidates.push(simple_locator(LocatorStrategy::Label(Resolved::new(
                label.to_owned(),
                false,
            ))));
        }
        if let Some(id) = metadata
            .id
            .as_deref()
            .filter(|value| stable_dom_id(value) && self.redactor.redact(value) == *value)
        {
            candidates.push(simple_locator(LocatorStrategy::Css(Resolved::new(
                id_selector(id),
                false,
            ))));
        }
        if !metadata.css_path.is_empty()
            && self.redactor.redact(&metadata.css_path) == metadata.css_path.as_str()
        {
            candidates.push(simple_locator(LocatorStrategy::Css(Resolved::new(
                metadata.css_path.clone(),
                false,
            ))));
        }
        let ancestor = metadata
            .ancestor_test_id
            .as_deref()
            .filter(|value| !value.is_empty() && self.redactor.redact(value) == *value)
            .map(|value| {
                simple_locator(LocatorStrategy::TestId(Resolved::new(
                    value.to_owned(),
                    false,
                )))
            })
            .or_else(|| {
                metadata
                    .ancestor_id
                    .as_deref()
                    .filter(|value| stable_dom_id(value) && self.redactor.redact(value) == *value)
                    .map(|value| {
                        simple_locator(LocatorStrategy::Css(Resolved::new(
                            id_selector(value),
                            false,
                        )))
                    })
            });
        if let Some(ancestor) = ancestor {
            let mut relational = simple_locator(LocatorStrategy::Role {
                value: Resolved::new(role.to_owned(), false),
                name: name.map(|value| Resolved::new(value.to_owned(), false)),
            });
            relational.relations.push(crate::flow::LocatorRelation {
                kind: RelationKind::Within,
                locator: Box::new(ancestor),
            });
            candidates.push(relational);
        }
        candidates.push(simple_locator(LocatorStrategy::Role {
            value: Resolved::new(role.to_owned(), false),
            name: None,
        }));
        for locator in &candidates {
            let resolved = self.active.locator().resolve_all(locator).await?;
            if resolved.backend_node_ids.as_slice() == [backend] {
                return Ok(locator.clone());
            }
        }
        let mut locator = candidates.pop().expect("role candidate exists");
        let all = self.active.locator().resolve_all(&locator).await?;
        locator.index = all
            .backend_node_ids
            .iter()
            .position(|node| *node == backend);
        Ok(locator)
    }

    pub(crate) async fn execute_interactive<F>(
        &mut self,
        host: &BrowserHost,
        flow: &CompiledFlow,
        artifact_directory: &Path,
        dialog_pending: F,
    ) -> Result<InteractiveStepResult, InteractiveStepError>
    where
        F: Future<Output = ()>,
    {
        let step = flow.steps.first().expect("interactive flow has one step");
        let context_id = self
            .context
            .as_ref()
            .expect("open session context")
            .id()
            .clone();
        let placeholder = ActiveContext::new(self.active.page.clone());
        let mut active = std::mem::replace(&mut self.active, placeholder);
        let mut redactor = std::mem::take(&mut self.redactor);
        redactor.extend(&flow.redactor);
        let mut runtime = RuntimeState {
            outputs: std::mem::take(&mut self.outputs),
            redactor,
            page_settings: self.page_settings,
            guard_results: BTreeMap::new(),
            stopped_loops: BTreeSet::new(),
            expects_dialog: false,
            dialog_listener: None,
            presentation_overlays: PresentationOverlays::default(),
            presentation_overlay_recording: false,
        };
        let deadline = Instant::now()
            .checked_add(step.timeout)
            .unwrap_or_else(Instant::now);
        let result = tokio::select! {
            biased;
            _ = dialog_pending => Ok(None),
            result = execute_step(
                host,
                &context_id,
                &mut active,
                step,
                deadline,
                artifact_directory,
                &mut runtime,
            ) => result,
        };
        self.active = active;
        self.outputs = runtime.outputs;
        self.redactor = runtime.redactor;
        match result {
            Ok(artifact) => Ok(InteractiveStepResult {
                url: String::new(),
                title: String::new(),
                outputs: self.output_names().into_iter().collect(),
                artifact: artifact.as_deref().map(path_text),
            }),
            Err(error) => Err(InteractiveStepError {
                category: error.category,
                message: self.redactor.redact(&error.message),
                last_observed: error
                    .last_observed
                    .map(|value| self.redactor.redact(&value)),
            }),
        }
    }

    pub(crate) async fn current_url_title(&self) -> anyhow::Result<(String, String)> {
        let url = self.active.url().await?.unwrap_or_default();
        let title = evaluate_value::<String>(&self.active, "document.title")
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        Ok((self.redactor.redact(&url), self.redactor.redact(&title)))
    }

    pub(crate) async fn scroll_position(&self) -> anyhow::Result<SnapshotScroll> {
        let [x, y, document_width, document_height]: [f64; 4] = evaluate_value(
            &self.active,
            "[scrollX, scrollY, document.documentElement.scrollWidth, document.documentElement.scrollHeight]",
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(SnapshotScroll {
            x,
            y,
            document_width,
            document_height,
        })
    }

    pub(crate) async fn reference_matches(
        &self,
        flow: &CompiledFlow,
        backend_node_id: i64,
    ) -> anyhow::Result<bool> {
        let locator = operation_locator(&flow.steps[0].operation)
            .ok_or_else(|| anyhow::anyhow!("interactive reference action has no locator"))?;
        let resolved = self.active.locator().resolve_all(locator).await?;
        Ok(resolved.backend_node_ids.as_slice() == [BackendNodeId::new(backend_node_id)])
    }

    pub(crate) async fn capture_agent_screenshot(
        &self,
        directory: &Path,
        file_name: &str,
        full_page: bool,
    ) -> anyhow::Result<PathBuf> {
        let bytes = if full_page && self.active.frame().is_none() {
            self.active
                .page
                .screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .full_page(true)
                        .build(),
                )
                .await?
        } else {
            screenshot_bytes(&self.active, None)
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?
        };
        let path = directory.join(file_name);
        publish_bytes(directory, &path, &bytes)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(path)
    }

    pub(crate) fn output(&self, name: &str) -> Option<&Value> {
        self.outputs.get(name).map(Resolved::expose)
    }

    pub(crate) fn output_names(&self) -> BTreeSet<String> {
        self.outputs.keys().cloned().collect()
    }

    pub(crate) fn redact(&self, value: &str) -> String {
        self.redactor.redact(value)
    }

    pub(crate) async fn inspect(
        &self,
        host: &BrowserHost,
        accessibility: bool,
        screenshot_directory: Option<&Path>,
    ) -> anyhow::Result<SessionInspection> {
        let mut status = host.subscribe_status();
        tokio::select! {
            result = tokio::time::timeout(
                INSPECTION_TIMEOUT,
                self.inspect_inner(host, accessibility, screenshot_directory),
            ) => result.map_err(|_| anyhow::anyhow!("session inspection timed out"))?,
            error = browser_unavailable(&mut status) => Err(anyhow::anyhow!(error)),
        }
    }

    async fn inspect_inner(
        &self,
        host: &BrowserHost,
        accessibility: bool,
        screenshot_directory: Option<&Path>,
    ) -> anyhow::Result<SessionInspection> {
        let context = self.context.as_ref().expect("open session context");
        let target_id = self.active.page.target_id();
        let targets = host
            .browser()
            .execute(GetTargetsParams::default())
            .await?
            .result
            .target_infos;
        let pages = targets
            .into_iter()
            .filter(|target| {
                target.r#type == "page" && target.browser_context_id.as_ref() == Some(context.id())
            })
            .take(INSPECT_PAGES)
            .map(|target| SessionPage {
                active: &target.target_id == target_id,
                url: bounded_inspection_text(target.url),
                title: bounded_inspection_text(target.title),
            })
            .collect();
        let url = bounded_inspection_text(self.active.url().await?.unwrap_or_default());
        let title = bounded_inspection_text(
            evaluate_value(&self.active, "document.title")
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?,
        );
        let active_frame = match self.active.frame() {
            Some(_) => self.active.url().await?,
            None => None,
        };
        let accessibility = if accessibility {
            let mut params = GetFullAxTreeParams::builder().depth(INSPECT_AX_DEPTH);
            if let Some(frame) = self.active.local_frame() {
                params = params.frame_id(frame.clone());
            }
            let nodes = self.active.target().execute(params.build()).await?.nodes;
            let mut bounded = Vec::new();
            let mut bytes = 2;
            for node in nodes.into_iter().take(INSPECT_AX_NODES) {
                let node_bytes = serde_json::to_vec(&node)?.len() + 1;
                if bytes + node_bytes > INSPECT_AX_BYTES {
                    break;
                }
                bytes += node_bytes;
                bounded.push(node);
            }
            Some(serde_json::to_value(bounded)?)
        } else {
            None
        };
        let screenshot = match screenshot_directory {
            Some(directory) => Some(path_text(
                &capture_screenshot(&self.active, directory, "inspect.png", None)
                    .await
                    .map_err(|error| anyhow::anyhow!(error.message))?,
            )),
            None => None,
        };
        Ok(SessionInspection {
            url,
            title,
            pages,
            active_frame,
            accessibility,
            screenshot,
        })
    }

    pub(crate) async fn execute(
        &mut self,
        host: &BrowserHost,
        flow: &CompiledFlow,
        options: &RunOptions,
    ) -> FlowReport {
        execute_flow(host, self, flow, options).await
    }

    pub(crate) async fn close(mut self, host: &BrowserHost) -> anyhow::Result<()> {
        let mut errors = Vec::new();
        if let Some(router) = self.active.router.take()
            && let Err(error) = router.close().await
        {
            errors.push(error.to_string());
        }
        let mut status = host.subscribe_status();
        let disposal = tokio::select! {
            result = tokio::time::timeout(
                SECONDARY_TIMEOUT,
                host.dispose_context(self.context.take().expect("open session context")),
            ) => match result {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("dispose browser context timed out")),
            },
            error = browser_unavailable(&mut status) => Err(anyhow::anyhow!(error)),
        };
        if let Err(error) = disposal {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(errors.join("; "))
        }
    }
}

impl SnapshotTransform {
    fn point(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.origin.0 + self.horizontal.0 * x + self.vertical.0 * y,
            self.origin.1 + self.horizontal.1 * x + self.vertical.1 * y,
        )
    }

    fn bounds(self, rect: crate::locator::Rect) -> SnapshotBounds {
        let corners = [
            self.point(rect.x, rect.y),
            self.point(rect.x + rect.width, rect.y),
            self.point(rect.x, rect.y + rect.height),
            self.point(rect.x + rect.width, rect.y + rect.height),
        ];
        let min_x = corners
            .iter()
            .map(|point| point.0)
            .fold(f64::INFINITY, f64::min);
        let max_x = corners
            .iter()
            .map(|point| point.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = corners
            .iter()
            .map(|point| point.1)
            .fold(f64::INFINITY, f64::min);
        let max_y = corners
            .iter()
            .map(|point| point.1)
            .fold(f64::NEG_INFINITY, f64::max);
        SnapshotBounds {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
    }
}

async fn snapshot_transform(active: &ActiveContext) -> Result<SnapshotTransform, StepError> {
    if active.frames.is_empty() {
        return Ok(SnapshotTransform {
            origin: (0.0, 0.0),
            horizontal: (1.0, 0.0),
            vertical: (0.0, 1.0),
        });
    }
    let origin = page_point(active, 0.0, 0.0).await?;
    let horizontal = page_point(active, 1.0, 0.0).await?;
    let vertical = page_point(active, 0.0, 1.0).await?;
    Ok(SnapshotTransform {
        origin,
        horizontal: (horizontal.0 - origin.0, horizontal.1 - origin.1),
        vertical: (vertical.0 - origin.0, vertical.1 - origin.1),
    })
}

fn bounded_inspection_text(value: String) -> String {
    value.chars().take(INSPECT_TEXT_CHARS).collect()
}

fn snapshot_ax_node(node: &AxNode) -> bool {
    if node.ignored || node.backend_dom_node_id.is_none() {
        return false;
    }
    let role = ax_text(node.role.as_ref()).unwrap_or_default();
    matches!(
        role,
        "button"
            | "checkbox"
            | "combobox"
            | "link"
            | "listbox"
            | "menuitem"
            | "option"
            | "radio"
            | "searchbox"
            | "slider"
            | "spinbutton"
            | "switch"
            | "tab"
            | "textbox"
            | "treeitem"
    ) || ax_property(node, AxPropertyName::Focusable)
        .is_some_and(|value| value == &Value::Bool(true))
}

fn ax_text(value: Option<&AxValue>) -> Option<&str> {
    value?.value.as_ref()?.as_str()
}

fn bounded_snapshot_text(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(SNAPSHOT_TEXT_CHARS).collect())
}

fn ax_property(node: &AxNode, name: AxPropertyName) -> Option<&Value> {
    node.properties
        .as_ref()?
        .iter()
        .find(|property| property.name == name)?
        .value
        .value
        .as_ref()
}

fn ax_bool(node: &AxNode, name: AxPropertyName) -> Option<bool> {
    ax_property(node, name).and_then(Value::as_bool)
}

fn snapshot_state(node: &AxNode, metadata: &SnapshotNodeMetadata) -> SemanticState {
    SemanticState {
        enabled: Some(metadata.enabled),
        editable: Some(metadata.editable),
        checked: ax_bool(node, AxPropertyName::Checked),
        selected: ax_bool(node, AxPropertyName::Selected),
        focused: ax_bool(node, AxPropertyName::Focused),
        expanded: ax_bool(node, AxPropertyName::Expanded),
        pressed: ax_bool(node, AxPropertyName::Pressed),
    }
}

fn simple_locator(strategy: LocatorStrategy) -> Locator {
    Locator {
        strategy,
        index: None,
        checked: None,
        selected: None,
        focused: None,
        enabled: None,
        relations: Vec::new(),
    }
}

fn stable_dom_id(value: &str) -> bool {
    !(value.is_empty()
        || value.len() > 100
        || value.chars().any(char::is_whitespace)
        || value.len() >= 8 && value.chars().filter(char::is_ascii_digit).count() >= 6)
}

pub(crate) fn locator_json(locator: &Locator) -> Value {
    let mut object = serde_json::Map::new();
    match &locator.strategy {
        LocatorStrategy::Css(value) => {
            object.insert("css".into(), Value::String(value.expose().clone()));
        }
        LocatorStrategy::TestId(value) => {
            object.insert("test_id".into(), Value::String(value.expose().clone()));
        }
        LocatorStrategy::Text { value, match_kind } => {
            object.insert(
                "text".into(),
                serde_json::json!({
                    "value": value.expose(),
                    "match": match match_kind { TextMatch::Exact => "exact", TextMatch::Contains => "contains" }
                }),
            );
        }
        LocatorStrategy::Label(value) => {
            object.insert("label".into(), Value::String(value.expose().clone()));
        }
        LocatorStrategy::Role { value, name } => {
            object.insert(
                "role".into(),
                serde_json::json!({ "value": value.expose(), "name": name.as_ref().map(Resolved::expose) }),
            );
        }
    }
    for (name, value) in [
        ("index", locator.index.map(|value| serde_json::json!(value))),
        ("checked", locator.checked.map(Value::Bool)),
        ("selected", locator.selected.map(Value::Bool)),
        ("focused", locator.focused.map(Value::Bool)),
        ("enabled", locator.enabled.map(Value::Bool)),
    ] {
        if let Some(value) = value {
            object.insert(name.into(), value);
        }
    }
    for relation in &locator.relations {
        object.insert(
            relation_name(relation.kind).into(),
            locator_json(&relation.locator),
        );
    }
    Value::Object(object)
}

/// Runs one compiled flow in a fresh incognito browser context.
pub async fn run_flow(host: &BrowserHost, flow: &CompiledFlow, options: &RunOptions) -> FlowReport {
    let started = Instant::now();
    let artifacts = ArtifactPaths {
        directory: path_text(&options.artifact_directory),
        ..ArtifactPaths::default()
    };
    if is_cancelled(options.cancellation.as_ref()) {
        return report(flow, started, artifacts, Vec::new(), true);
    }
    let mut session = match SessionRuntime::open(host, flow).await {
        Ok(session) => session,
        Err(error) => {
            let category = browser_error_category(host);
            return report(
                flow,
                started,
                artifacts,
                vec![failure(flow, category, error.to_string(), None)],
                is_cancelled(options.cancellation.as_ref()),
            );
        }
    };
    let mut result = session.execute(host, flow, options).await;
    match tokio::time::timeout(SECONDARY_TIMEOUT * 2, session.close(host)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => result.failures.push(failure(
            flow,
            browser_error_category(host),
            error.to_string(),
            None,
        )),
        Err(_) => result.failures.push(failure(
            flow,
            FailureCategory::Protocol,
            "dispose browser context timed out",
            None,
        )),
    }
    if !result.failures.is_empty() && result.status == FlowStatus::Passed {
        result.status = FlowStatus::Failed;
    }
    result
}

async fn execute_flow(
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
        .expect("open session context")
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

async fn pause_until(duration: Duration, deadline: Instant) -> Result<(), StepError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if duration > remaining {
        tokio::time::sleep(remaining).await;
        return Err(StepError::new(FailureCategory::Timeout, "step deadline expired").deadline());
    }
    tokio::time::sleep(duration).await;
    Ok(())
}

async fn wait_for_cancellation(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn browser_unavailable(status: &mut tokio::sync::watch::Receiver<BrowserStatus>) -> String {
    loop {
        match status.borrow().clone() {
            BrowserStatus::Running => {}
            BrowserStatus::Failed(error) => return format!("Chromium is unavailable: {error}"),
            BrowserStatus::Closed => return "Chromium is closed".to_owned(),
        }
        if status.changed().await.is_err() {
            return "Chromium status monitor stopped".to_owned();
        }
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
    expects_dialog: bool,
    dialog_listener: Option<EventStream<EventJavascriptDialogOpening>>,
    presentation_overlays: PresentationOverlays,
    presentation_overlay_recording: bool,
}

impl RuntimeState {
    fn presentation_overlays_active(&self) -> bool {
        self.presentation_overlay_recording
            && self.presentation_overlays != PresentationOverlays::default()
    }
}

async fn deactivate_presentation_overlay(active: &ActiveContext, runtime: &mut RuntimeState) {
    runtime.presentation_overlay_recording = false;
    let _ = remove_presentation_overlay(active).await;
}

fn step_captures_screenshot(step: &CompiledStep) -> bool {
    matches!(
        step.operation,
        Operation::Screenshot { .. } | Operation::Assert(Assertion::Screenshot(_))
    )
}

async fn update_presentation_overlay(
    active: &ActiveContext,
    step: &CompiledStep,
    overlays: &PresentationOverlays,
    redactor: &Redactor,
) -> Result<(), StepError> {
    let url = if overlays.url {
        redactor.redact(&active.url().await.map_err(protocol)?.unwrap_or_default())
    } else {
        String::new()
    };
    let step_text = if overlays.step {
        redactor.redact(&format!(
            "Step {}{}",
            step.index,
            step.id
                .as_deref()
                .map_or(String::new(), |id| format!(" · {id}"))
        ))
    } else {
        String::new()
    };
    // ponytail: values are JSON-serialized before injection; any new dynamic
    // value must go through serde_json::to_string to stay injection-safe.
    let script = format!(
        r#"(() => {{
            const tag = 'playrust-presentation-overlay';
            let host = document.querySelector(`${{tag}}[data-playrust-presentation-overlay]`);
            if (!host) {{
                if (typeof document.__playrustPresentationOverlayCleanup === 'function') {{
                    document.__playrustPresentationOverlayCleanup();
                }}
                host = document.createElement(tag);
                host.dataset.playrustPresentationOverlay = '';
                host.setAttribute('aria-hidden', 'true');
                host.style.cssText = 'all:initial;display:contents;pointer-events:none';
                const shadow = host.attachShadow({{ mode: 'open' }});
                const style = document.createElement('style');
                style.textContent = `
                    :host, * {{ box-sizing:border-box;pointer-events:none }}
                    #context {{ position:fixed;left:0;right:0;top:0;display:flex;gap:16px;padding:10px 14px;background:rgba(0,0,0,.72);font:600 14px sans-serif;color:white;text-shadow:0 1px 2px #000;white-space:nowrap;overflow:hidden }}
                    #pointer {{ position:fixed;width:18px;height:18px;border:3px solid #ff3b30;border-radius:50%;transform:translate(-50%,-50%);left:50%;top:50% }}
                    [data-marker="click"] {{ position:fixed;width:34px;height:34px;border:4px solid #34c759;border-radius:50%;transform:translate(-50%,-50%);box-shadow:0 0 0 5px rgba(52,199,89,.28) }}
                    [data-marker="scroll"] {{ position:fixed;min-width:44px;padding:8px 12px;border-radius:22px;transform:translate(-50%,-50%);background:#ffd60a;font:700 22px sans-serif;color:#111;text-align:center }}
                `;
                const context = document.createElement('div');
                context.id = 'context';
                const pointer = document.createElement('div');
                pointer.id = 'pointer';
                const markers = document.createElement('div');
                markers.id = 'markers';
                shadow.append(style, context, pointer, markers);

                const showMarker = (kind, x, y, text) => {{
                    markers.querySelector(`[data-marker="${{kind}}"]`)?.remove();
                    const marker = document.createElement('div');
                    marker.dataset.marker = kind;
                    if (Number.isFinite(x)) marker.style.left = `${{x}}px`;
                    if (Number.isFinite(y)) marker.style.top = `${{y}}px`;
                    marker.textContent = text;
                    markers.appendChild(marker);
                }};
                const onMove = event => {{
                    pointer.style.left = `${{event.clientX}}px`;
                    pointer.style.top = `${{event.clientY}}px`;
                }};
                const onPointerDown = event => showMarker('click', event.clientX, event.clientY, '');
                const onWheel = event => showMarker(
                    'scroll',
                    innerWidth / 2,
                    innerHeight / 2,
                    Math.abs(event.deltaY) >= Math.abs(event.deltaX)
                        ? (event.deltaY >= 0 ? '↓' : '↑')
                        : (event.deltaX >= 0 ? '→' : '←')
                );
                if ({pointer}) {{
                    document.addEventListener('pointermove', onMove, true);
                    document.addEventListener('pointerdown', onPointerDown, true);
                    document.addEventListener('wheel', onWheel, true);
                    document.__playrustPresentationOverlayCleanup = () => {{
                        document.removeEventListener('pointermove', onMove, true);
                        document.removeEventListener('pointerdown', onPointerDown, true);
                        document.removeEventListener('wheel', onWheel, true);
                    }};
                }}
                document.documentElement.appendChild(host);
            }}
            const shadow = host.shadowRoot;
            const context = shadow.getElementById('context');
            context.replaceChildren();
            const add = value => {{
                if (!value) return;
                const item = document.createElement('span');
                item.textContent = value;
                context.appendChild(item);
            }};
            add({step});
            add({url});
            shadow.getElementById('pointer').hidden = !{pointer};
        }})()"#,
        step = serde_json::to_string(&step_text).expect("overlay step serializes"),
        url = serde_json::to_string(&url).expect("overlay URL serializes"),
        pointer = overlays.pointer,
    );
    tokio::time::timeout(SECONDARY_TIMEOUT, active.page.evaluate(script))
        .await
        .map_err(|_| protocol("presentation overlay update timed out"))?
        .map_err(protocol)?;
    Ok(())
}

async fn remove_presentation_overlay(active: &ActiveContext) -> Result<(), StepError> {
    tokio::time::timeout(
        SECONDARY_TIMEOUT,
        active.page.evaluate(
            r#"(() => {
                if (typeof document.__playrustPresentationOverlayCleanup === 'function') {
                    document.__playrustPresentationOverlayCleanup();
                    delete document.__playrustPresentationOverlayCleanup;
                }
                document.querySelector('playrust-presentation-overlay[data-playrust-presentation-overlay]')?.remove();
            })()"#,
        ),
    )
    .await
    .map_err(|_| protocol("presentation overlay removal timed out"))?
        .map_err(protocol)?;
    Ok(())
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

async fn step_matches(
    active: &ActiveContext,
    step: &CompiledStep,
    runtime: &mut RuntimeState,
) -> Result<bool, StepError> {
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
    active: &ActiveContext,
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
    let mut params = CallFunctionOnParams::builder()
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
    let target = active.target();
    if let CdpTarget::Oopif(_, _) = target
        && let Some(frame) = active.local_frame()
    {
        params.execution_context_id = Some(ExecutionContextId::new(
            target
                .execution_context(frame.as_ref())
                .ok_or_else(|| protocol("active frame has no executable context"))?,
        ));
    }
    match target {
        CdpTarget::Root(page) => match page.evaluate_function(params).await {
            Ok(result) => Ok(result.into_value::<Value>().ok()),
            Err(CdpError::JavascriptException(_)) => Err(StepError::new(
                FailureCategory::Script,
                "page script failed",
            )),
            Err(error) => Err(protocol(error)),
        },
        CdpTarget::Oopif(_, _) => match target.execute(params).await {
            Ok(result) if result.exception_details.is_some() => Err(StepError::new(
                FailureCategory::Script,
                "page script failed",
            )),
            Ok(result) => Ok(result.result.value),
            Err(error) => Err(protocol(error)),
        },
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
        .ok_or_else(|| StepError::new(FailureCategory::Request, "request URL is invalid"))?;
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
        .map_err(|_| StepError::new(FailureCategory::Request, "HTTP request failed"))?;
    if response.status().as_u16() != expected_status {
        return Err(StepError::new(
            FailureCategory::Request,
            format!(
                "HTTP status was {}, expected {expected_status}",
                response.status().as_u16()
            ),
        ));
    }
    if !save_body {
        return Ok(None);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RUNTIME_VALUE_BYTES as u64)
    {
        return Err(StepError::new(
            FailureCategory::Request,
            "HTTP response body exceeds the runtime value size limit",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| StepError::new(FailureCategory::Request, "HTTP response body failed"))?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_RUNTIME_VALUE_BYTES)
        {
            return Err(StepError::new(
                FailureCategory::Request,
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
    let previous_url = active.url().await.map_err(protocol)?.unwrap_or_default();
    let mut params = NavigateParams::new(url);
    params.frame_id = active.local_frame().cloned();
    let target_frame = active.frame().cloned();
    let target = active.target();
    let navigation = target.execute(params);
    tokio::pin!(navigation);
    let mut command_completed = false;
    loop {
        if !command_completed {
            tokio::select! {
                response = &mut navigation => match response {
                    Ok(response) => {
                        if let Some(error) = response.error_text {
                            return Err(StepError::new(FailureCategory::Navigation, error));
                        }
                        if response.is_download == Some(true) {
                            return Err(StepError::new(
                                FailureCategory::Navigation,
                                "navigation resulted in a download",
                            ));
                        }
                        if let Some(frame) = &target_frame
                            && response.frame_id != *frame
                        {
                            return Err(protocol("navigation completed for an unexpected frame"));
                        }
                        command_completed = true;
                    }
                    Err(error) if target_frame.is_some() && retryable_cdp_message(&error.to_string()) => {
                        command_completed = true;
                    }
                    Err(error) => return Err(StepError::new(
                        FailureCategory::Navigation,
                        error.to_string(),
                    )),
                },
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
        let current_url = match active.url().await {
            Ok(url) => url.unwrap_or_default(),
            Err(error) if retryable_cdp_message(&error.to_string()) => {
                if Instant::now() >= deadline {
                    return Err(StepError::new(
                        FailureCategory::Timeout,
                        "navigation deadline expired",
                    )
                    .deadline());
                }
                sleep_until_poll(deadline).await;
                continue;
            }
            Err(error) => return Err(protocol(error)),
        };
        if command_completed || current_url != previous_url {
            match evaluate_value::<String>(active, "document.readyState").await {
                Ok(state) if state != "loading" => return Ok(()),
                Ok(_) => {}
                Err(error) if retryable_cdp_message(&error.message) => {}
                Err(error) => return Err(error),
            }
        }
        if Instant::now() >= deadline {
            return Err(
                StepError::new(FailureCategory::Timeout, "navigation deadline expired").deadline(),
            );
        }
        sleep_until_poll(deadline).await;
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
                .target()
                .execute(
                    DescribeNodeParams::builder()
                        .backend_node_id(element.backend_node_id)
                        .depth(1)
                        .build(),
                )
                .await
                .map_err(protocol)?
                .node;
            let frame = node.frame_id.ok_or_else(|| {
                StepError::new(
                    FailureCategory::Actionability,
                    "switch_frame target is not an iframe or frame element",
                )
            })?;
            let oopif = node.content_document.is_none();
            if oopif {
                active
                    .router
                    .as_deref()
                    .expect("OOPIF router missing")
                    .wait_for_target(frame.as_ref(), deadline)
                    .await
                    .map_err(protocol)?;
            }
            active.frames.push(ActiveFrame { id: frame });
        }
    }
    Ok(())
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

async fn focus(target: CdpTarget<'_>, node: BackendNodeId) -> Result<(), StepError> {
    let focused: bool = call_on_target(target, node, FOCUS_FUNCTION, &[]).await?;
    if !focused {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "target could not receive focus",
        ));
    }
    Ok(())
}

async fn prepare_fill(target: CdpTarget<'_>, node: BackendNodeId) -> Result<(), StepError> {
    let focused: bool = call_on_target(target, node, PREPARE_FILL_FUNCTION, &[]).await?;
    if !focused {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "target could not be prepared for fill",
        ));
    }
    Ok(())
}

async fn erase(target: CdpTarget<'_>, node: BackendNodeId) -> Result<(), StepError> {
    match call_on_target::<String>(target, node, ERASE_FUNCTION, &[])
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

async fn select(target: CdpTarget<'_>, node: BackendNodeId, value: &str) -> Result<(), StepError> {
    match call_on_target::<String>(
        target,
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
    let Some(mut index) = active.frames.len().checked_sub(1) else {
        return Ok((x, y));
    };
    loop {
        let frame = &active.frames[index];
        let target = active.target_before(index);
        let owner = target
            .execute(GetFrameOwnerParams::new(frame.id.clone()))
            .await
            .map_err(protocol)?
            .backend_node_id;
        let [width, height]: [f64; 2] =
            call_on_target(target, owner, FRAME_SIZE_FUNCTION, &[]).await?;
        let quad = target
            .execute(
                GetContentQuadsParams::builder()
                    .backend_node_id(owner)
                    .build(),
            )
            .await
            .map_err(protocol)?
            .quads
            .into_iter()
            .next()
            .ok_or_else(|| protocol("active frame has no content quad"))?;
        (x, y) = map_frame_point(quad.inner(), width, height, x, y)?;
        let Some(parent_oopif) = active.frames[..index].iter().rposition(|frame| {
            active
                .router
                .as_deref()
                .is_some_and(|router| router.has_target(frame.id.as_ref()))
        }) else {
            break;
        };
        index = parent_oopif;
    }
    Ok((x, y))
}

fn map_frame_point(
    quad: &[f64],
    width: f64,
    height: f64,
    x: f64,
    y: f64,
) -> Result<(f64, f64), StepError> {
    if quad.len() != 8 || width <= 0.0 || height <= 0.0 {
        return Err(protocol("active frame has invalid content geometry"));
    }
    let horizontal = x / width;
    let vertical = y / height;
    Ok((
        quad[0] + (quad[2] - quad[0]) * horizontal + (quad[6] - quad[0]) * vertical,
        quad[1] + (quad[3] - quad[1]) * horizontal + (quad[7] - quad[1]) * vertical,
    ))
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
    if active.local_frame().is_none() {
        let params = EvaluateParams::builder()
            .expression(expression)
            .return_by_value(true)
            .await_promise(true)
            .build()
            .map_err(protocol)?;
        let response = active.target().execute(params).await.map_err(protocol)?;
        if let Some(exception) = response.exception_details {
            return Err(protocol(format!(
                "page expression threw: {}",
                exception.text
            )));
        }
        return serde_json::from_value(
            response
                .result
                .value
                .ok_or_else(|| protocol("page expression returned no value"))?,
        )
        .map_err(protocol);
    }
    if let CdpTarget::Oopif(_, _) = active.target() {
        let frame = active.local_frame().expect("local frame checked");
        let context = active
            .target()
            .execution_context(frame.as_ref())
            .ok_or_else(|| protocol("active frame has no executable context"))?;
        let params = EvaluateParams::builder()
            .expression(expression)
            .context_id(ExecutionContextId::new(context))
            .return_by_value(true)
            .await_promise(true)
            .build()
            .map_err(protocol)?;
        let response = active.target().execute(params).await.map_err(protocol)?;
        if let Some(exception) = response.exception_details {
            return Err(protocol(format!(
                "page expression threw: {}",
                exception.text
            )));
        }
        return serde_json::from_value(
            response
                .result
                .value
                .ok_or_else(|| protocol("page expression returned no value"))?,
        )
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
    let target_index = previous_history_index(history.current_index)?;
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

fn previous_history_index(current_index: i64) -> Result<i64, StepError> {
    current_index
        .checked_sub(1)
        .filter(|index| *index >= 0)
        .ok_or_else(|| StepError::new(FailureCategory::Navigation, "no previous history entry"))
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

async fn dialog_listener<'a>(
    page: &Page,
    runtime: &'a mut RuntimeState,
) -> Result<Option<&'a mut EventStream<EventJavascriptDialogOpening>>, StepError> {
    if !runtime.expects_dialog {
        return Ok(None);
    }
    if runtime.dialog_listener.is_none() {
        runtime.dialog_listener = Some(
            page.event_listener::<EventJavascriptDialogOpening>()
                .await
                .map_err(protocol)?,
        );
    }
    Ok(runtime.dialog_listener.as_mut())
}

async fn dispatch_flow_click(
    page: &Page,
    x: f64,
    y: f64,
    clicks: i64,
    runtime: &mut RuntimeState,
) -> Result<(), StepError> {
    let settle_after_mouse_press =
        runtime.presentation_overlays_active() && runtime.presentation_overlays.pointer;
    let dialogs = dialog_listener(page, runtime).await?;
    dispatch_click(page, x, y, clicks, settle_after_mouse_press, dialogs).await
}

async fn dispatch_click(
    page: &Page,
    x: f64,
    y: f64,
    clicks: i64,
    settle_after_mouse_press: bool,
    mut dialogs: Option<&mut EventStream<EventJavascriptDialogOpening>>,
) -> Result<(), StepError> {
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
        if dispatch_mouse_event(
            page,
            event(DispatchMouseEventType::MousePressed),
            dialogs.as_deref_mut(),
        )
        .await?
        {
            return Ok(());
        }
        if settle_after_mouse_press {
            settle_video(page).await;
        }
        if dispatch_mouse_event(
            page,
            event(DispatchMouseEventType::MouseReleased),
            dialogs.as_deref_mut(),
        )
        .await?
        {
            return Ok(());
        }
    }
    Ok(())
}

async fn dispatch_mouse_event(
    page: &Page,
    event: DispatchMouseEventParams,
    dialogs: Option<&mut EventStream<EventJavascriptDialogOpening>>,
) -> Result<bool, StepError> {
    let mut command = Box::pin(page.execute(event));
    let Some(dialogs) = dialogs else {
        return command.await.map(|_| false).map_err(protocol);
    };
    tokio::select! {
        result = &mut command => result.map(|_| false).map_err(protocol),
        dialog = dialogs.next() => match dialog {
            Some(_) => Ok(true),
            None => command.await.map(|_| false).map_err(protocol),
        },
    }
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
                let actual: String = match call_on_target(
                    active.target(),
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

async fn call_on_target<T: DeserializeOwned>(
    target: CdpTarget<'_>,
    node: BackendNodeId,
    function: &str,
    arguments: &[serde_json::Value],
) -> Result<T, StepError> {
    let object = target
        .execute(ResolveNodeParams::builder().backend_node_id(node).build())
        .await
        .map_err(protocol)?
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
    let response = target.execute(params).await.map_err(protocol)?;
    let _ = target.execute(ReleaseObjectParams::new(object_id)).await;
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

struct ScreencastSource {
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

    fn finish_result(self, path: Option<PathBuf>) -> SessionRecordingFinish {
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
        | Operation::Pause { .. }
        | Operation::Recording(_)
        | Operation::Dialog { .. }
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
mod tests;
