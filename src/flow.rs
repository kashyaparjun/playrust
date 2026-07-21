use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::browser::Geolocation;

pub const REDACTED: &str = "[REDACTED]";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum accepted YAML flow source size in bytes (1 MiB).
pub const MAX_FLOW_SOURCE_BYTES: usize = 1024 * 1024;
/// Maximum number of steps in one flow.
pub const MAX_FLOW_STEPS: usize = 10_000;
/// Maximum nested subflow include depth, excluding the entrypoint.
pub const MAX_SUBFLOW_DEPTH: usize = 32;
/// Maximum nested locator relation depth, excluding the root locator.
pub const MAX_LOCATOR_DEPTH: usize = 8;
/// Maximum compile-time repetitions of one step or subflow.
pub const MAX_REPEAT: usize = 100;
/// Maximum runtime iterations of one bounded while loop.
pub const MAX_WHILE_ITERATIONS: usize = 100;
/// Maximum nesting depth of a structured boolean expression.
pub const MAX_EXPRESSION_DEPTH: usize = 8;
/// Maximum nodes in one structured boolean expression.
pub const MAX_EXPRESSION_NODES: usize = 64;
/// Maximum additional attempts for an assertion.
pub const MAX_RETRIES: usize = 10;
/// Maximum size of a YAML scalar or interpolated value in bytes (64 KiB).
pub const MAX_SCALAR_BYTES: usize = 64 * 1024;
/// Maximum serialized size of one runtime output or HTTP body (64 KiB).
pub const MAX_RUNTIME_VALUE_BYTES: usize = 64 * 1024;
/// Maximum number of headers accepted by one HTTP request.
pub const MAX_HTTP_HEADERS: usize = 100;
/// Maximum timeout accepted for flow settings or an individual step.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const MAX_GESTURE_DELTA: i32 = 10_000;
pub const MAX_GESTURE_DURATION: Duration = Duration::from_secs(10);
pub const DEFAULT_SWIPE_DURATION: Duration = Duration::from_millis(300);
pub const DEFAULT_LONG_PRESS_DURATION: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub enum FlowError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid YAML: {0}")]
    Yaml(String),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    value: T,
    secret: bool,
}

impl<T> Resolved<T> {
    pub fn expose(&self) -> &T {
        &self.value
    }

    pub fn is_secret(&self) -> bool {
        self.secret
    }

    pub(crate) fn new(value: T, secret: bool) -> Self {
        Self { value, secret }
    }
}

impl<T: fmt::Debug> fmt::Debug for Resolved<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.secret {
            formatter.write_str(REDACTED)
        } else {
            self.value.fmt(formatter)
        }
    }
}

impl<T: fmt::Display> fmt::Display for Resolved<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.secret {
            formatter.write_str(REDACTED)
        } else {
            self.value.fmt(formatter)
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn redact(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret, REDACTED)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    pub(crate) fn extend(&mut self, other: &Self) {
        self.secrets.extend(other.secrets.iter().cloned());
        self.secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        self.secrets.dedup();
    }

    pub(crate) fn add_secret(&mut self, secret: String) {
        if !secret.is_empty() {
            self.secrets.push(secret);
            self.secrets
                .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
            self.secrets.dedup();
        }
    }
}

