use std::fmt;
use std::time::{Duration, Instant};

use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxValueNativeSourceType, QueryAxTreeParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, ResolveNodeParams};
use chromiumoxide::cdp::browser_protocol::page::GetFrameTreeParams;
use chromiumoxide::cdp::js_protocol::runtime::{
    CallArgument, CallFunctionOnParams, ReleaseObjectParams,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

use crate::flow::{Locator, LocatorStrategy, TextMatch};

pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

const TEXT_MATCH_FUNCTION: &str = r#"function(expected, exact) {
    const normalize = value => value.replace(/\s+/gu, ' ').trim();
    const visible = element => {
        if (!element.isConnected || element.getClientRects().length === 0) return false;
        const ownStyle = getComputedStyle(element);
        if (ownStyle.visibility === 'hidden' || ownStyle.visibility === 'collapse') return false;
        for (let current = element; current instanceof Element; current = current.parentElement) {
            const style = getComputedStyle(current);
            if (style.display === 'none' || style.contentVisibility === 'hidden' ||
                Number(style.opacity) === 0) return false;
        }
        return true;
    };
    const matches = value => exact ? value === expected : value.includes(expected);
    if (!visible(this) || !matches(normalize(this.innerText))) return false;
    return !Array.from(this.querySelectorAll('*')).some(element =>
        visible(element) && matches(normalize(element.innerText))
    );
}"#;

const FORM_CONTROL_FUNCTION: &str = r#"function() {
    return this instanceof Element &&
        this.matches('button,input:not([type="hidden"]),meter,output,progress,select,textarea');
}"#;

const NATIVE_LABEL_MATCH_FUNCTION: &str = r#"function(expected) {
    const normalize = value => value.replace(/\s+/gu, ' ').trim();
    return 'labels' in this && this.labels !== null &&
        normalize(Array.from(this.labels, label => label.textContent).join(' ')) === expected;
}"#;

const STATE_FILTER_FUNCTION: &str = r#"function(checked, selected, focused) {
    if (checked !== null && (!('checked' in this) || this.checked !== checked)) return false;
    if (selected !== null && (!('selected' in this) || this.selected !== selected)) return false;
    if (focused !== null && (document.activeElement === this) !== focused) return false;
    return true;
}"#;

const ACTIONABILITY_FUNCTION: &str = r#"function(scroll) {
    if (!this.isConnected) return { attached: false };
    const visible = (() => {
        if (this.getClientRects().length === 0) return false;
        const ownStyle = getComputedStyle(this);
        if (ownStyle.visibility === 'hidden' || ownStyle.visibility === 'collapse') return false;
        for (let current = this; current instanceof Element; current = current.parentElement) {
            const style = getComputedStyle(current);
            if (style.display === 'none' || style.contentVisibility === 'hidden' ||
                Number(style.opacity) === 0) return false;
        }
        return true;
    })();
    const disabled = this.matches(':disabled') || this.closest('[inert]') !== null ||
        this.closest('[aria-disabled="true"]') !== null;
    const editable = this.isContentEditable ||
        (this instanceof HTMLTextAreaElement && !this.readOnly && !this.disabled) ||
        (this instanceof HTMLInputElement && !this.readOnly && !this.disabled &&
            ['text','search','email','url','tel','password'].includes(this.type));
    if (scroll && visible) {
        this.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    }
    const rect = this.getBoundingClientRect();
    const center = { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
    const hit = rect.width > 0 && rect.height > 0 &&
        center.x >= 0 && center.y >= 0 && center.x < innerWidth && center.y < innerHeight
        ? document.elementFromPoint(center.x, center.y) : null;
    const covered = hit === null || (hit !== this && !this.contains(hit));
    return {
        attached: true,
        visible,
        enabled: !disabled,
        editable,
        rect: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
        center,
        covered,
        covering: covered && hit !== null
            ? `${hit.tagName.toLowerCase()}${hit.id ? '#' + hit.id : ''}` : null
    };
}"#;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Actionability {
    pub visible: bool,
    pub enabled: bool,
    pub editable: bool,
    pub stable: bool,
    pub hit_test: bool,
}

impl Actionability {
    pub const ATTACHED: Self = Self {
        visible: false,
        enabled: false,
        editable: false,
        stable: false,
        hit_test: false,
    };

