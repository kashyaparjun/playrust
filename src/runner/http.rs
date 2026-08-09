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

pub(crate) async fn http_request(
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
