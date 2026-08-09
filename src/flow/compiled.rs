use super::interpolate::push_interpolated;
use super::raw::{
    ClearTarget, NativeDialogResponse, PageSwitch, RecordingControl, RelativePoint, TextMatch,
    VideoMode, ViewportPoint,
};
use super::redact::{REDACTED, Redactor};
use super::{FlowError, Resolved, invalid};
use crate::browser::Geolocation;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

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

/// Static warning when visual capture is enabled for a flow that exposes a
/// secret-derived or runtime-output-derived value to the page. Never contains a
/// secret value.
pub const RECORDING_SECRET_WARNING: &str = "video or screenshots may capture secret-derived or runtime-output-derived values; rendered page content may contain sensitive data";

impl CompiledFlow {
    /// Returns a recording/screenshot warning when the flow both (1) captures
    /// visual artifacts (video not `off`, a `screenshot` step, or a visual
    /// screenshot assertion) and (2) exposes a secret-tainted value to the page
    /// via `open` URL, `fill`, `select`, `dialog` text, `evaluate` args, or a
    /// secret `switch_page` name/URL. Request-only secret usage is excluded.
    /// `--video` overrides are baked into `settings.video` at compile time.
    pub fn recording_secret_warning(&self) -> Option<&'static str> {
        if !self.captures_visual_artifacts() {
            return None;
        }
        self.steps
            .iter()
            .any(|step| operation_exposes_secret_tainted_value(&step.operation))
            .then_some(RECORDING_SECRET_WARNING)
    }

    fn captures_visual_artifacts(&self) -> bool {
        self.settings.video.enabled()
            || self.steps.iter().any(|step| {
                matches!(
                    step.operation,
                    Operation::Screenshot { .. } | Operation::Assert(Assertion::Screenshot(_))
                )
            })
    }
}

fn operation_exposes_secret_tainted_value(operation: &Operation) -> bool {
    match operation {
        Operation::Open { url, .. } => url.is_secret(),
        Operation::Fill { value, .. } | Operation::Select { value, .. } => value.is_secret(),
        Operation::Dialog { text, .. } => text.as_ref().is_some_and(RuntimeValue::is_secret),
        Operation::Evaluate { args, .. } => args.iter().any(RuntimeValue::is_secret),
        Operation::SwitchPage(PageSwitch::Name(name)) => name.is_secret(),
        Operation::SwitchPage(PageSwitch::Url(url)) => url.is_secret(),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowSettings {
    pub timeout: Duration,
    pub viewport: Viewport,
    pub video: VideoMode,
    pub geolocation: Option<Geolocation>,
    pub overlays: PresentationOverlays,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresentationOverlays {
    #[serde(default)]
    pub step: bool,
    #[serde(default)]
    pub url: bool,
    #[serde(default)]
    pub pointer: bool,
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
        settle: Option<SettleCondition>,
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
    Pause {
        duration: Duration,
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

/// A post-navigation settling condition attached to an `open` step.
///
/// The navigation step waits for document loading to complete and then, if a
/// settle condition is present, waits for the condition to be satisfied before
/// the step succeeds. Both phases are bounded by the normal step timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleCondition {
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
    pub(crate) template: Resolved<String>,
    pub(crate) parts: Vec<RuntimeValuePart>,
    pub(crate) outputs: BTreeSet<String>,
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
pub(crate) enum RuntimeValuePart {
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

    pub(crate) fn output_names(&self) -> impl Iterator<Item = &String> {
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