    pub const VISIBLE: Self = Self {
        visible: true,
        ..Self::ATTACHED
    };

    pub const CLICK: Self = Self {
        visible: true,
        enabled: true,
        editable: false,
        stable: true,
        hit_test: true,
    };

    pub const EDITABLE: Self = Self {
        editable: true,
        ..Self::CLICK
    };
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedElement {
    pub backend_node_id: BackendNodeId,
    pub rect: Rect,
    pub center: Point,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CandidateSet {
    pub backend_node_ids: Vec<BackendNodeId>,
}

impl CandidateSet {
    pub fn match_observation(&self) -> MatchObservation {
        match self.backend_node_ids.len() {
            0 => MatchObservation::NoMatch,
            1 => MatchObservation::Unique,
            count => MatchObservation::Multiple { count },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchObservation {
    NoMatch,
    Unique,
    Multiple { count: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Observation {
    NoMatch,
    Multiple {
        count: usize,
    },
    Detached,
    Hidden,
    Disabled,
    NonEditable,
    EmptyBox,
    Unstable {
        backend_node_id: BackendNodeId,
        previous: Rect,
        current: Rect,
    },
    Covered {
        covering: Option<String>,
    },
    Unavailable {
        message: String,
    },
    Ready(ResolvedElement),
}

impl fmt::Display for Observation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMatch => formatter.write_str("no match"),
            Self::Multiple { count } => write!(formatter, "{count} matches"),
            Self::Detached => formatter.write_str("detached"),
            Self::Hidden => formatter.write_str("hidden"),
            Self::Disabled => formatter.write_str("disabled"),
            Self::NonEditable => formatter.write_str("non-editable"),
            Self::EmptyBox => formatter.write_str("empty box"),
            Self::Unstable { .. } => formatter.write_str("unstable"),
            Self::Covered { covering } => match covering {
                Some(covering) => write!(formatter, "covered by {covering}"),
                None => formatter.write_str("covered"),
            },
            Self::Unavailable { message } => {
                write!(formatter, "temporarily unavailable: {message}")
            }
            Self::Ready(_) => formatter.write_str("ready"),
        }
    }
}

#[derive(Debug, Error)]
pub enum LocatorError {
    #[error("Chromium locator command failed: {0}")]
    Protocol(String),
    #[error("Chromium returned an invalid locator response: {0}")]
    InvalidResponse(String),
    #[error("locator deadline expired; last observation: {last}")]
    Timeout { last: Observation },
}

pub struct LocatorEngine<'page> {
    page: &'page Page,
}

impl<'page> LocatorEngine<'page> {
    pub fn new(page: &'page Page) -> Self {
        Self { page }
    }

    pub async fn resolve_all(&self, locator: &Locator) -> Result<CandidateSet, LocatorError> {
        let backend_node_ids = match &locator.strategy {
            LocatorStrategy::Css(selector) => self.resolve_css(selector.expose()).await?,
            LocatorStrategy::TestId(test_id) => {
                self.resolve_css(&test_id_selector(test_id.expose()))
                    .await?
            }
            LocatorStrategy::Text { value, match_kind } => {
                self.resolve_text(value.expose(), *match_kind).await?
            }
            LocatorStrategy::Label(name) => {
                self.resolve_ax(None, Some(name.expose()), true).await?
            }
            LocatorStrategy::Role { value, name } => {
                self.resolve_ax(
                    Some(value.expose()),
                    name.as_ref().map(|name| name.expose().as_str()),
                    false,
                )
                .await?
            }
        };
        let mut backend_node_ids = if locator.index.is_some() {
            self.in_dom_order(backend_node_ids).await?
        } else {
            let mut backend_node_ids = backend_node_ids;
            backend_node_ids.sort_by_key(|id| *id.inner());
            backend_node_ids.dedup();
            backend_node_ids
        };
        if locator.checked.is_some() || locator.selected.is_some() || locator.focused.is_some() {
            let arguments = [
                optional_bool(locator.checked),
                optional_bool(locator.selected),
                optional_bool(locator.focused),
            ];
            let mut filtered = Vec::with_capacity(backend_node_ids.len());
            for backend_node_id in backend_node_ids {
                if self
                    .call_on_node::<bool>(backend_node_id, STATE_FILTER_FUNCTION, &arguments)
                    .await?
                {
                    filtered.push(backend_node_id);
                }
            }
            backend_node_ids = filtered;
        }
        backend_node_ids = select_index(backend_node_ids, locator.index);
        Ok(CandidateSet { backend_node_ids })
    }

    async fn in_dom_order(
        &self,
        candidates: Vec<BackendNodeId>,
    ) -> Result<Vec<BackendNodeId>, LocatorError> {
        let elements = self.page.find_elements("*").await.map_err(protocol)?;
        Ok(elements
            .into_iter()
            .map(|element| element.backend_node_id)
            .filter(|id| candidates.contains(id))
            .collect())
    }

    pub async fn observe_unique(
        &self,
        locator: &Locator,
        requirements: Actionability,
    ) -> Result<Observation, LocatorError> {
        let candidates = self.resolve_all(locator).await?;
        match candidates.backend_node_ids.as_slice() {
            [] => Ok(Observation::NoMatch),
            [backend_node_id] => {
                self.observe_candidate(*backend_node_id, requirements, None)
                    .await
            }
            candidates => Ok(Observation::Multiple {
                count: candidates.len(),
            }),
        }
    }

    pub async fn observe_any_visible(
        &self,
        locator: &Locator,
    ) -> Result<Observation, LocatorError> {
        let candidates = self.resolve_all(locator).await?;
        if candidates.backend_node_ids.is_empty() {
            return Ok(Observation::NoMatch);
        }
        let mut observations = Vec::with_capacity(candidates.backend_node_ids.len());
        for backend_node_id in candidates.backend_node_ids {
            let observation = self
                .observe_candidate(backend_node_id, Actionability::VISIBLE, None)
                .await?;
            if matches!(observation, Observation::Ready(_)) {
                return Ok(observation);
            }
            observations.push(observation);
        }
        Ok(first_visible_or_hidden(observations))
    }

    pub async fn inspect(
        &self,
        backend_node_id: BackendNodeId,
        requirements: Actionability,
    ) -> Result<Observation, LocatorError> {
        self.observe_candidate(backend_node_id, requirements, None)
            .await
    }

    pub async fn wait_unique(
        &self,
        locator: &Locator,
        requirements: Actionability,
        deadline: Instant,
    ) -> Result<ResolvedElement, LocatorError> {
        let mut previous = None;
        loop {
            let observation = match self.resolve_all(locator).await {
                Ok(candidates) => match candidates.backend_node_ids.as_slice() {
                    [] => Observation::NoMatch,
                    [backend_node_id] => {
                        match self
                            .observe_candidate(*backend_node_id, requirements, previous)
                            .await
                        {
                            Ok(observation) => observation,
                            Err(error) if retryable(&error) => Observation::Unavailable {
                                message: error.to_string(),
                            },
                            Err(error) => return Err(error),
                        }
                    }
                    candidates => Observation::Multiple {
                        count: candidates.len(),
                    },
                },
                Err(error) if retryable(&error) => Observation::Unavailable {
                    message: error.to_string(),
                },
                Err(error) => return Err(error),
            };
            if let Observation::Ready(element) = observation {
                return Ok(element);
            }
            previous = stability_sample(&observation);

            let now = Instant::now();
            if now >= deadline {
                return Err(LocatorError::Timeout { last: observation });
            }
            tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now))).await;
        }
    }

