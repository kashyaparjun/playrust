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
use super::{INSPECT_TEXT_CHARS, SNAPSHOT_TEXT_CHARS, page_point, relation_name};
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

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SnapshotNodeMetadata {
    pub(crate) test_id: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) ancestor_test_id: Option<String>,
    pub(crate) ancestor_id: Option<String>,
    pub(crate) css_path: String,
    pub(crate) visible: bool,
    pub(crate) enabled: bool,
    pub(crate) editable: bool,
    pub(crate) rect: crate::locator::Rect,
}

#[derive(Clone, Copy)]
pub(crate) struct SnapshotTransform {
    pub(crate) origin: (f64, f64),
    pub(crate) horizontal: (f64, f64),
    pub(crate) vertical: (f64, f64),
}
impl SnapshotTransform {
    pub(crate) fn point(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.origin.0 + self.horizontal.0 * x + self.vertical.0 * y,
            self.origin.1 + self.horizontal.1 * x + self.vertical.1 * y,
        )
    }

    pub(crate) fn bounds(self, rect: crate::locator::Rect) -> SnapshotBounds {
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

pub(crate) async fn snapshot_transform(
    active: &ActiveContext,
) -> Result<SnapshotTransform, StepError> {
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

pub(crate) fn bounded_inspection_text(value: String) -> String {
    value.chars().take(INSPECT_TEXT_CHARS).collect()
}

pub(crate) fn snapshot_ax_node(node: &AxNode) -> bool {
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

pub(crate) fn ax_text(value: Option<&AxValue>) -> Option<&str> {
    value?.value.as_ref()?.as_str()
}

pub(crate) fn bounded_snapshot_text(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(SNAPSHOT_TEXT_CHARS).collect())
}

pub(crate) fn ax_property(node: &AxNode, name: AxPropertyName) -> Option<&Value> {
    node.properties
        .as_ref()?
        .iter()
        .find(|property| property.name == name)?
        .value
        .value
        .as_ref()
}

pub(crate) fn ax_bool(node: &AxNode, name: AxPropertyName) -> Option<bool> {
    ax_property(node, name).and_then(Value::as_bool)
}

pub(crate) fn snapshot_state(node: &AxNode, metadata: &SnapshotNodeMetadata) -> SemanticState {
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

pub(crate) fn simple_locator(strategy: LocatorStrategy) -> Locator {
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

pub(crate) fn stable_dom_id(value: &str) -> bool {
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
