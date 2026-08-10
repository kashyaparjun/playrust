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
use super::super::{deadline_timeout_ms, operation_locator, settle_video};
use super::RunOptions;
use super::execute::{execute_flow, execute_step};
use super::recorder::{
    VideoStartup, apply_video_finish, capture_failure_screenshot, capture_screenshot,
    publish_bytes, screenshot_bytes, start_video,
};
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
/// A persistent isolated browser context with stable page selection and runtime outputs.
pub(crate) struct SessionRuntime {
    pub(crate) context: Option<BrowserContext>,
    pub(crate) active: ActiveContext,
    pub(crate) page_settings: PageSettings,
    pub(crate) outputs: BTreeMap<String, Resolved<Value>>,
    pub(crate) redactor: Redactor,
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

    pub(crate) async fn capture_agent_snapshot_once(&self) -> anyhow::Result<CapturedSnapshot> {
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

    pub(crate) async fn snapshot_locator(
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
            // Invariant: SessionRuntime is only constructed via open_settings,
            // which stores an open browser context; context is only taken by
            // close(), which consumes the runtime. This method runs before
            // close, so context is always Some.
            .expect("open session context on a live runtime")
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

    pub(crate) async fn inspect_inner(
        &self,
        host: &BrowserHost,
        accessibility: bool,
        screenshot_directory: Option<&Path>,
    ) -> anyhow::Result<SessionInspection> {
        let context = self
            .context
            .as_ref()
            // Invariant: SessionRuntime is only constructed with an open
            // context, and context is only taken by close(), which consumes
            // the runtime. inspect runs on a live session, so context is Some.
            .expect("open session context on a live runtime");
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
                host.dispose_context(self.context.take()
                    // Invariant: this is close(); SessionRuntime is only
                    // constructed with an open context and context is taken
                    // only here, exactly once, on the terminal close path.
                    .expect("close disposes an open session context")),
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