    async fn resolve_css(&self, selector: &str) -> Result<Vec<BackendNodeId>, LocatorError> {
        self.page
            .find_elements(selector)
            .await
            .map(|elements| {
                elements
                    .into_iter()
                    .map(|element| element.backend_node_id)
                    .collect()
            })
            .map_err(protocol)
    }

    async fn resolve_text(
        &self,
        expected: &str,
        match_kind: TextMatch,
    ) -> Result<Vec<BackendNodeId>, LocatorError> {
        let elements = self.page.find_elements("*").await.map_err(protocol)?;
        let arguments = [
            argument(Value::String(normalize_text(expected))),
            argument(Value::Bool(match_kind == TextMatch::Exact)),
        ];
        let mut matches = Vec::new();
        for element in elements {
            let matched = self
                .call_on_node::<bool>(element.backend_node_id, TEXT_MATCH_FUNCTION, &arguments)
                .await?;
            if matched {
                matches.push(element.backend_node_id);
            }
        }
        Ok(matches)
    }

    async fn resolve_ax(
        &self,
        role: Option<&str>,
        name: Option<&str>,
        form_controls_only: bool,
    ) -> Result<Vec<BackendNodeId>, LocatorError> {
        let document = self.page.get_document().await.map_err(protocol)?;
        let main_frame = self
            .page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(protocol)?
            .result
            .frame_tree
            .frame
            .id;
        let mut query = QueryAxTreeParams::builder().node_id(document.node_id);
        if let Some(role) = role {
            query = query.role(role);
        }
        if let Some(name) = name
            && !form_controls_only
        {
            query = query.accessible_name(name);
        }
        let nodes = self
            .page
            .execute(query.build())
            .await
            .map_err(protocol)?
            .result
            .nodes;
        let mut matches = Vec::new();
        for node in nodes {
            if node.ignored
                || node.frame_id.as_ref().is_some_and(|id| id != &main_frame)
                || !ax_matches(&node, role, None)
            {
                continue;
            }
            let Some(backend_node_id) = node.backend_dom_node_id else {
                continue;
            };
            if !(ax_matches(&node, role, name)
                || form_controls_only
                    && ax_has_active_native_label(&node)
                    && self
                        .call_on_node::<bool>(
                            backend_node_id,
                            NATIVE_LABEL_MATCH_FUNCTION,
                            &[argument(Value::String(name.unwrap_or_default().to_owned()))],
                        )
                        .await?)
            {
                continue;
            }
            if form_controls_only
                && !self
                    .call_on_node::<bool>(backend_node_id, FORM_CONTROL_FUNCTION, &[])
                    .await?
            {
                continue;
            }
            matches.push(backend_node_id);
        }
        Ok(matches)
    }