impl fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Redactor")
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawFlow {
    pub version: u32,
    pub name: String,
    pub base_url: Option<String>,
    #[serde(default)]
    pub settings: RawSettings,
    #[serde(default)]
    pub vars: BTreeMap<String, RawVariable>,
    #[serde(default)]
    pub secrets: BTreeMap<String, RawSecret>,
    pub steps: Vec<RawStep>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSettings {
    pub timeout: Option<String>,
    pub viewport: Option<RawViewport>,
    pub video: Option<VideoMode>,
    pub geolocation: Option<RawGeolocation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawViewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawGeolocation {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawVariable {
    Literal(String),
    Environment(RawEnvironmentVariable),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEnvironmentVariable {
    pub env: String,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSecret {
    pub env: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStep {
    #[serde(skip)]
    source_index: Option<usize>,
    pub id: Option<String>,
    pub timeout: Option<String>,
    pub run: Option<RawRun>,
    pub when: Option<RawWhen>,
    pub r#while: Option<RawWhile>,
    pub repeat: Option<usize>,
    pub retry: Option<usize>,
    pub open: Option<RawOpen>,
    pub click: Option<RawClick>,
    pub double_click: Option<RawClick>,
    pub fill: Option<RawFill>,
    pub erase: Option<RawTargetAction>,
    pub select: Option<RawSelect>,
    pub scroll: Option<RawScroll>,
    pub scroll_until_visible: Option<RawScrollUntilVisible>,
    pub swipe: Option<RawSwipe>,
    pub long_press: Option<RawLongPress>,
    pub wait_until_visible: Option<RawTargetAction>,
    pub wait_until_stable: Option<RawTargetAction>,
    pub back: Option<RawEmpty>,
    pub switch_page: Option<RawPageSwitch>,
    pub switch_frame: Option<RawFrameSwitch>,
    pub press: Option<RawPress>,
    pub screenshot: Option<RawScreenshot>,
    pub recording: Option<RecordingControl>,
    pub dialog: Option<RawDialog>,
    pub clear: Option<ClearTarget>,
    pub evaluate: Option<RawEvaluate>,
    pub request: Option<RawRequest>,
    #[serde(rename = "assert")]
    pub assertion: Option<RawAssertion>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawOpen {
    Url(String),
    Options(RawOpenOptions),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOpenOptions {
    pub url: String,
    pub wait_until: Option<RawOpenWaitUntil>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOpenWaitUntil {
    pub visible: Option<RawLocator>,
    pub stable: Option<RawLocator>,
}

impl RawStep {
    fn operation_count(&self) -> usize {
        [
            self.run.is_some(),
            self.open.is_some(),
            self.click.is_some(),
            self.double_click.is_some(),
            self.fill.is_some(),
            self.erase.is_some(),
            self.select.is_some(),
            self.scroll.is_some(),
            self.scroll_until_visible.is_some(),
            self.swipe.is_some(),
            self.long_press.is_some(),
            self.wait_until_visible.is_some(),
            self.wait_until_stable.is_some(),
            self.back.is_some(),
            self.switch_page.is_some(),
            self.switch_frame.is_some(),
            self.press.is_some(),
            self.screenshot.is_some(),
            self.recording.is_some(),
            self.dialog.is_some(),
            self.clear.is_some(),
            self.evaluate.is_some(),
            self.request.is_some(),
            self.assertion.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawRun {
    Path(String),
    Mapped(RawRunOptions),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRunOptions {
    pub path: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWhen {
    pub visible: Option<RawLocator>,
    pub hidden: Option<RawLocator>,
    pub variable: Option<RawVariablePredicate>,
    pub platform: Option<Platform>,
    pub expression: Option<RawExpression>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Web,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWhile {
    pub expression: RawExpression,
    pub max_iterations: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExpression {
    pub all: Option<Vec<RawExpression>>,
    pub any: Option<Vec<RawExpression>>,
    pub not: Option<Box<RawExpression>>,
    pub equals: Option<RawComparison>,
    pub not_equals: Option<RawComparison>,
    pub boolean: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawComparison {
    pub left: String,
    pub right: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVariablePredicate {
    pub name: String,
    pub equals: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEvaluate {
    pub script: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub save_as: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub expected_status: u16,
    pub save_as: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScreenshot {
    pub name: String,
    pub crop: Option<RawCrop>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTargetAction {
    pub target: RawLocator,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawClick {
    pub target: Option<RawLocator>,
    pub position: Option<RelativePoint>,
    pub point: Option<ViewportPoint>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawFill {
    pub target: RawLocator,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSelect {
    pub target: RawLocator,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScroll {
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub y: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScrollUntilVisible {
    pub target: RawLocator,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSwipe {
    pub target: RawLocator,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    pub duration: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLongPress {
    pub target: RawLocator,
    pub duration: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEmpty {}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDialog {
    pub action: NativeDialogResponse,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeDialogResponse {
    Accept,
    Dismiss,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawPageSwitch {
    Location(PageLocation),
    Selector(RawPageSelector),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPageSelector {
    pub name: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageLocation {
    Popup,
    Opener,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageSwitch {
    Popup,
    Opener,
    Name(Resolved<String>),
    Url(Resolved<Url>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawFrameSwitch {
    Target(Box<RawTargetAction>),
    Location(FrameLocation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameLocation {
    Main,
    Parent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPress {
    pub target: RawLocator,
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAssertion {
    pub visible: Option<RawLocator>,
    pub hidden: Option<RawLocator>,
    pub text: Option<RawTextAssertion>,
    pub url: Option<RawUrlAssertion>,
    pub screenshot: Option<RawVisualAssertion>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVisualAssertion {
    pub baseline: String,
    pub crop: Option<RawCrop>,
    pub channel_tolerance: Option<u8>,
    pub max_changed_ratio: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTextAssertion {
    pub target: RawLocator,
    pub equals: Option<String>,
    pub contains: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawUrlAssertion {
    pub equals: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLocator {
    pub css: Option<String>,
    pub test_id: Option<String>,
    pub text: Option<RawTextLocator>,
    pub label: Option<String>,
    pub role: Option<RawRoleLocator>,
    pub index: Option<usize>,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub focused: Option<bool>,
    pub enabled: Option<bool>,
    pub within: Option<Box<RawLocator>>,
    pub child_of: Option<Box<RawLocator>>,
    pub has: Option<Box<RawLocator>>,
    pub above: Option<Box<RawLocator>>,
    pub below: Option<Box<RawLocator>>,
    pub left: Option<Box<RawLocator>>,
    pub right: Option<Box<RawLocator>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawTextLocator {
    Scalar(String),
    Detailed(RawTextLocatorOptions),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTextLocatorOptions {
    pub value: String,
    #[serde(rename = "match")]
    pub match_kind: Option<TextMatch>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRoleLocator {
    pub value: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoMode {
    Off,
    On,
    RetainOnFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextMatch {
    Exact,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelativePoint {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewportPoint {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClearTarget {
    Cookies,
    Storage,
    Indexeddb,
    CacheStorage,
    ServiceWorkers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingControl {
    Start,
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledFlow {
    pub source: PathBuf,
    pub name: String,
    pub base_url: Option<Resolved<Url>>,
    pub settings: FlowSettings,
    pub inputs: BTreeMap<String, Resolved<String>>,
    pub steps: Vec<CompiledStep>,
    pub manual_recording: bool,
    pub redactor: Redactor,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowSettings {
    pub timeout: Duration,
    pub viewport: Viewport,
    pub video: VideoMode,
    pub geolocation: Option<Geolocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledStep {
    pub index: usize,
    pub source: PathBuf,
    pub source_index: usize,
    pub id: Option<String>,
    pub timeout: Duration,
    pub when: Option<When>,
    pub guards: Vec<Guard>,
    pub retries: usize,
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    Visible(Locator),
    Hidden(Locator),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub id: usize,
    pub first: bool,
    pub kind: GuardKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardKind {
    When(Expression),
    While {
        loop_id: usize,
        new_loop: bool,
        expression: Expression,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    All(Vec<Expression>),
    Any(Vec<Expression>),
    Not(Box<Expression>),
    Equals(RuntimeValue, RuntimeValue),
    NotEquals(RuntimeValue, RuntimeValue),
    Boolean(RuntimeValue),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Open {
        url: Resolved<Url>,
        wait_until: Option<OpenWaitUntil>,
    },
    Click {
        target: Locator,
        position: Option<RelativePoint>,
    },
    ClickPoint {
        point: ViewportPoint,
    },
    DoubleClick {
        target: Locator,
        position: Option<RelativePoint>,
    },
    Fill {
        target: Locator,
        value: RuntimeValue,
    },
    Erase {
        target: Locator,
    },
    Select {
        target: Locator,
        value: RuntimeValue,
    },
    Scroll {
        x: i64,
        y: i64,
    },
    ScrollUntilVisible {
        target: Locator,
        x: i32,
        y: i32,
    },
    Swipe {
        target: Locator,
        x: i32,
        y: i32,
        duration: Duration,
    },
    LongPress {
        target: Locator,
        duration: Duration,
    },
    WaitUntilVisible {
        target: Locator,
    },
    WaitUntilStable {
        target: Locator,
    },
    Back,
    SwitchPage(PageSwitch),
    SwitchFrame(FrameSwitch),
    Press {
        target: Locator,
        key: Key,
        modifiers: Vec<Modifier>,
    },
    Screenshot {
        name: String,
        crop: Option<Crop>,
    },
    Recording(RecordingControl),
    Dialog {
        action: NativeDialogResponse,
        text: Option<RuntimeValue>,
    },
    Clear(ClearTarget),
    Evaluate {
        script: String,
        args: Vec<RuntimeValue>,
        save_as: Option<String>,
    },
    Request {
        method: String,
        url: RuntimeValue,
        headers: BTreeMap<String, RuntimeValue>,
        body: Option<RuntimeValue>,
        expected_status: u16,
        save_as: Option<String>,
    },
    Assert(Assertion),
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpenWaitUntil {
    Visible(Locator),
    Stable(Locator),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameSwitch {
    Target(Locator),
    Main,
    Parent,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeValue {
    template: Resolved<String>,
    parts: Vec<RuntimeValuePart>,
    outputs: BTreeSet<String>,
}

impl fmt::Debug for RuntimeValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_secret() {
            formatter.write_str(REDACTED)
        } else {
            self.template.fmt(formatter)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeValuePart {
    Literal(Resolved<String>),
    Output(String),
}

impl RuntimeValue {
    pub fn resolve(
        &self,
        outputs: &BTreeMap<String, Resolved<Value>>,
    ) -> Result<Resolved<String>, FlowError> {
        let mut value = String::new();
        let mut secret = false;
        for part in &self.parts {
            let resolved = match part {
                RuntimeValuePart::Literal(value) => value.clone(),
                RuntimeValuePart::Output(name) => {
                    let output = outputs.get(name).ok_or_else(|| {
                        FlowError::Invalid(format!("runtime output {name:?} is unavailable"))
                    })?;
                    Resolved::new(
                        match output.expose() {
                            Value::String(value) => value.clone(),
                            value => serde_json::to_string(value).expect("JSON value serializes"),
                        },
                        true,
                    )
                }
            };
            push_interpolated("runtime value", &mut value, resolved.expose())?;
            secret |= resolved.secret;
        }
        Ok(Resolved::new(value, secret))
    }

    pub fn expose(&self) -> &str {
        self.template.expose()
    }

    pub fn is_secret(&self) -> bool {
        self.template.is_secret() || !self.outputs.is_empty()
    }

    fn output_names(&self) -> impl Iterator<Item = &String> {
        self.outputs.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Assertion {
    Visible(Locator),
    Hidden(Locator),
    Text {
        target: Locator,
        expected: Resolved<String>,
        match_kind: TextMatch,
    },
    Url(UrlExpectation),
    Screenshot(VisualExpectation),
}

#[derive(Debug, Clone, PartialEq)]
pub struct VisualExpectation {
    pub baseline: PathBuf,
    pub crop: Option<Crop>,
    pub channel_tolerance: u8,
    pub max_changed_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlExpectation {
    Equals(Resolved<Url>),
    Path(Resolved<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locator {
    pub strategy: LocatorStrategy,
    pub index: Option<usize>,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub focused: Option<bool>,
    pub enabled: Option<bool>,
    pub relations: Vec<LocatorRelation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatorRelation {
    pub kind: RelationKind,
    pub locator: Box<Locator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Within,
    ChildOf,
    Has,
    Above,
    Below,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorStrategy {
    Css(Resolved<String>),
    TestId(Resolved<String>),
    Text {
        value: Resolved<String>,
        match_kind: TextMatch,
    },
    Label(Resolved<String>),
    Role {
        value: Resolved<String>,
        name: Option<Resolved<String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Character(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modifier {
    Alt,
    Control,
    Meta,
    Shift,
}

pub fn parse_yaml(source: &str) -> Result<RawFlow, FlowError> {
    let options = serde_saphyr::options! {
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 0,
            max_replay_stack_depth: 0,
            max_alias_expansions_per_anchor: 0,
        },
        strict_booleans: true,
    };
    serde_saphyr::from_str_with_options(source, options)
        .map_err(|error| FlowError::Yaml(error.to_string()))
}

pub fn compile_yaml(
    source: &str,
    source_path: impl Into<PathBuf>,
    cli_vars: &BTreeMap<String, String>,
) -> Result<CompiledFlow, FlowError> {
    let environment = std::env::vars().collect();
    compile_yaml_with_env(source, source_path, cli_vars, &environment)
}

pub fn compile_inline_yaml(
    source: &str,
    source_path: impl Into<PathBuf>,
    cli_vars: &BTreeMap<String, String>,
    available_outputs: &BTreeSet<String>,
) -> Result<CompiledFlow, FlowError> {
    compile_inline_yaml_with_video(source, source_path, cli_vars, available_outputs, None)
}

pub fn compile_inline_yaml_with_video(
    source: &str,
    source_path: impl Into<PathBuf>,
    cli_vars: &BTreeMap<String, String>,
    available_outputs: &BTreeSet<String>,
    video: Option<VideoMode>,
) -> Result<CompiledFlow, FlowError> {
    let environment = std::env::vars().collect();
    require_source_size(source.len())?;
    let mut raw = parse_yaml(source)?;
    if let Some(video) = video {
        raw.settings.video = Some(video);
    }
    if let Some((index, _)) = raw
        .steps
        .iter()
        .enumerate()
        .find(|(_, step)| step.run.is_some())
    {
        return invalid(format!(
            "step {} run subflows are unavailable for inline session submissions",
            index + 1
        ));
    }
    if let Some((index, _)) = raw.steps.iter().enumerate().find(|(_, step)| {
        step.assertion
            .as_ref()
            .is_some_and(|assertion| assertion.screenshot.is_some())
    }) {
        return invalid(format!(
            "step {} visual baselines are unavailable for inline session submissions",
            index + 1
        ));
    }
    let mut flow = compile_raw_inner(
        raw,
        source_path.into(),
        cli_vars,
        &environment,
        &BTreeMap::new(),
        true,
        false,
    )?;
    validate_expanded_steps_with_outputs(&mut flow.steps, available_outputs)?;
    validate_page_switching_video(&flow)?;
    Ok(flow)
}

pub fn compile_yaml_with_env(
    source: &str,
    source_path: impl Into<PathBuf>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
) -> Result<CompiledFlow, FlowError> {
    require_source_size(source.len())?;
    compile_raw(parse_yaml(source)?, source_path, cli_vars, environment)
}

pub fn compile_file(
    path: impl AsRef<Path>,
    cli_vars: &BTreeMap<String, String>,
) -> Result<CompiledFlow, FlowError> {
    compile_file_with_video(path, cli_vars, None)
}

pub fn compile_file_with_video(
    path: impl AsRef<Path>,
    cli_vars: &BTreeMap<String, String>,
    video: Option<VideoMode>,
) -> Result<CompiledFlow, FlowError> {
    let environment = std::env::vars().collect();
    compile_file_with_env_and_video(path, cli_vars, &environment, video)
}

pub fn compile_file_with_env(
    path: impl AsRef<Path>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
) -> Result<CompiledFlow, FlowError> {
    compile_file_with_env_and_video(path, cli_vars, environment, None)
}

fn compile_file_with_env_and_video(
    path: impl AsRef<Path>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    video: Option<VideoMode>,
) -> Result<CompiledFlow, FlowError> {
    let path = path.as_ref();
    let canonical = fs::canonicalize(path).map_err(|source| FlowError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut state = ExpansionState {
        active: vec![(canonical, path.to_owned())],
        declared_cli_vars: BTreeSet::new(),
        browser_settings: None,
    };
    let mut raw = read_flow(path)?;
    if let Some(video) = video {
        raw.settings.video = Some(video);
    }
    let mut flow = compile_raw_expanded(
        raw,
        path.to_owned(),
        cli_vars,
        environment,
        &BTreeMap::new(),
        0,
        &mut state,
    )?;
    if let Some(name) = cli_vars
        .keys()
        .find(|name| !state.declared_cli_vars.contains(*name))
    {
        return invalid(format!("CLI variable {name:?} is not declared under vars"));
    }
    validate_expanded_steps(&mut flow.steps)?;
    validate_page_switching_video(&flow)?;
    Ok(flow)
}

fn read_flow(path: &Path) -> Result<RawFlow, FlowError> {
    let file = fs::File::open(path).map_err(|source| FlowError::Io {
        path: path.to_owned(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| FlowError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.len() > MAX_FLOW_SOURCE_BYTES as u64 {
        return invalid(format!(
            "flow source exceeds the maximum size of {MAX_FLOW_SOURCE_BYTES} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FLOW_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| FlowError::Io {
            path: path.to_owned(),
            source,
        })?;
    require_source_size(bytes.len())?;
    let source = String::from_utf8(bytes).map_err(|source| FlowError::Io {
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidData, source),
    })?;
    parse_yaml(&source).map_err(|error| with_path(path, error))
}

pub fn compile_raw(
    raw: RawFlow,
    source_path: impl Into<PathBuf>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
) -> Result<CompiledFlow, FlowError> {
    let mut flow = compile_raw_inner(
        raw,
        source_path.into(),
        cli_vars,
        environment,
        &BTreeMap::new(),
        true,
        false,
    )?;
    validate_expanded_steps(&mut flow.steps)?;
    validate_page_switching_video(&flow)?;
    Ok(flow)
}

fn compile_raw_inner(
    raw: RawFlow,
    source_path: PathBuf,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    passed_vars: &BTreeMap<String, Resolved<String>>,
    reject_unknown_cli_vars: bool,
    allow_empty_steps: bool,
) -> Result<CompiledFlow, FlowError> {
    if raw.version != 1 {
        return invalid("version must be 1");
    }
    if raw.steps.is_empty() && !allow_empty_steps {
        return invalid("steps must not be empty");
    }
    if raw.steps.len() > MAX_FLOW_STEPS {
        return invalid(format!("steps must not exceed {MAX_FLOW_STEPS}"));
    }

    let manual_recording = raw.steps.iter().any(|step| step.recording.is_some());
    let inputs = resolve_inputs(
        &raw.vars,
        &raw.secrets,
        cli_vars,
        environment,
        passed_vars,
        reject_unknown_cli_vars,
    )?;
    let redactor = Redactor::from_inputs(&inputs);
    let name = interpolate_non_secret("name", &raw.name, &inputs)?;
    require_non_empty("name", &name)?;

    let timeout = raw
        .settings
        .timeout
        .as_deref()
        .map(|value| interpolate("settings.timeout", value, &inputs))
        .transpose()?
        .map(|value| parse_duration("settings.timeout", value.expose()))
        .transpose()?
        .unwrap_or(DEFAULT_TIMEOUT);
    let viewport = raw.settings.viewport.map_or(
        Viewport {
            width: 1280,
            height: 720,
        },
        |viewport| Viewport {
            width: viewport.width,
            height: viewport.height,
        },
    );
    if viewport.width == 0 || viewport.height == 0 {
        return invalid("viewport width and height must be greater than zero");
    }
    let video = raw.settings.video.unwrap_or(VideoMode::On);
    if video != VideoMode::Off
        && (!viewport.width.is_multiple_of(2) || !viewport.height.is_multiple_of(2))
    {
        return invalid("video requires even viewport width and height");
    }
    let geolocation = raw
        .settings
        .geolocation
        .map(|value| {
            Geolocation::new(
                value.latitude,
                value.longitude,
                value.accuracy.unwrap_or(0.0),
            )
            .map_err(|error| FlowError::Invalid(error.to_string()))
        })
        .transpose()?;

    let base_url = raw
        .base_url
        .as_deref()
        .map(|value| {
            let value = interpolate("base_url", value, &inputs)?;
            parse_absolute_url("base_url", value)
        })
        .transpose()?;

    let settings = FlowSettings {
        timeout,
        viewport,
        video,
        geolocation,
    };
    let mut ids = BTreeSet::new();
    let mut screenshot_names = BTreeSet::new();
    let mut steps = Vec::with_capacity(raw.steps.len());
    for (offset, step) in raw.steps.into_iter().enumerate() {
        let index = step.source_index.unwrap_or(offset + 1);
        let repeat = validate_count("repeat", index, step.repeat.unwrap_or(1), MAX_REPEAT)?;
        let while_control = step
            .r#while
            .clone()
            .map(|control| compile_while(control, index, &inputs))
            .transpose()?;
        let retries = step
            .retry
            .map(|value| validate_count("retry", index, value, MAX_RETRIES))
            .transpose()?
            .unwrap_or(0);
        if retries > 0 && step.assertion.is_none() {
            return invalid(format!(
                "step {index} retry is only supported for assertions"
            ));
        }
        if retries > 0 && step.when.is_some() {
            return invalid(format!("step {index} cannot combine when and retry"));
        }
        let id = step
            .id
            .as_deref()
            .map(|value| interpolate_non_secret(&format!("step {index} id"), value, &inputs))
            .transpose()?;
        if let Some(id) = &id {
            require_non_empty(&format!("step {index} id"), id)?;
            if !ids.insert(id.clone()) {
                return invalid(format!("duplicate step id {id:?}"));
            }
            if repeat > 1 || while_control.is_some() {
                return invalid(format!(
                    "step {index} cannot combine id and repeat or while"
                ));
            }
        }
        let step_timeout = step
            .timeout
            .as_deref()
            .map(|value| interpolate(&format!("step {index} timeout"), value, &inputs))
            .transpose()?
            .map(|value| parse_duration(&format!("step {index} timeout"), value.expose()))
            .transpose()?
            .unwrap_or(timeout);
        if step.run.is_some() {
            return invalid(format!(
                "step {index} subflow includes require compiling a flow file"
            ));
        }
        let when = compile_when(step.when.clone(), index, &inputs)?;
        let operation = compile_operation(
            step,
            index,
            &source_path,
            base_url.as_ref(),
            viewport,
            &inputs,
        )?;
        let skip = matches!(when, CompiledWhen::Skip);
        let when = match when {
            CompiledWhen::Runtime(when) => Some(when),
            CompiledWhen::Always | CompiledWhen::Skip => None,
        };
        if skip {
            continue;
        }
        if let Operation::Screenshot { name, .. } = &operation
            && !screenshot_names.insert(name.to_ascii_lowercase())
        {
            return invalid(format!("duplicate screenshot name {name:?}"));
        }
        for _ in 0..repeat {
            let iterations = while_control
                .as_ref()
                .map_or(1, |(_, max_iterations)| *max_iterations);
            for iteration in 0..iterations {
                let guards = while_control
                    .as_ref()
                    .map(|(expression, _)| {
                        vec![Guard {
                            id: 0,
                            first: true,
                            kind: GuardKind::While {
                                loop_id: 0,
                                new_loop: iteration == 0,
                                expression: expression.clone(),
                            },
                        }]
                    })
                    .unwrap_or_default();
                steps.push(CompiledStep {
                    index,
                    source: source_path.clone(),
                    source_index: index,
                    id: id.clone(),
                    timeout: step_timeout,
                    when: when.clone(),
                    guards,
                    retries,
                    operation: operation.clone(),
                });
            }
        }
        if steps.len() > MAX_FLOW_STEPS {
            return invalid(format!("expanded steps must not exceed {MAX_FLOW_STEPS}"));
        }
    }

    Ok(CompiledFlow {
        source: source_path,
        name,
        base_url,
        settings,
        inputs,
        steps,
        manual_recording,
        redactor,
    })
}

struct ExpansionState {
    active: Vec<(PathBuf, PathBuf)>,
    declared_cli_vars: BTreeSet<String>,
    browser_settings: Option<(Viewport, VideoMode)>,
}

fn compile_raw_expanded(
    mut raw: RawFlow,
    source_path: PathBuf,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    passed_vars: &BTreeMap<String, Resolved<String>>,
    depth: usize,
    state: &mut ExpansionState,
) -> Result<CompiledFlow, FlowError> {
    if depth > 0 && (raw.settings.viewport.is_some() || raw.settings.video.is_some()) {
        return invalid(format!(
            "{}: subflows cannot set settings.viewport or settings.video",
            source_path.display()
        ));
    }
    if let Some((viewport, video)) = state.browser_settings {
        raw.settings.viewport = Some(RawViewport {
            width: viewport.width,
            height: viewport.height,
        });
        raw.settings.video = Some(video);
    }
    if raw.steps.is_empty() {
        return invalid(format!(
            "{}: steps must not be empty",
            source_path.display()
        ));
    }
    if raw.steps.len() > MAX_FLOW_STEPS {
        return invalid(format!(
            "{}: steps must not exceed {MAX_FLOW_STEPS}",
            source_path.display()
        ));
    }
    state.declared_cli_vars.extend(raw.vars.keys().cloned());
    let original_len = raw.steps.len();
    let mut includes = BTreeMap::new();
    for (offset, step) in raw.steps.iter_mut().enumerate() {
        step.source_index = Some(offset + 1);
        if let Some(run) = &step.run {
            if step.id.is_some() || step.timeout.is_some() || step.operation_count() != 1 {
                return invalid(format!(
                    "{}: step {} run must be the only field except when, while, repeat, and retry",
                    source_path.display(),
                    offset + 1
                ));
            }
            if step
                .when
                .as_ref()
                .is_some_and(|when| when.visible.is_some() || when.hidden.is_some())
            {
                return invalid(format!(
                    "{}: step {} run does not support DOM when predicates",
                    source_path.display(),
                    offset + 1
                ));
            }
            let repeat =
                validate_count("repeat", offset + 1, step.repeat.unwrap_or(1), MAX_REPEAT)?;
            let retries = step
                .retry
                .map(|value| validate_count("retry", offset + 1, value, MAX_RETRIES))
                .transpose()?
                .unwrap_or(0);
            includes.insert(
                offset,
                (
                    run.clone(),
                    step.when.clone(),
                    step.r#while.clone(),
                    repeat,
                    retries,
                ),
            );
        }
    }
    raw.steps.retain(|step| step.run.is_none());
    let filtered_cli = cli_vars
        .iter()
        .filter(|(name, _)| raw.vars.contains_key(*name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut flow = compile_raw_inner(
        raw,
        source_path.clone(),
        &filtered_cli,
        environment,
        passed_vars,
        false,
        true,
    )
    .map_err(|error| with_path(&source_path, error))?;
    if depth == 0 {
        state.browser_settings = Some((flow.settings.viewport, flow.settings.video));
    }

    let mut compiled = flow.steps.into_iter().peekable();
    let mut steps = Vec::new();
    for offset in 0..original_len {
        let local_index = offset + 1;
        if let Some((run, when, while_control, repeat, retries)) = includes.get(&offset) {
            let compiled_when = compile_when(when.clone(), local_index, &flow.inputs)?;
            let skip = matches!(compiled_when, CompiledWhen::Skip);
            let when_expression = match compiled_when {
                CompiledWhen::Runtime(When::Expression(expression)) => Some(expression),
                _ => None,
            };
            let while_control = while_control
                .clone()
                .map(|control| compile_while(control, local_index, &flow.inputs))
                .transpose()?;
            if depth == MAX_SUBFLOW_DEPTH {
                return invalid(format!(
                    "{}: step {local_index} exceeds maximum subflow depth {MAX_SUBFLOW_DEPTH}",
                    source_path.display()
                ));
            }
            let (run, raw_vars) = match run {
                RawRun::Path(path) => (path.as_str(), None),
                RawRun::Mapped(options) => (options.path.as_str(), Some(&options.vars)),
            };
            validate_subflow_path(run, &source_path, local_index)?;
            let child_path = source_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(run);
            let canonical = fs::canonicalize(&child_path).map_err(|source| FlowError::Io {
                path: child_path.clone(),
                source,
            })?;
            if let Some(position) = state
                .active
                .iter()
                .position(|(active, _)| active == &canonical)
            {
                let mut cycle = state.active[position..]
                    .iter()
                    .map(|(_, path)| path.display().to_string())
                    .collect::<Vec<_>>();
                cycle.push(child_path.display().to_string());
                return invalid(format!("subflow include cycle: {}", cycle.join(" -> ")));
            }
            state.active.push((canonical, child_path.clone()));
            let child_raw = read_flow(&child_path)?;
            let child_vars = raw_vars
                .map(|vars| compile_run_vars(vars, local_index, &flow.inputs))
                .transpose()?
                .unwrap_or_default();
            let child = compile_raw_expanded(
                child_raw,
                child_path,
                cli_vars,
                environment,
                &child_vars,
                depth + 1,
                state,
            );
            state.active.pop();
            let child = child?;
            flow.redactor.extend(&child.redactor);
            flow.manual_recording |= child.manual_recording;
            if *retries > 0
                && child
                    .steps
                    .iter()
                    .any(|step| !matches!(step.operation, Operation::Assert(_)))
            {
                return invalid(format!(
                    "{}: step {local_index} retry requires an assertion-only subflow",
                    source_path.display()
                ));
            }
            if skip {
                continue;
            }
            let mut first_when_step = true;
            for _ in 0..*repeat {
                let iterations = while_control
                    .as_ref()
                    .map_or(1, |(_, max_iterations)| *max_iterations);
                for iteration in 0..iterations {
                    for (child_offset, mut step) in child.steps.iter().cloned().enumerate() {
                        step.retries = step.retries.checked_add(*retries).filter(|value| *value <= MAX_RETRIES).ok_or_else(|| {
                            FlowError::Invalid(format!(
                                "{}: step {local_index} combined retry must not exceed {MAX_RETRIES}",
                                source_path.display()
                            ))
                        })?;
                        let mut guards = Vec::new();
                        if let Some(expression) = &when_expression {
                            guards.push(Guard {
                                id: 0,
                                first: first_when_step,
                                kind: GuardKind::When(expression.clone()),
                            });
                        }
                        if let Some((expression, _)) = &while_control {
                            guards.push(Guard {
                                id: 0,
                                first: child_offset == 0,
                                kind: GuardKind::While {
                                    loop_id: 0,
                                    new_loop: iteration == 0 && child_offset == 0,
                                    expression: expression.clone(),
                                },
                            });
                        }
                        guards.append(&mut step.guards);
                        step.guards = guards;
                        steps.push(step);
                        first_when_step = false;
                    }
                    if steps.len() > MAX_FLOW_STEPS {
                        return invalid(format!("expanded steps must not exceed {MAX_FLOW_STEPS}"));
                    }
                }
            }
        } else {
            while compiled
                .peek()
                .is_some_and(|step| step.source_index == local_index)
            {
                steps.push(compiled.next().expect("peeked step"));
            }
        }
        if steps.len() > MAX_FLOW_STEPS {
            return invalid(format!("expanded steps must not exceed {MAX_FLOW_STEPS}"));
        }
    }
    flow.steps = steps;
    Ok(flow)
}

fn validate_subflow_path(run: &str, source: &Path, index: usize) -> Result<(), FlowError> {
    require_scalar_size(&format!("step {index} run"), run)?;
    require_non_empty(&format!("step {index} run"), run)?;
    let path = Path::new(run);
    if path.is_absolute() {
        return invalid(format!(
            "{}: step {index} run must be relative to its containing flow",
            source.display()
        ));
    }
    if !is_subflow(path) {
        return invalid(format!(
            "{}: step {index} run must name a .subflow.yaml or .subflow.yml file",
            source.display()
        ));
    }
    Ok(())
}

fn validate_expanded_steps(steps: &mut [CompiledStep]) -> Result<(), FlowError> {
    validate_expanded_steps_with_outputs(steps, &BTreeSet::new())
}

fn validate_expanded_steps_with_outputs(
    steps: &mut [CompiledStep],
    available_outputs: &BTreeSet<String>,
) -> Result<(), FlowError> {
    let mut ids = BTreeSet::new();
    let mut screenshots = BTreeSet::new();
    let mut outputs = available_outputs
        .iter()
        .map(|name| (name.clone(), None))
        .collect::<BTreeMap<_, Option<(PathBuf, usize)>>>();
    let mut next_guard_id = 0;
    let mut next_loop_id = 0;
    let mut active_guards = Vec::<usize>::new();
    let mut active_loops = Vec::<Option<usize>>::new();
    for (offset, step) in steps.iter_mut().enumerate() {
        step.index = offset + 1;
        active_guards.truncate(step.guards.len());
        active_loops.truncate(step.guards.len());
        active_loops.resize(step.guards.len(), None);
        for (depth, guard) in step.guards.iter_mut().enumerate() {
            if guard.first {
                next_guard_id += 1;
                if active_guards.len() == depth {
                    active_guards.push(next_guard_id);
                } else {
                    active_guards[depth] = next_guard_id;
                }
            }
            guard.id = *active_guards.get(depth).ok_or_else(|| {
                FlowError::Invalid("invalid compiled control-flow guard".to_owned())
            })?;
            if let GuardKind::While {
                loop_id, new_loop, ..
            } = &mut guard.kind
            {
                if *new_loop {
                    next_loop_id += 1;
                    active_loops[depth] = Some(next_loop_id);
                }
                *loop_id =
                    active_loops.get(depth).copied().flatten().ok_or_else(|| {
                        FlowError::Invalid("invalid compiled while loop".to_owned())
                    })?;
            }
        }
        if let Some(id) = &step.id
            && !ids.insert(id.clone())
        {
            return invalid(format!("duplicate step id {id:?} in expanded flow"));
        }
        if let Operation::Screenshot { name, .. } = &step.operation
            && !screenshots.insert(name.to_ascii_lowercase())
        {
            return invalid(format!(
                "duplicate screenshot name {name:?} in expanded flow"
            ));
        }
        let runtime_values = step
            .guards
            .iter()
            .flat_map(|guard| expression_runtime_values(guard_expression(guard)))
            .chain(
                step.when
                    .iter()
                    .filter_map(|when| match when {
                        When::Expression(expression) => Some(expression),
                        _ => None,
                    })
                    .flat_map(expression_runtime_values),
            )
            .chain(operation_runtime_values(&step.operation));
        for name in runtime_values.flat_map(RuntimeValue::output_names) {
            if !outputs.contains_key(name) {
                return invalid(format!(
                    "step {} references unknown variable or runtime output {name:?} before it is saved",
                    step.index
                ));
            }
        }
        if let Some(name) = operation_save_as(&step.operation) {
            let producer = (step.source.clone(), step.source_index);
            if let Some(Some(existing)) = outputs.insert(name.to_owned(), Some(producer.clone()))
                && existing != producer
            {
                return invalid(format!("duplicate runtime output {name:?}"));
            }
        }
    }
    if steps.is_empty() {
        return invalid("expanded steps must not be empty");
    }
    let controls = steps
        .iter()
        .filter_map(|step| match step.operation {
            Operation::Recording(control) => Some((step.index, control)),
            _ => None,
        })
        .collect::<Vec<_>>();
    match controls.as_slice() {
        [] | [(_, RecordingControl::Start), (_, RecordingControl::Stop)] => {}
        [(step, RecordingControl::Stop), ..] => {
            return invalid(format!(
                "step {step} recording stop must follow recording start"
            ));
        }
        [(_, RecordingControl::Start)] => {
            return invalid("recording start requires one later recording stop");
        }
        _ => return invalid("a flow may contain only one recording start/stop pair"),
    }
    Ok(())
}

fn guard_expression(guard: &Guard) -> &Expression {
    match &guard.kind {
        GuardKind::When(expression) | GuardKind::While { expression, .. } => expression,
    }
}

fn expression_runtime_values(expression: &Expression) -> Vec<&RuntimeValue> {
    let mut values = Vec::new();
    collect_expression_runtime_values(expression, &mut values);
    values
}

fn collect_expression_runtime_values<'a>(
    expression: &'a Expression,
    values: &mut Vec<&'a RuntimeValue>,
) {
    match expression {
        Expression::All(children) | Expression::Any(children) => {
            for child in children {
                collect_expression_runtime_values(child, values);
            }
        }
        Expression::Not(child) => collect_expression_runtime_values(child, values),
        Expression::Equals(left, right) | Expression::NotEquals(left, right) => {
            values.extend([left, right]);
        }
        Expression::Boolean(value) => values.push(value),
    }
}

fn validate_page_switching_video(flow: &CompiledFlow) -> Result<(), FlowError> {
    if (flow.settings.video != VideoMode::Off
        || flow
            .steps
            .iter()
            .any(|step| matches!(step.operation, Operation::Recording(_))))
        && let Some(step) = flow
            .steps
            .iter()
            .find(|step| matches!(step.operation, Operation::SwitchPage(_)))
    {
        return invalid(format!(
            "step {} switch_page requires settings.video: off and no recording controls because recording cannot safely move between pages",
            step.index
        ));
    }
    Ok(())
}

fn operation_runtime_values(operation: &Operation) -> Box<dyn Iterator<Item = &RuntimeValue> + '_> {
    match operation {
        Operation::Fill { value, .. } | Operation::Select { value, .. } => {
            Box::new(std::iter::once(value))
        }
        Operation::Dialog {
            text: Some(value), ..
        } => Box::new(std::iter::once(value)),
        Operation::Evaluate { args, .. } => Box::new(args.iter()),
        Operation::Request {
            url, headers, body, ..
        } => Box::new(
            std::iter::once(url)
                .chain(headers.values())
                .chain(body.iter()),
        ),
        _ => Box::new(std::iter::empty()),
    }
}

fn operation_save_as(operation: &Operation) -> Option<&str> {
    match operation {
        Operation::Evaluate { save_as, .. } | Operation::Request { save_as, .. } => {
            save_as.as_deref()
        }
        _ => None,
    }
}

pub fn discover_flow_files(path: impl AsRef<Path>) -> Result<Vec<PathBuf>, FlowError> {
    let path = path.as_ref();
    let metadata = fs::metadata(path).map_err(|source| FlowError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_file() {
        if !is_yaml(path) {
            return invalid(format!("{} is not a .yaml or .yml file", path.display()));
        }
        return Ok(vec![path.to_owned()]);
    }
    if !metadata.is_dir() {
        return invalid(format!("{} is not a file or directory", path.display()));
    }

    let mut files = Vec::new();
    discover_directory(path, &mut files)?;
    files.sort_by_key(|path| normalized_path(path));
    if files.is_empty() {
        return invalid(format!("{} contains no YAML flow files", path.display()));
    }
    Ok(files)
}

pub fn artifact_key(root: impl AsRef<Path>, file: impl AsRef<Path>) -> String {
    let file = file.as_ref();
    let relative = file.strip_prefix(root.as_ref()).unwrap_or(file);
    let normalized = normalized_path(relative);
    let without_extension = normalized
        .strip_suffix(".yaml")
        .or_else(|| normalized.strip_suffix(".yml"))
        .unwrap_or(&normalized);
    let safe = without_extension
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches('-');
    let safe = if safe.is_empty() { "flow" } else { safe };
    let digest = Sha256::digest(normalized.as_bytes());
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{safe}-{hash}")
}

impl Redactor {
    fn from_inputs(inputs: &BTreeMap<String, Resolved<String>>) -> Self {
        let mut secrets = inputs
            .values()
            .filter(|value| value.secret)
            .map(|value| value.value.clone())
            .collect::<Vec<_>>();
        secrets.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        secrets.dedup();
        Self { secrets }
    }
}

enum CompiledWhen {
    Always,
    Skip,
    Runtime(When),
}

fn compile_when(
    raw: Option<RawWhen>,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<CompiledWhen, FlowError> {
    let Some(raw) = raw else {
        return Ok(CompiledWhen::Always);
    };
    let count = [
        raw.visible.is_some(),
        raw.hidden.is_some(),
        raw.variable.is_some(),
        raw.platform.is_some(),
        raw.expression.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if count != 1 {
        return invalid(format!(
            "step {index} when must contain exactly one predicate"
        ));
    }
    if let Some(locator) = raw.visible {
        return Ok(CompiledWhen::Runtime(When::Visible(compile_locator(
            locator, index, inputs,
        )?)));
    }
    if let Some(locator) = raw.hidden {
        return Ok(CompiledWhen::Runtime(When::Hidden(compile_locator(
            locator, index, inputs,
        )?)));
    }
    if raw.platform.is_some() {
        return Ok(CompiledWhen::Always);
    }
    if let Some(expression) = raw.expression {
        return Ok(CompiledWhen::Runtime(When::Expression(compile_expression(
            expression, index, inputs,
        )?)));
    }
    let predicate = raw.variable.expect("predicate count checked");
    validate_input_name(&predicate.name)?;
    let actual = inputs.get(&predicate.name).ok_or_else(|| {
        FlowError::Invalid(format!(
            "step {index} when.variable references unknown variable {:?}",
            predicate.name
        ))
    })?;
    if actual.is_secret() {
        return invalid(format!(
            "step {index} when.variable must reference a non-secret variable"
        ));
    }
    let expected = interpolate(
        &format!("step {index} when.variable.equals"),
        &predicate.equals,
        inputs,
    )?;
    if expected.is_secret() {
        return invalid(format!(
            "step {index} when.variable.equals cannot contain a secret"
        ));
    }
    Ok(if actual.expose() == expected.expose() {
        CompiledWhen::Always
    } else {
        CompiledWhen::Skip
    })
}

fn compile_while(
    control: RawWhile,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<(Expression, usize), FlowError> {
    let max_iterations = validate_count(
        "while.max_iterations",
        index,
        control.max_iterations,
        MAX_WHILE_ITERATIONS,
    )?;
    Ok((
        compile_expression(control.expression, index, inputs)?,
        max_iterations,
    ))
}

fn compile_expression(
    raw: RawExpression,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Expression, FlowError> {
    let mut nodes = 0;
    compile_expression_inner(raw, index, inputs, 0, &mut nodes)
}

fn compile_expression_inner(
    raw: RawExpression,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
    depth: usize,
    nodes: &mut usize,
) -> Result<Expression, FlowError> {
    if depth > MAX_EXPRESSION_DEPTH {
        return invalid(format!(
            "step {index} expression exceeds maximum depth {MAX_EXPRESSION_DEPTH}"
        ));
    }
    *nodes += 1;
    if *nodes > MAX_EXPRESSION_NODES {
        return invalid(format!(
            "step {index} expression exceeds maximum size {MAX_EXPRESSION_NODES}"
        ));
    }
    let count = [
        raw.all.is_some(),
        raw.any.is_some(),
        raw.not.is_some(),
        raw.equals.is_some(),
        raw.not_equals.is_some(),
        raw.boolean.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if count != 1 {
        return invalid(format!(
            "step {index} expression must contain exactly one operator"
        ));
    }
    let is_all = raw.all.is_some();
    if let Some(children) = raw.all.or(raw.any) {
        if children.is_empty() {
            return invalid(format!("step {index} expression list must not be empty"));
        }
        let children = children
            .into_iter()
            .map(|child| compile_expression_inner(child, index, inputs, depth + 1, nodes))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(if is_all {
            Expression::All(children)
        } else {
            Expression::Any(children)
        });
    }
    if let Some(child) = raw.not {
        return Ok(Expression::Not(Box::new(compile_expression_inner(
            *child,
            index,
            inputs,
            depth + 1,
            nodes,
        )?)));
    }
    let is_equals = raw.equals.is_some();
    let comparison = raw.equals.or(raw.not_equals);
    if let Some(comparison) = comparison {
        let left = compile_runtime_value(
            &format!("step {index} expression.left"),
            &comparison.left,
            inputs,
        )?;
        let right = compile_runtime_value(
            &format!("step {index} expression.right"),
            &comparison.right,
            inputs,
        )?;
        return Ok(if is_equals {
            Expression::Equals(left, right)
        } else {
            Expression::NotEquals(left, right)
        });
    }
    Ok(Expression::Boolean(compile_runtime_value(
        &format!("step {index} expression.boolean"),
        raw.boolean.as_deref().expect("operator count checked"),
        inputs,
    )?))
}

fn compile_run_vars(
    vars: &BTreeMap<String, String>,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<BTreeMap<String, Resolved<String>>, FlowError> {
    vars.iter()
        .map(|(name, value)| {
            validate_input_name(name)?;
            let value = interpolate(&format!("step {index} run.vars.{name}"), value, inputs)?;
            require_non_empty(&format!("step {index} run.vars.{name}"), value.expose())?;
            Ok((name.clone(), value))
        })
        .collect()
}

fn validate_count(
    field: &str,
    index: usize,
    value: usize,
    maximum: usize,
) -> Result<usize, FlowError> {
    if value == 0 || value > maximum {
        return invalid(format!(
            "step {index} {field} must be between 1 and {maximum}",
        ));
    }
    Ok(value)
}

fn resolve_inputs(
    vars: &BTreeMap<String, RawVariable>,
    secrets: &BTreeMap<String, RawSecret>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    passed_vars: &BTreeMap<String, Resolved<String>>,
    reject_unknown_cli_vars: bool,
) -> Result<BTreeMap<String, Resolved<String>>, FlowError> {
    for name in vars.keys().chain(secrets.keys()) {
        validate_input_name(name)?;
    }
    for (name, raw) in vars {
        match raw {
            RawVariable::Literal(value) => require_scalar_size(&format!("vars.{name}"), value)?,
            RawVariable::Environment(raw) => {
                require_scalar_size(&format!("vars.{name}.env"), &raw.env)?;
                if let Some(default) = &raw.default {
                    require_scalar_size(&format!("vars.{name}.default"), default)?;
                }
            }
        }
    }
    for (name, raw) in secrets {
        require_scalar_size(&format!("secrets.{name}.env"), &raw.env)?;
    }
    if let Some(name) = vars.keys().find(|name| secrets.contains_key(*name)) {
        return invalid(format!(
            "input {name:?} is declared in both vars and secrets"
        ));
    }
    if reject_unknown_cli_vars
        && let Some(name) = cli_vars.keys().find(|name| !vars.contains_key(*name))
    {
        return invalid(format!("CLI variable {name:?} is not declared under vars"));
    }
    if let Some(name) = passed_vars.keys().find(|name| !vars.contains_key(*name)) {
        return invalid(format!("run variable {name:?} is not declared under vars"));
    }

    let mut resolved = BTreeMap::new();
    for (name, raw) in vars {
        let value = if let Some(value) = passed_vars.get(name) {
            require_scalar_size(&format!("vars.{name}"), value.expose())?;
            require_non_empty(&format!("vars.{name}"), value.expose())?;
            resolved.insert(name.clone(), value.clone());
            continue;
        } else if let Some(value) = cli_vars.get(name) {
            value.clone()
        } else {
            match raw {
                RawVariable::Literal(value) => value.clone(),
                RawVariable::Environment(raw) => {
                    require_non_empty(&format!("vars.{name}.env"), &raw.env)?;
                    environment
                        .get(&raw.env)
                        .cloned()
                        .or_else(|| raw.default.clone())
                        .ok_or_else(|| {
                            FlowError::Invalid(format!(
                                "vars.{name} requires environment variable {}",
                                raw.env
                            ))
                        })?
                }
            }
        };
        require_scalar_size(&format!("vars.{name}"), &value)?;
        require_non_empty(&format!("vars.{name}"), &value)?;
        resolved.insert(name.clone(), Resolved::new(value, false));
    }
    for (name, raw) in secrets {
        require_non_empty(&format!("secrets.{name}.env"), &raw.env)?;
        let value = environment.get(&raw.env).cloned().ok_or_else(|| {
            FlowError::Invalid(format!(
                "secrets.{name} requires environment variable {}",
                raw.env
            ))
        })?;
        require_scalar_size(&format!("secrets.{name}"), &value)?;
        require_non_empty(&format!("secrets.{name}"), &value)?;
        resolved.insert(name.clone(), Resolved::new(value, true));
    }
    Ok(resolved)
}

fn compile_operation(
    step: RawStep,
    index: usize,
    source: &Path,
    base_url: Option<&Resolved<Url>>,
    viewport: Viewport,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Operation, FlowError> {
    if step.operation_count() != 1 {
        return invalid(format!("step {index} must contain exactly one operation"));
    }

    if let Some(raw) = step.open {
        let (raw_url, raw_wait_until) = match raw {
            RawOpen::Url(url) => (url, None),
            RawOpen::Options(options) => (options.url, options.wait_until),
        };
        let value = interpolate(&format!("step {index} open.url"), &raw_url, inputs)?;
        require_non_empty(&format!("step {index} open.url"), value.expose())?;
        let (url, base_secret) = match Url::parse(value.expose()) {
            Ok(url) => (
                validate_http_url(&format!("step {index} open"), url)?,
                false,
            ),
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                let base = base_url.ok_or_else(|| {
                    FlowError::Invalid(format!(
                        "step {index} has a relative open URL but base_url is not set"
                    ))
                })?;
                let url = base.expose().join(value.expose()).map_err(|_| {
                    FlowError::Invalid(format!("step {index} open is not a valid URL"))
                })?;
                (
                    validate_http_url(&format!("step {index} open"), url)?,
                    base.secret,
                )
            }
            Err(_) => return invalid(format!("step {index} open is not a valid URL")),
        };
        let wait_until = raw_wait_until
            .map(|wait| match (wait.visible, wait.stable) {
                (Some(_), Some(_)) => Err(FlowError::Invalid(format!(
                    "step {index} open.wait_until must contain exactly one condition"
                ))),
                (Some(target), None) => Ok(OpenWaitUntil::Visible(compile_locator(
                    target, index, inputs,
                )?)),
                (None, Some(target)) => Ok(OpenWaitUntil::Stable(compile_locator(
                    target, index, inputs,
                )?)),
                (None, None) => Err(FlowError::Invalid(format!(
                    "step {index} open.wait_until must contain visible or stable"
                ))),
            })
            .transpose()?;
        return Ok(Operation::Open {
            url: Resolved::new(url, value.secret || base_secret),
            wait_until,
        });
    }
    if let Some(raw) = step.click {
        if let Some(point) = raw.point {
            if raw.target.is_some() || raw.position.is_some() {
                return invalid(format!(
                    "step {index} click point cannot be combined with target or position"
                ));
            }
            if point.x >= viewport.width || point.y >= viewport.height {
                return invalid(format!(
                    "step {index} click point ({}, {}) is outside viewport {}x{}",
                    point.x, point.y, viewport.width, viewport.height
                ));
            }
            return Ok(Operation::ClickPoint { point });
        }
        let target = raw.target.ok_or_else(|| {
            FlowError::Invalid(format!(
                "step {index} click requires exactly one of target or point"
            ))
        })?;
        return Ok(Operation::Click {
            target: compile_locator(target, index, inputs)?,
            position: raw.position,
        });
    }
    if let Some(raw) = step.double_click {
        if raw.point.is_some() {
            return invalid(format!("step {index} double_click does not support point"));
        }
        return Ok(Operation::DoubleClick {
            target: compile_locator(
                raw.target.ok_or_else(|| {
                    FlowError::Invalid(format!("step {index} double_click requires target"))
                })?,
                index,
                inputs,
            )?,
            position: raw.position,
        });
    }
    if let Some(raw) = step.fill {
        let value = compile_runtime_value(&format!("step {index} fill.value"), &raw.value, inputs)?;
        if value.outputs.is_empty() {
            require_non_empty(&format!("step {index} fill.value"), value.template.expose())?;
        }
        return Ok(Operation::Fill {
            target: compile_locator(raw.target, index, inputs)?,
            value,
        });
    }
    if let Some(raw) = step.erase {
        return Ok(Operation::Erase {
            target: compile_locator(raw.target, index, inputs)?,
        });
    }
    if let Some(raw) = step.select {
        let value =
            compile_runtime_value(&format!("step {index} select.value"), &raw.value, inputs)?;
        return Ok(Operation::Select {
            target: compile_locator(raw.target, index, inputs)?,
            value,
        });
    }
    if let Some(raw) = step.scroll {
        if raw.x == 0 && raw.y == 0 {
            return invalid(format!("step {index} scroll requires a non-zero x or y"));
        }
        return Ok(Operation::Scroll { x: raw.x, y: raw.y });
    }
    if let Some(raw) = step.scroll_until_visible {
        validate_gesture_delta(index, "scroll_until_visible", raw.x, raw.y)?;
        return Ok(Operation::ScrollUntilVisible {
            target: compile_locator(raw.target, index, inputs)?,
            x: raw.x,
            y: raw.y,
        });
    }
    if let Some(raw) = step.swipe {
        validate_gesture_delta(index, "swipe", raw.x, raw.y)?;
        return Ok(Operation::Swipe {
            target: compile_locator(raw.target, index, inputs)?,
            x: raw.x,
            y: raw.y,
            duration: compile_gesture_duration(
                index,
                "swipe",
                raw.duration.as_deref(),
                DEFAULT_SWIPE_DURATION,
                inputs,
            )?,
        });
    }
    if let Some(raw) = step.long_press {
        return Ok(Operation::LongPress {
            target: compile_locator(raw.target, index, inputs)?,
            duration: compile_gesture_duration(
                index,
                "long_press",
                raw.duration.as_deref(),
                DEFAULT_LONG_PRESS_DURATION,
                inputs,
            )?,
        });
    }
    if let Some(raw) = step.wait_until_visible {
        return Ok(Operation::WaitUntilVisible {
            target: compile_locator(raw.target, index, inputs)?,
        });
    }
    if let Some(raw) = step.wait_until_stable {
        return Ok(Operation::WaitUntilStable {
            target: compile_locator(raw.target, index, inputs)?,
        });
    }
    if step.back.is_some() {
        return Ok(Operation::Back);
    }
    if let Some(page) = step.switch_page {
        return Ok(Operation::SwitchPage(match page {
            RawPageSwitch::Location(PageLocation::Popup) => PageSwitch::Popup,
            RawPageSwitch::Location(PageLocation::Opener) => PageSwitch::Opener,
            RawPageSwitch::Selector(raw) => match (raw.name, raw.url) {
                (Some(name), None) => {
                    let name =
                        interpolate(&format!("step {index} switch_page.name"), &name, inputs)?;
                    require_non_empty(&format!("step {index} switch_page.name"), name.expose())?;
                    PageSwitch::Name(name)
                }
                (None, Some(raw)) => {
                    let value =
                        interpolate(&format!("step {index} switch_page.url"), &raw, inputs)?;
                    require_non_empty(&format!("step {index} switch_page.url"), value.expose())?;
                    let (url, base_secret) = match Url::parse(value.expose()) {
                        Ok(url) => (
                            validate_http_url(&format!("step {index} switch_page.url"), url)?,
                            false,
                        ),
                        Err(url::ParseError::RelativeUrlWithoutBase) => {
                            let base = base_url.ok_or_else(|| {
                                FlowError::Invalid(format!(
                                    "step {index} has a relative switch_page URL but base_url is not set"
                                ))
                            })?;
                            let url = base.expose().join(value.expose()).map_err(|_| {
                                FlowError::Invalid(format!(
                                    "step {index} switch_page.url is not a valid URL"
                                ))
                            })?;
                            (
                                validate_http_url(&format!("step {index} switch_page.url"), url)?,
                                base.secret,
                            )
                        }
                        Err(_) => {
                            return invalid(format!(
                                "step {index} switch_page.url is not a valid URL"
                            ));
                        }
                    };
                    PageSwitch::Url(Resolved::new(url, value.secret || base_secret))
                }
                _ => {
                    return invalid(format!(
                        "step {index} switch_page must contain exactly one of name or url"
                    ));
                }
            },
        }));
    }
    if let Some(frame) = step.switch_frame {
        return Ok(Operation::SwitchFrame(match frame {
            RawFrameSwitch::Target(raw) => {
                FrameSwitch::Target(compile_locator(raw.target, index, inputs)?)
            }
            RawFrameSwitch::Location(FrameLocation::Main) => FrameSwitch::Main,
            RawFrameSwitch::Location(FrameLocation::Parent) => FrameSwitch::Parent,
        }));
    }
    if let Some(raw) = step.press {
        let key = interpolate_non_secret(&format!("step {index} press.key"), &raw.key, inputs)?;
        let mut unique = BTreeSet::new();
        let mut modifiers = Vec::with_capacity(raw.modifiers.len());
        for raw_modifier in raw.modifiers {
            let value = interpolate_non_secret(
                &format!("step {index} press.modifiers"),
                &raw_modifier,
                inputs,
            )?;
            let modifier = parse_modifier(index, &value)?;
            if !unique.insert(modifier) {
                return invalid(format!(
                    "step {index} contains duplicate modifier {value:?}"
                ));
            }
            modifiers.push(modifier);
        }
        return Ok(Operation::Press {
            target: compile_locator(raw.target, index, inputs)?,
            key: parse_key(index, &key)?,
            modifiers,
        });
    }
    if let Some(raw) = step.screenshot {
        let name =
            interpolate_non_secret(&format!("step {index} screenshot.name"), &raw.name, inputs)?;
        validate_screenshot_name(index, &name)?;
        let crop = raw
            .crop
            .map(|crop| validate_crop(index, crop, viewport))
            .transpose()?;
        return Ok(Operation::Screenshot { name, crop });
    }
    if let Some(control) = step.recording {
        return Ok(Operation::Recording(control));
    }
    if let Some(raw) = step.dialog {
        if raw.text.is_some() && raw.action != NativeDialogResponse::Accept {
            return invalid(format!(
                "step {index} dialog text is only valid with action accept"
            ));
        }
        return Ok(Operation::Dialog {
            action: raw.action,
            text: raw
                .text
                .as_deref()
                .map(|text| {
                    compile_runtime_value(&format!("step {index} dialog.text"), text, inputs)
                })
                .transpose()?,
        });
    }
    if let Some(target) = step.clear {
        return Ok(Operation::Clear(target));
    }
    if let Some(raw) = step.evaluate {
        require_scalar_size(&format!("step {index} evaluate.script"), &raw.script)?;
        require_non_empty(&format!("step {index} evaluate.script"), &raw.script)?;
        let save_as = compile_save_as(index, raw.save_as, inputs)?;
        let args = raw
            .args
            .iter()
            .enumerate()
            .map(|(offset, value)| {
                compile_runtime_value(
                    &format!("step {index} evaluate.args[{offset}]"),
                    value,
                    inputs,
                )
            })
            .collect::<Result<_, _>>()?;
        return Ok(Operation::Evaluate {
            script: raw.script,
            args,
            save_as,
        });
    }
    if let Some(raw) = step.request {
        if raw.headers.len() > MAX_HTTP_HEADERS {
            return invalid(format!(
                "step {index} request.headers must not exceed {MAX_HTTP_HEADERS} entries"
            ));
        }
        let method = raw.method.to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ) {
            return invalid(format!("step {index} request.method is unsupported"));
        }
        if !(100..=599).contains(&raw.expected_status) {
            return invalid(format!(
                "step {index} request.expected_status must be between 100 and 599"
            ));
        }
        let url = compile_runtime_value(&format!("step {index} request.url"), &raw.url, inputs)?;
        if url.outputs.is_empty() {
            parse_absolute_url(&format!("step {index} request.url"), url.template.clone())?;
        }
        let mut headers = BTreeMap::new();
        for (name, value) in raw.headers {
            require_scalar_size(&format!("step {index} request header name"), &name)?;
            require_non_empty(&format!("step {index} request header name"), &name)?;
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                FlowError::Invalid(format!("step {index} request header name is invalid"))
            })?;
            headers.insert(
                name,
                compile_runtime_value(&format!("step {index} request header"), &value, inputs)?,
            );
        }
        let body = raw
            .body
            .as_deref()
            .map(|value| {
                compile_runtime_value(&format!("step {index} request.body"), value, inputs)
            })
            .transpose()?;
        return Ok(Operation::Request {
            method,
            url,
            headers,
            body,
            expected_status: raw.expected_status,
            save_as: compile_save_as(index, raw.save_as, inputs)?,
        });
    }
    Ok(Operation::Assert(compile_assertion(
        step.assertion.expect("operation count checked"),
        index,
        source,
        viewport,
        inputs,
    )?))
}

fn validate_gesture_delta(index: usize, operation: &str, x: i32, y: i32) -> Result<(), FlowError> {
    if x == 0 && y == 0 {
        return invalid(format!(
            "step {index} {operation} requires a non-zero x or y"
        ));
    }
    if x.unsigned_abs() > MAX_GESTURE_DELTA as u32 || y.unsigned_abs() > MAX_GESTURE_DELTA as u32 {
        return invalid(format!(
            "step {index} {operation} x and y must be between -{MAX_GESTURE_DELTA} and {MAX_GESTURE_DELTA}"
        ));
    }
    Ok(())
}

fn compile_gesture_duration(
    index: usize,
    operation: &str,
    raw: Option<&str>,
    default: Duration,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Duration, FlowError> {
    let context = format!("step {index} {operation}.duration");
    let duration = raw
        .map(|value| interpolate(&context, value, inputs))
        .transpose()?
        .map(|value| parse_duration(&context, value.expose()))
        .transpose()?
        .unwrap_or(default);
    if duration > MAX_GESTURE_DURATION {
        return invalid(format!(
            "{context} must not exceed {} seconds",
            MAX_GESTURE_DURATION.as_secs()
        ));
    }
    Ok(duration)
}

fn validate_screenshot_name(index: usize, name: &str) -> Result<(), FlowError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.eq_ignore_ascii_case("failure")
        && !is_windows_reserved_name(name)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if !valid {
        return invalid(format!(
            "step {index} screenshot.name must be 1-64 ASCII letters, numbers, '-' or '_', starting and ending with a letter or number, and cannot be 'failure' or a reserved filename"
        ));
    }
    Ok(())
}

fn is_windows_reserved_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            })
        })
}

fn validate_crop(index: usize, crop: RawCrop, viewport: Viewport) -> Result<Crop, FlowError> {
    if crop.width == 0 || crop.height == 0 {
        return invalid(format!(
            "step {index} screenshot.crop width and height must be greater than zero"
        ));
    }
    if crop
        .x
        .checked_add(crop.width)
        .is_none_or(|right| right > viewport.width)
        || crop
            .y
            .checked_add(crop.height)
            .is_none_or(|bottom| bottom > viewport.height)
    {
        return invalid(format!(
            "step {index} screenshot.crop must fit within the {}x{} viewport",
            viewport.width, viewport.height
        ));
    }
    Ok(Crop {
        x: crop.x,
        y: crop.y,
        width: crop.width,
        height: crop.height,
    })
}

fn compile_assertion(
    raw: RawAssertion,
    index: usize,
    source: &Path,
    viewport: Viewport,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Assertion, FlowError> {
    let assertion_count = [
        raw.visible.is_some(),
        raw.hidden.is_some(),
        raw.text.is_some(),
        raw.url.is_some(),
        raw.screenshot.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if assertion_count != 1 {
        return invalid(format!(
            "step {index} assert must contain exactly one assertion"
        ));
    }
    if let Some(locator) = raw.visible {
        return Ok(Assertion::Visible(compile_locator(locator, index, inputs)?));
    }
    if let Some(locator) = raw.hidden {
        return Ok(Assertion::Hidden(compile_locator(locator, index, inputs)?));
    }
    if let Some(text) = raw.text {
        let (expected, match_kind) = match (text.equals, text.contains) {
            (Some(value), None) => (value, TextMatch::Exact),
            (None, Some(value)) => (value, TextMatch::Contains),
            _ => {
                return invalid(format!(
                    "step {index} text assertion requires exactly one of equals or contains"
                ));
            }
        };
        let expected = interpolate(
            &format!("step {index} text assertion value"),
            &expected,
            inputs,
        )?;
        require_non_empty(
            &format!("step {index} text assertion value"),
            expected.expose(),
        )?;
        return Ok(Assertion::Text {
            target: compile_locator(text.target, index, inputs)?,
            expected,
            match_kind,
        });
    }

    if let Some(screenshot) = raw.screenshot {
        let baseline = interpolate_non_secret(
            &format!("step {index} screenshot assertion baseline"),
            &screenshot.baseline,
            inputs,
        )?;
        let baseline = validate_baseline_path(index, source, &baseline)?;
        let crop = screenshot
            .crop
            .map(|crop| validate_crop(index, crop, viewport))
            .transpose()?;
        let (width, height) = crop
            .map(|crop| (crop.width, crop.height))
            .unwrap_or((viewport.width, viewport.height));
        crate::visual::validate_dimensions(width, height)
            .map_err(|message| FlowError::Invalid(format!("step {index} {message}")))?;
        let max_changed_ratio = screenshot.max_changed_ratio.unwrap_or(0.0);
        if !max_changed_ratio.is_finite() || !(0.0..=1.0).contains(&max_changed_ratio) {
            return invalid(format!(
                "step {index} screenshot assertion max_changed_ratio must be between 0 and 1"
            ));
        }
        return Ok(Assertion::Screenshot(VisualExpectation {
            baseline,
            crop,
            channel_tolerance: screenshot.channel_tolerance.unwrap_or(0),
            max_changed_ratio,
        }));
    }

    let url = raw.url.expect("assertion count checked");
    let expectation = match (url.equals, url.path) {
        (Some(value), None) => {
            let value = interpolate(&format!("step {index} URL equals"), &value, inputs)?;
            UrlExpectation::Equals(parse_absolute_url(
                &format!("step {index} URL equals"),
                value,
            )?)
        }
        (None, Some(value)) => {
            let value = interpolate(&format!("step {index} URL path"), &value, inputs)?;
            UrlExpectation::Path(parse_url_path(index, value)?)
        }
        _ => {
            return invalid(format!(
                "step {index} URL assertion requires exactly one of equals or path"
            ));
        }
    };
    Ok(Assertion::Url(expectation))
}

fn validate_baseline_path(index: usize, source: &Path, value: &str) -> Result<PathBuf, FlowError> {
    require_non_empty(
        &format!("step {index} screenshot assertion baseline"),
        value,
    )?;
    let path = Path::new(value);
    if path.extension().and_then(|value| value.to_str()) != Some("png")
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return invalid(format!(
            "step {index} screenshot assertion baseline must be a relative .png path without '..'"
        ));
    }
    Ok(source.parent().unwrap_or_else(|| Path::new(".")).join(path))
}

fn compile_locator(
    raw: RawLocator,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Locator, FlowError> {
    compile_locator_at(raw, index, inputs, 0)
}

fn compile_locator_at(
    raw: RawLocator,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
    depth: usize,
) -> Result<Locator, FlowError> {
    let strategy_count = [
        raw.css.is_some(),
        raw.test_id.is_some(),
        raw.text.is_some(),
        raw.label.is_some(),
        raw.role.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if strategy_count != 1 {
        return invalid(format!(
            "step {index} locator must contain exactly one strategy"
        ));
    }
    let context = format!("step {index} locator");
    let strategy = if let Some(value) = raw.css {
        LocatorStrategy::Css(interpolate_non_empty(&context, &value, inputs)?)
    } else if let Some(value) = raw.test_id {
        LocatorStrategy::TestId(interpolate_non_empty(&context, &value, inputs)?)
    } else if let Some(text) = raw.text {
        let (value, match_kind) = match text {
            RawTextLocator::Scalar(value) => (value, TextMatch::Exact),
            RawTextLocator::Detailed(options) => (
                options.value,
                options.match_kind.unwrap_or(TextMatch::Exact),
            ),
        };
        LocatorStrategy::Text {
            value: interpolate_non_empty(&context, &value, inputs)?,
            match_kind,
        }
    } else if let Some(value) = raw.label {
        LocatorStrategy::Label(interpolate_non_empty(&context, &value, inputs)?)
    } else {
        let role = raw.role.expect("strategy count checked");
        LocatorStrategy::Role {
            value: interpolate_non_empty(&context, &role.value, inputs)?,
            name: role
                .name
                .as_deref()
                .map(|value| interpolate_non_empty(&context, value, inputs))
                .transpose()?,
        }
    };
    let mut relations = Vec::new();
    for (kind, relation) in [
        (RelationKind::Within, raw.within),
        (RelationKind::ChildOf, raw.child_of),
        (RelationKind::Has, raw.has),
        (RelationKind::Above, raw.above),
        (RelationKind::Below, raw.below),
        (RelationKind::Left, raw.left),
        (RelationKind::Right, raw.right),
    ] {
        if let Some(relation) = relation {
            if depth == MAX_LOCATOR_DEPTH {
                return invalid(format!(
                    "step {index} locator exceeds maximum relation depth {MAX_LOCATOR_DEPTH}"
                ));
            }
            relations.push(LocatorRelation {
                kind,
                locator: Box::new(compile_locator_at(*relation, index, inputs, depth + 1)?),
            });
        }
    }
    Ok(Locator {
        strategy,
        index: raw.index,
        checked: raw.checked,
        selected: raw.selected,
        focused: raw.focused,
        enabled: raw.enabled,
        relations,
    })
}

fn interpolate_non_empty(
    context: &str,
    source: &str,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Resolved<String>, FlowError> {
    let value = interpolate(context, source, inputs)?;
    require_non_empty(context, value.expose())?;
    Ok(value)
}

fn compile_save_as(
    index: usize,
    save_as: Option<String>,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Option<String>, FlowError> {
    let Some(save_as) = save_as else {
        return Ok(None);
    };
    require_scalar_size(&format!("step {index} save_as"), &save_as)?;
    validate_input_name(&save_as)
        .map_err(|_| FlowError::Invalid(format!("step {index} save_as is not a valid name")))?;
    if inputs.contains_key(&save_as) {
        return invalid(format!(
            "step {index} save_as conflicts with an input named {save_as:?}"
        ));
    }
    Ok(Some(save_as))
}

fn compile_runtime_value(
    context: &str,
    source: &str,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<RuntimeValue, FlowError> {
    require_scalar_size(context, source)?;
    let mut output = String::with_capacity(source.len());
    let mut outputs = BTreeSet::new();
    let mut parts = Vec::new();
    let mut secret = false;
    let mut remaining = source;
    while let Some(start) = remaining.find("${") {
        let literal = &remaining[..start];
        push_interpolated(context, &mut output, literal)?;
        if !literal.is_empty() {
            parts.push(RuntimeValuePart::Literal(Resolved::new(
                literal.to_owned(),
                false,
            )));
        }
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .ok_or_else(|| FlowError::Invalid(format!("{context} has an unterminated variable")))?;
        let name = &after_start[..end];
        validate_input_name(name).map_err(|_| {
            FlowError::Invalid(format!("{context} contains invalid variable reference"))
        })?;
        if let Some(value) = inputs.get(name) {
            push_interpolated(context, &mut output, value.expose())?;
            secret |= value.secret;
            parts.push(RuntimeValuePart::Literal(value.clone()));
        } else {
            outputs.insert(name.to_owned());
            push_interpolated(context, &mut output, &format!("${{{name}}}"))?;
            parts.push(RuntimeValuePart::Output(name.to_owned()));
        }
        remaining = &after_start[end + 1..];
    }
    push_interpolated(context, &mut output, remaining)?;
    if !remaining.is_empty() {
        parts.push(RuntimeValuePart::Literal(Resolved::new(
            remaining.to_owned(),
            false,
        )));
    }
    Ok(RuntimeValue {
        template: Resolved::new(output, secret),
        parts,
        outputs,
    })
}

fn interpolate_non_secret(
    context: &str,
    source: &str,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<String, FlowError> {
    let value = interpolate(context, source, inputs)?;
    if value.secret {
        return invalid(format!("{context} cannot contain a secret"));
    }
    Ok(value.value)
}

fn interpolate(
    context: &str,
    source: &str,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Resolved<String>, FlowError> {
    require_scalar_size(context, source)?;
    let mut output = String::with_capacity(source.len());
    let mut secret = false;
    let mut remaining = source;
    while let Some(start) = remaining.find("${") {
        push_interpolated(context, &mut output, &remaining[..start])?;
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .ok_or_else(|| FlowError::Invalid(format!("{context} has an unterminated variable")))?;
        let name = &after_start[..end];
        validate_input_name(name).map_err(|_| {
            FlowError::Invalid(format!("{context} contains invalid variable reference"))
        })?;
        let value = inputs.get(name).ok_or_else(|| {
            FlowError::Invalid(format!("{context} references unknown variable {name:?}"))
        })?;
        push_interpolated(context, &mut output, &value.value)?;
        secret |= value.secret;
        remaining = &after_start[end + 1..];
    }
    push_interpolated(context, &mut output, remaining)?;
    Ok(Resolved::new(output, secret))
}

fn push_interpolated(context: &str, output: &mut String, value: &str) -> Result<(), FlowError> {
    if output
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > MAX_SCALAR_BYTES)
    {
        return invalid(format!(
            "{context} exceeds the maximum scalar size of {MAX_SCALAR_BYTES} bytes"
        ));
    }
    output.push_str(value);
    Ok(())
}

pub fn parse_duration(context: &str, value: &str) -> Result<Duration, FlowError> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u128)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000_u128)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000_u128)
    } else {
        return invalid(format!("{context} must use ms, s, or m"));
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return invalid(format!("{context} is not a valid duration"));
    }
    let number = number
        .parse::<u128>()
        .map_err(|_| FlowError::Invalid(format!("{context} is not a valid duration")))?;
    let milliseconds = number
        .checked_mul(multiplier)
        .filter(|value| *value > 0)
        .ok_or_else(|| FlowError::Invalid(format!("{context} is outside the supported range")))?;
    if milliseconds > MAX_TIMEOUT.as_millis() {
        return invalid(format!(
            "{context} must not exceed {} seconds",
            MAX_TIMEOUT.as_secs()
        ));
    }
    Ok(Duration::from_millis(milliseconds as u64))
}

fn parse_absolute_url(context: &str, value: Resolved<String>) -> Result<Resolved<Url>, FlowError> {
    let url = Url::parse(value.expose())
        .map_err(|_| FlowError::Invalid(format!("{context} must be an absolute URL")))?;
    Ok(Resolved::new(
        validate_http_url(context, url)?,
        value.secret,
    ))
}

fn validate_http_url(context: &str, url: Url) -> Result<Url, FlowError> {
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return invalid(format!("{context} must use an absolute http or https URL"));
    }
    Ok(url)
}

fn parse_url_path(index: usize, value: Resolved<String>) -> Result<Resolved<String>, FlowError> {
    if !value.expose().starts_with('/') || value.expose().starts_with("//") {
        return invalid(format!("step {index} URL path must start with one slash"));
    }
    let parsed = Url::parse("https://playrust.invalid")
        .expect("constant URL is valid")
        .join(value.expose())
        .map_err(|_| FlowError::Invalid(format!("step {index} URL path is invalid")))?;
    if parsed.fragment().is_some() {
        return invalid(format!("step {index} URL path must not include a fragment"));
    }
    let mut normalized = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        normalized.push('?');
        normalized.push_str(query);
    }
    Ok(Resolved::new(normalized, value.secret))
}

fn parse_key(index: usize, value: &str) -> Result<Key, FlowError> {
    let named = match value {
        "Enter" => Some(NamedKey::Enter),
        "Tab" => Some(NamedKey::Tab),
        "Escape" => Some(NamedKey::Escape),
        "Space" => Some(NamedKey::Space),
        "Backspace" => Some(NamedKey::Backspace),
        "Delete" => Some(NamedKey::Delete),
        "ArrowUp" => Some(NamedKey::ArrowUp),
        "ArrowDown" => Some(NamedKey::ArrowDown),
        "ArrowLeft" => Some(NamedKey::ArrowLeft),
        "ArrowRight" => Some(NamedKey::ArrowRight),
        "Home" => Some(NamedKey::Home),
        "End" => Some(NamedKey::End),
        "PageUp" => Some(NamedKey::PageUp),
        "PageDown" => Some(NamedKey::PageDown),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(Key::Named(named));
    }
    let mut characters = value.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if !character.is_control() && !character.is_whitespace() => {
            Ok(Key::Character(character))
        }
        _ => invalid(format!("step {index} has unsupported key {value:?}")),
    }
}

fn parse_modifier(index: usize, value: &str) -> Result<Modifier, FlowError> {
    match value {
        "Alt" => Ok(Modifier::Alt),
        "Control" => Ok(Modifier::Control),
        "Meta" => Ok(Modifier::Meta),
        "Shift" => Ok(Modifier::Shift),
        _ => invalid(format!("step {index} has unsupported modifier {value:?}")),
    }
}

fn validate_input_name(name: &str) -> Result<(), FlowError> {
    require_scalar_size("input name", name)?;
    let mut bytes = name.bytes();
    if !matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return invalid(format!("invalid input name {name:?}"));
    }
    Ok(())
}

fn require_source_size(length: usize) -> Result<(), FlowError> {
    if length > MAX_FLOW_SOURCE_BYTES {
        return invalid(format!(
            "flow source exceeds the maximum size of {MAX_FLOW_SOURCE_BYTES} bytes"
        ));
    }
    Ok(())
}

fn require_scalar_size(context: &str, value: &str) -> Result<(), FlowError> {
    if value.len() > MAX_SCALAR_BYTES {
        return invalid(format!(
            "{context} exceeds the maximum scalar size of {MAX_SCALAR_BYTES} bytes"
        ));
    }
    Ok(())
}

fn require_non_empty(context: &str, value: &str) -> Result<(), FlowError> {
    if value.trim().is_empty() {
        return invalid(format!("{context} must not be empty"));
    }
    Ok(())
}

fn discover_directory(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), FlowError> {
    let entries = fs::read_dir(directory).map_err(|source| FlowError::Io {
        path: directory.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| FlowError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| FlowError::Io {
            path: entry.path(),
            source,
        })?;
        if file_type.is_dir() {
            discover_directory(&entry.path(), files)?;
        } else if file_type.is_file() && is_yaml(&entry.path()) && !is_subflow(&entry.path()) {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("yaml" | "yml")
    )
}

fn is_subflow(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".subflow.yaml") || name.ends_with(".subflow.yml"))
}

fn with_path(path: &Path, error: FlowError) -> FlowError {
    match error {
        FlowError::Yaml(message) => FlowError::Yaml(format!("{}: {message}", path.display())),
        FlowError::Invalid(message) => FlowError::Invalid(format!("{}: {message}", path.display())),
        error @ FlowError::Io { .. } => error,
    }
}

fn normalized_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn invalid<T>(message: impl Into<String>) -> Result<T, FlowError> {
    Err(FlowError::Invalid(message.into()))
}

#[cfg(test)]
mod tests;
