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

pub(crate) fn resolve_runtime(
    value: &crate::flow::RuntimeValue,
    outputs: &BTreeMap<String, Resolved<Value>>,
) -> Result<Resolved<String>, StepError> {
    value
        .resolve(outputs)
        .map_err(|error| StepError::new(FailureCategory::Protocol, error.to_string()))
}

pub(crate) async fn evaluate_page(
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

pub(crate) fn store_output(
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
    let bare_string = match &value {
        Value::String(value) => Some(value.as_str()),
        _ => None,
    };
    redactor.add_serialized_secret(serialized, bare_string);
    register_string_secrets(redactor, &value);
    outputs.insert(name.to_owned(), Resolved::new(value, true));
    Ok(())
}

pub(crate) fn register_string_secrets(redactor: &mut Redactor, value: &Value) {
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