    async fn observe_candidate(
        &self,
        backend_node_id: BackendNodeId,
        requirements: Actionability,
        previous: Option<(BackendNodeId, Rect)>,
    ) -> Result<Observation, LocatorError> {
        let state = self
            .call_on_node::<ElementState>(
                backend_node_id,
                ACTIONABILITY_FUNCTION,
                &[argument(Value::Bool(
                    requirements.visible
                        || requirements.enabled
                        || requirements.editable
                        || requirements.stable
                        || requirements.hit_test,
                ))],
            )
            .await?;
        if !state.attached {
            return Ok(Observation::Detached);
        }
        if requirements.visible && !state.visible {
            return Ok(Observation::Hidden);
        }
        if requirements.enabled && !state.enabled {
            return Ok(Observation::Disabled);
        }
        if requirements.editable && !state.editable {
            return Ok(Observation::NonEditable);
        }
        let rect = state
            .rect
            .ok_or_else(|| LocatorError::InvalidResponse("attached node had no box".into()))?;
        let center = state
            .center
            .ok_or_else(|| LocatorError::InvalidResponse("attached node had no center".into()))?;
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return Ok(Observation::EmptyBox);
        }
        if requirements.stable {
            let Some((previous_id, previous_rect)) = previous else {
                return Ok(Observation::Unstable {
                    backend_node_id,
                    previous: rect,
                    current: rect,
                });
            };
            if previous_id != backend_node_id || !rect_is_stable(previous_rect, rect) {
                return Ok(Observation::Unstable {
                    backend_node_id,
                    previous: previous_rect,
                    current: rect,
                });
            }
        }
        if requirements.hit_test && state.covered {
            return Ok(Observation::Covered {
                covering: state.covering,
            });
        }
        Ok(Observation::Ready(ResolvedElement {
            backend_node_id,
            rect,
            center,
        }))
    }

    async fn call_on_node<T: DeserializeOwned>(
        &self,
        backend_node_id: BackendNodeId,
        function: &str,
        arguments: &[CallArgument],
    ) -> Result<T, LocatorError> {
        let object = self
            .page
            .execute(
                ResolveNodeParams::builder()
                    .backend_node_id(backend_node_id)
                    .build(),
            )
            .await
            .map_err(protocol)?
            .result
            .object;
        let object_id = object.object_id.ok_or_else(|| {
            LocatorError::InvalidResponse("resolved DOM node had no object id".into())
        })?;
        let params = CallFunctionOnParams::builder()
            .function_declaration(function)
            .object_id(object_id.clone())
            .arguments(arguments.iter().cloned())
            .return_by_value(true)
            .await_promise(false)
            .build()
            .map_err(LocatorError::InvalidResponse)?;
        let response = self.page.execute(params).await.map_err(protocol)?.result;
        let _ = self.page.execute(ReleaseObjectParams::new(object_id)).await;
        if let Some(exception) = response.exception_details {
            return Err(LocatorError::Protocol(format!(
                "page function threw: {}",
                exception.text
            )));
        }
        let value = response.result.value.ok_or_else(|| {
            LocatorError::InvalidResponse("page function returned no value".into())
        })?;
        serde_json::from_value(value)
            .map_err(|error| LocatorError::InvalidResponse(error.to_string()))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ElementState {
    attached: bool,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    editable: bool,
    rect: Option<Rect>,
    center: Option<Point>,
    #[serde(default)]
    covered: bool,
    covering: Option<String>,
}

impl<'de> Deserialize<'de> for Rect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawRect {
            x: f64,
            y: f64,
            width: f64,
            height: f64,
        }
        let raw = RawRect::deserialize(deserializer)?;
        Ok(Self {
            x: raw.x,
            y: raw.y,
            width: raw.width,
            height: raw.height,
        })
    }
}

