use super::compiled::PresentationOverlays;
use super::{Resolved, invalid};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use url::Url;

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
    pub overlays: Option<PresentationOverlays>,
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

/// `open` accepts either a plain URL string (the legacy form) or a mapping
/// with a `url` and an optional `wait_until` settle condition.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawOpen {
    Url(String),
    Detailed(Box<RawOpenOptions>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOpenOptions {
    pub url: Option<String>,
    pub wait_until: Option<RawSettle>,
}

/// A structured settle condition. Exactly one of `visible` or `stable` is
/// expected; `compile_operation` validates this.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSettle {
    pub visible: Option<RawLocator>,
    pub stable: Option<RawLocator>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStep {
    #[serde(skip)]
    pub(crate) source_index: Option<usize>,
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
    pub pause: Option<String>,
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

impl RawStep {
    pub(crate) fn operation_count(&self) -> usize {
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
            self.pause.is_some(),
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
