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

use super::super::StepError;
use super::super::context::ActiveContext;
use super::super::context::ActiveFrame;
use super::super::{ERASE_FUNCTION, FOCUS_FUNCTION, PREPARE_FILL_FUNCTION, SELECT_FUNCTION};
use super::super::{assertion_locator_error, failure, locator_error, path_text, protocol, safe};
use super::interaction::evaluate_value;
use super::interaction::{call_on_target, page_point, sleep_until_poll};
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

pub(crate) async fn navigate(
    active: &ActiveContext,
    url: &str,
    deadline: Instant,
) -> Result<(), StepError> {
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

pub(crate) async fn settle_after_open(
    active: &ActiveContext,
    settle: &SettleCondition,
    deadline: Instant,
) -> Result<(), StepError> {
    let (target, requirements) = match settle {
        SettleCondition::Visible(target) => (target, Actionability::VISIBLE),
        SettleCondition::Stable(target) => (target, Actionability::STABLE),
    };
    active
        .locator()
        .wait_unique(target, requirements, None, deadline)
        .await
        .map_err(settle_locator_error)?;
    Ok(())
}

/// Leave headroom so settle timeouts complete inside `timeout_at` instead of
/// being cancelled as a generic `step deadline expired`. When that headroom
/// does not fit in the remaining budget, treat it as settle-budget exhaustion
/// rather than an immediate settle miss.
pub(crate) const OPEN_SETTLE_DEADLINE_SLACK: Duration = Duration::from_millis(50);

pub(crate) fn prepare_open_settle(deadline: Instant) -> Result<Instant, StepError> {
    open_phase_deadline(deadline).ok_or_else(open_settle_budget_error)
}

pub(crate) fn open_phase_deadline(deadline: Instant) -> Option<Instant> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }
    deadline
        .checked_sub(OPEN_SETTLE_DEADLINE_SLACK)
        .filter(|early| *early > now)
}

pub(crate) fn open_settle_budget_error() -> StepError {
    StepError::new(
        FailureCategory::Timeout,
        "navigation completed without enough remaining time for the open settle condition",
    )
    .deadline()
}

pub(crate) fn settle_locator_error(error: LocatorError) -> StepError {
    match error {
        LocatorError::Timeout { last } => {
            let category = match last {
                Observation::NoMatch | Observation::Multiple { .. } => FailureCategory::Locator,
                Observation::Unavailable { .. } => FailureCategory::Protocol,
                _ => FailureCategory::Actionability,
            };
            StepError::new(
                category,
                "open settle condition was not satisfied before the step deadline",
            )
            .deadline()
            .observed(last.to_string())
        }
        LocatorError::Protocol(message) | LocatorError::InvalidResponse(message) => {
            StepError::new(FailureCategory::Protocol, message)
        }
    }
}

pub(crate) fn find_frame<'a>(
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

pub(crate) async fn switch_page(
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

pub(crate) async fn switch_frame(
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

pub(crate) async fn wait_actionable(
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

pub(crate) async fn focus(target: CdpTarget<'_>, node: BackendNodeId) -> Result<(), StepError> {
    let focused: bool = call_on_target(target, node, FOCUS_FUNCTION, &[]).await?;
    if !focused {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "target could not receive focus",
        ));
    }
    Ok(())
}

pub(crate) async fn prepare_fill(
    target: CdpTarget<'_>,
    node: BackendNodeId,
) -> Result<(), StepError> {
    let focused: bool = call_on_target(target, node, PREPARE_FILL_FUNCTION, &[]).await?;
    if !focused {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "target could not be prepared for fill",
        ));
    }
    Ok(())
}

pub(crate) async fn erase(target: CdpTarget<'_>, node: BackendNodeId) -> Result<(), StepError> {
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

pub(crate) async fn select(
    target: CdpTarget<'_>,
    node: BackendNodeId,
    value: &str,
) -> Result<(), StepError> {
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
