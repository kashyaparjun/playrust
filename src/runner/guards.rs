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

use super::StepError;
use super::context::ActiveContext;
use super::outputs::resolve_runtime;
use super::state::RuntimeState;
use super::{assertion_locator_error, failure, locator_error, path_text, protocol, safe};
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

pub(crate) async fn step_matches(
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

pub(crate) fn guards_match(
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

pub(crate) fn guard_expression(guard: &GuardKind) -> &Expression {
    match guard {
        GuardKind::When(expression) | GuardKind::While { expression, .. } => expression,
    }
}

pub(crate) async fn when_matches(
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

pub(crate) fn evaluate_expression(
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