impl<'de> Deserialize<'de> for Point {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawPoint {
            x: f64,
            y: f64,
        }
        let raw = RawPoint::deserialize(deserializer)?;
        Ok(Self { x: raw.x, y: raw.y })
    }
}

pub fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn text_matches(actual: &str, expected: &str, match_kind: TextMatch) -> bool {
    let actual = normalize_text(actual);
    let expected = normalize_text(expected);
    match match_kind {
        TextMatch::Exact => actual == expected,
        TextMatch::Contains => actual.contains(&expected),
    }
}

pub fn test_id_selector(test_id: &str) -> String {
    format!("[data-testid=\"{}\"]", css_string_escape(test_id))
}

fn css_string_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\0' => escaped.push('\u{fffd}'),
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' => escaped.push_str("\\a "),
            '\r' => escaped.push_str("\\d "),
            '\u{c}' => escaped.push_str("\\c "),
            character if character.is_control() => {
                escaped.push('\\');
                escaped.push_str(&format!("{:x} ", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn argument(value: Value) -> CallArgument {
    CallArgument::builder().value(value).build()
}

fn optional_bool(value: Option<bool>) -> CallArgument {
    argument(value.map_or(Value::Null, Value::Bool))
}

fn select_index<T>(values: Vec<T>, index: Option<usize>) -> Vec<T> {
    match index {
        Some(index) => values.into_iter().nth(index).into_iter().collect(),
        None => values,
    }
}

fn ax_matches(node: &AxNode, role: Option<&str>, name: Option<&str>) -> bool {
    role.is_none_or(|expected| ax_string(node.role.as_ref()) == Some(expected))
        && name.is_none_or(|expected| ax_string(node.name.as_ref()) == Some(expected))
}

fn ax_has_active_native_label(node: &AxNode) -> bool {
    node.name
        .as_ref()
        .and_then(|name| name.sources.as_ref())
        .is_some_and(|sources| {
            sources.iter().any(|source| {
                source.superseded != Some(true)
                    && matches!(
                        source.native_source,
                        Some(
                            AxValueNativeSourceType::Label
                                | AxValueNativeSourceType::Labelfor
                                | AxValueNativeSourceType::Labelwrapped
                        )
                    )
            })
        })
}

fn ax_string(
    value: Option<&chromiumoxide::cdp::browser_protocol::accessibility::AxValue>,
) -> Option<&str> {
    value?.value.as_ref()?.as_str()
}

fn rect_is_stable(left: Rect, right: Rect) -> bool {
    const TOLERANCE: f64 = 0.25;
    (left.x - right.x).abs() <= TOLERANCE
        && (left.y - right.y).abs() <= TOLERANCE
        && (left.width - right.width).abs() <= TOLERANCE
        && (left.height - right.height).abs() <= TOLERANCE
}

fn stability_sample(observation: &Observation) -> Option<(BackendNodeId, Rect)> {
    match observation {
        Observation::Unstable {
            backend_node_id,
            current,
            ..
        } => Some((*backend_node_id, *current)),
        _ => None,
    }
}

fn first_visible_or_hidden(observations: Vec<Observation>) -> Observation {
    observations
        .into_iter()
        .find(|observation| {
            !matches!(
                observation,
                Observation::NoMatch
                    | Observation::Detached
                    | Observation::Hidden
                    | Observation::EmptyBox
            )
        })
        .unwrap_or(Observation::Hidden)
}

fn protocol(error: impl fmt::Display) -> LocatorError {
    LocatorError::Protocol(error.to_string())
}

pub(crate) fn retryable(error: &LocatorError) -> bool {
    let LocatorError::Protocol(message) = error else {
        return false;
    };
    retryable_cdp_message(message)
}

pub(crate) fn retryable_cdp_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "execution context",
        "cannot find context",
        "context with specified id",
        "node with given id",
        "navigat",
    ]
    .iter()
    .any(|part| message.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id_selector_escapes_a_css_string_without_changing_ordinary_text() {
        assert_eq!(test_id_selector("submit"), "[data-testid=\"submit\"]");
        assert_eq!(
            test_id_selector("quote\" slash\\ line\nend"),
            "[data-testid=\"quote\\\" slash\\\\ line\\a end\"]"
        );
        assert_eq!(test_id_selector("nul\0id"), "[data-testid=\"nul�id\"]");
    }

    #[test]
    fn index_is_zero_based_and_out_of_range_is_no_match() {
        assert_eq!(select_index(vec![10, 20, 30], Some(0)), [10]);
        assert_eq!(select_index(vec![10, 20, 30], Some(2)), [30]);
        assert!(select_index(vec![10, 20, 30], Some(3)).is_empty());
        assert_eq!(select_index(vec![10, 20, 30], None), [10, 20, 30]);
    }

    #[test]
    fn normalized_text_is_case_sensitive_and_collapses_unicode_whitespace() {
        assert_eq!(normalize_text("  Sign\n\tin\u{2003}now  "), "Sign in now");
        assert!(text_matches(" Sign   in ", "Sign in", TextMatch::Exact));
        assert!(text_matches(
            "Welcome, Ada Lovelace",
            "Ada Love",
            TextMatch::Contains
        ));
        assert!(!text_matches("sign in", "Sign in", TextMatch::Exact));
    }

    #[test]
    fn geometric_stability_allows_only_subpixel_noise() {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 30.0,
        };
        assert!(rect_is_stable(rect, Rect { x: 10.2, ..rect }));
        assert!(!rect_is_stable(rect, Rect { x: 10.3, ..rect }));
    }

    #[test]
    fn hidden_passes_for_any_candidate_count_when_none_is_visible() {
        assert_eq!(first_visible_or_hidden(Vec::new()), Observation::Hidden);
        assert_eq!(
            first_visible_or_hidden(vec![
                Observation::Hidden,
                Observation::Detached,
                Observation::EmptyBox,
            ]),
            Observation::Hidden
        );
        assert_eq!(
            first_visible_or_hidden(vec![Observation::Hidden, Observation::Disabled]),
            Observation::Disabled
        );
    }

    #[test]
    fn actionability_only_treats_text_inputs_as_editable() {
        assert!(
            ACTIONABILITY_FUNCTION.contains("['text','search','email','url','tel','password']")
        );
        assert!(!ACTIONABILITY_FUNCTION.contains("'date'"));
        assert!(!ACTIONABILITY_FUNCTION.contains("'number'"));
    }

    #[test]
    fn transient_cdp_context_errors_are_retryable() {
        assert!(retryable_cdp_message(
            "Cannot find context with specified id"
        ));
        assert!(retryable_cdp_message(
            "Execution context was destroyed by navigation"
        ));
        assert!(!retryable_cdp_message("Target closed"));
        assert!(!retryable_cdp_message("Session closed"));
        assert!(!retryable_cdp_message("Invalid selector"));
    }
}
