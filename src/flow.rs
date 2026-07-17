use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
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
/// Maximum size of a YAML scalar or interpolated value in bytes (64 KiB).
pub const MAX_SCALAR_BYTES: usize = 64 * 1024;
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

    fn new(value: T, secret: bool) -> Self {
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

    fn extend(&mut self, other: &Self) {
        self.secrets.extend(other.secrets.iter().cloned());
        self.secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        self.secrets.dedup();
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
    pub run: Option<String>,
    pub open: Option<String>,
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
    pub press: Option<RawPress>,
    pub screenshot: Option<RawScreenshot>,
    pub clear: Option<ClearTarget>,
    #[serde(rename = "assert")]
    pub assertion: Option<RawAssertion>,
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
    pub target: RawLocator,
    pub position: Option<RelativePoint>,
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
#[serde(rename_all = "lowercase")]
pub enum ClearTarget {
    Cookies,
    Storage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledFlow {
    pub source: PathBuf,
    pub name: String,
    pub base_url: Option<Resolved<Url>>,
    pub settings: FlowSettings,
    pub inputs: BTreeMap<String, Resolved<String>>,
    pub steps: Vec<CompiledStep>,
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
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Open {
        url: Resolved<Url>,
    },
    Click {
        target: Locator,
        position: Option<RelativePoint>,
    },
    DoubleClick {
        target: Locator,
        position: Option<RelativePoint>,
    },
    Fill {
        target: Locator,
        value: Resolved<String>,
    },
    Erase {
        target: Locator,
    },
    Select {
        target: Locator,
        value: Resolved<String>,
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
    Press {
        target: Locator,
        key: Key,
        modifiers: Vec<Modifier>,
    },
    Screenshot {
        name: String,
        crop: Option<Crop>,
    },
    Clear(ClearTarget),
    Assert(Assertion),
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
    };
    let mut raw = read_flow(path)?;
    if let Some(video) = video {
        raw.settings.video = Some(video);
    }
    let mut flow =
        compile_raw_expanded(raw, path.to_owned(), cli_vars, environment, 0, &mut state)?;
    if let Some(name) = cli_vars
        .keys()
        .find(|name| !state.declared_cli_vars.contains(*name))
    {
        return invalid(format!("CLI variable {name:?} is not declared under vars"));
    }
    validate_expanded_steps(&mut flow.steps)?;
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
    compile_raw_inner(raw, source_path.into(), cli_vars, environment, true, false)
}

fn compile_raw_inner(
    raw: RawFlow,
    source_path: PathBuf,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
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

    let inputs = resolve_inputs(
        &raw.vars,
        &raw.secrets,
        cli_vars,
        environment,
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
        let operation = compile_operation(
            step,
            index,
            &source_path,
            base_url.as_ref(),
            viewport,
            &inputs,
        )?;
        if let Operation::Screenshot { name, .. } = &operation
            && !screenshot_names.insert(name.to_ascii_lowercase())
        {
            return invalid(format!("duplicate screenshot name {name:?}"));
        }
        steps.push(CompiledStep {
            index,
            source: source_path.clone(),
            source_index: index,
            id,
            timeout: step_timeout,
            operation,
        });
    }

    Ok(CompiledFlow {
        source: source_path,
        name,
        base_url,
        settings,
        inputs,
        steps,
        redactor,
    })
}

struct ExpansionState {
    active: Vec<(PathBuf, PathBuf)>,
    declared_cli_vars: BTreeSet<String>,
}

fn compile_raw_expanded(
    mut raw: RawFlow,
    source_path: PathBuf,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    depth: usize,
    state: &mut ExpansionState,
) -> Result<CompiledFlow, FlowError> {
    if depth > 0 && (raw.settings.viewport.is_some() || raw.settings.video.is_some()) {
        return invalid(format!(
            "{}: subflows cannot set settings.viewport or settings.video",
            source_path.display()
        ));
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
    let mut includes = BTreeMap::new();
    for (offset, step) in raw.steps.iter_mut().enumerate() {
        step.source_index = Some(offset + 1);
        if let Some(run) = &step.run {
            let operation_count = [
                step.open.is_some(),
                step.click.is_some(),
                step.double_click.is_some(),
                step.fill.is_some(),
                step.erase.is_some(),
                step.select.is_some(),
                step.scroll.is_some(),
                step.scroll_until_visible.is_some(),
                step.swipe.is_some(),
                step.long_press.is_some(),
                step.wait_until_visible.is_some(),
                step.wait_until_stable.is_some(),
                step.back.is_some(),
                step.press.is_some(),
                step.screenshot.is_some(),
                step.clear.is_some(),
                step.assertion.is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count();
            if step.id.is_some() || step.timeout.is_some() || operation_count != 0 {
                return invalid(format!(
                    "{}: step {} run must be the only field",
                    source_path.display(),
                    offset + 1
                ));
            }
            includes.insert(offset, run.clone());
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
        false,
        true,
    )
    .map_err(|error| with_path(&source_path, error))?;

    let mut compiled = flow.steps.into_iter();
    let original_len = compiled.len() + includes.len();
    let mut steps = Vec::new();
    for offset in 0..original_len {
        let local_index = offset + 1;
        if let Some(run) = includes.get(&offset) {
            if depth == MAX_SUBFLOW_DEPTH {
                return invalid(format!(
                    "{}: step {local_index} exceeds maximum subflow depth {MAX_SUBFLOW_DEPTH}",
                    source_path.display()
                ));
            }
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
            let child = compile_raw_expanded(
                child_raw,
                child_path,
                cli_vars,
                environment,
                depth + 1,
                state,
            );
            state.active.pop();
            let child = child?;
            flow.redactor.extend(&child.redactor);
            steps.extend(child.steps);
        } else if let Some(mut step) = compiled.next() {
            step.source_index = local_index;
            steps.push(step);
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
    let mut ids = BTreeSet::new();
    let mut screenshots = BTreeSet::new();
    for (offset, step) in steps.iter_mut().enumerate() {
        step.index = offset + 1;
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
    }
    if steps.is_empty() {
        return invalid("expanded steps must not be empty");
    }
    Ok(())
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

fn resolve_inputs(
    vars: &BTreeMap<String, RawVariable>,
    secrets: &BTreeMap<String, RawSecret>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
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

    let mut resolved = BTreeMap::new();
    for (name, raw) in vars {
        let value = if let Some(value) = cli_vars.get(name) {
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
    let operation_count = [
        step.open.is_some(),
        step.click.is_some(),
        step.double_click.is_some(),
        step.fill.is_some(),
        step.erase.is_some(),
        step.select.is_some(),
        step.scroll.is_some(),
        step.scroll_until_visible.is_some(),
        step.swipe.is_some(),
        step.long_press.is_some(),
        step.wait_until_visible.is_some(),
        step.wait_until_stable.is_some(),
        step.back.is_some(),
        step.press.is_some(),
        step.screenshot.is_some(),
        step.clear.is_some(),
        step.assertion.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if operation_count != 1 {
        return invalid(format!("step {index} must contain exactly one operation"));
    }

    if let Some(raw) = step.open {
        let value = interpolate(&format!("step {index} open"), &raw, inputs)?;
        require_non_empty(&format!("step {index} open"), value.expose())?;
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
        return Ok(Operation::Open {
            url: Resolved::new(url, value.secret || base_secret),
        });
    }
    if let Some(raw) = step.click {
        return Ok(Operation::Click {
            target: compile_locator(raw.target, index, inputs)?,
            position: raw.position,
        });
    }
    if let Some(raw) = step.double_click {
        return Ok(Operation::DoubleClick {
            target: compile_locator(raw.target, index, inputs)?,
            position: raw.position,
        });
    }
    if let Some(raw) = step.fill {
        let value = interpolate(&format!("step {index} fill.value"), &raw.value, inputs)?;
        require_non_empty(&format!("step {index} fill.value"), value.expose())?;
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
        let value = interpolate(&format!("step {index} select.value"), &raw.value, inputs)?;
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
    if let Some(target) = step.clear {
        return Ok(Operation::Clear(target));
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

fn parse_duration(context: &str, value: &str) -> Result<Duration, FlowError> {
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
mod tests {
    use super::*;

    fn compile(source: &str) -> Result<CompiledFlow, FlowError> {
        compile_yaml_with_env(
            source,
            "flows/example.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
    }

    fn error(source: &str) -> String {
        compile(source).unwrap_err().to_string()
    }

    #[test]
    fn compiles_all_canonical_operations_and_locators() {
        let source = r#"
version: 1
name: canonical
base_url: https://example.test/app/
settings:
  timeout: 2s
  viewport: { width: 800, height: 600 }
  video: retain-on-failure
steps:
  - id: open-home
    open: ../home
  - click:
      target: { css: "button.primary" }
  - fill:
      target: { test_id: email }
      value: user@example.test
  - press:
      target: { label: Search }
      key: Enter
      modifiers: [Control, Shift]
  - screenshot:
      name: search-results
      crop: { x: 10, y: 20, width: 400, height: 300 }
  - clear: cookies
  - clear: storage
  - click:
      target:
        text: { value: Welcome, match: contains }
  - click:
      target:
        role: { value: button, name: Sign in }
  - assert:
      visible: { text: Welcome }
  - assert:
      hidden: { css: .spinner }
  - assert:
      text:
        target: { test_id: status }
        equals: Saved
  - assert:
      text:
        target: { label: Status }
        contains: complete
  - assert:
      url: { equals: "https://example.test/dashboard" }
  - assert:
      url: { path: "/dashboard?q=a b" }
"#;

        let flow = compile(source).unwrap();
        assert_eq!(flow.settings.timeout, Duration::from_secs(2));
        assert_eq!(flow.settings.video, VideoMode::RetainOnFailure);
        assert_eq!(flow.steps.len(), 15);
        assert!(matches!(
            &flow.steps[0].operation,
            Operation::Open { url } if url.expose().as_str() == "https://example.test/home"
        ));
        assert!(matches!(
            &flow.steps[3].operation,
            Operation::Press { key: Key::Named(NamedKey::Enter), modifiers, .. }
                if modifiers == &[Modifier::Control, Modifier::Shift]
        ));
        assert!(matches!(
            &flow.steps[4].operation,
            Operation::Screenshot {
                name,
                crop: Some(Crop { x: 10, y: 20, width: 400, height: 300 })
            } if name == "search-results"
        ));
        assert!(matches!(
            &flow.steps[7].operation,
            Operation::Click {
                target: Locator {
                    strategy: LocatorStrategy::Text {
                        match_kind: TextMatch::Contains,
                        ..
                    },
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            &flow.steps[14].operation,
            Operation::Assert(Assertion::Url(UrlExpectation::Path(path)))
                if path.expose() == "/dashboard?q=a%20b"
        ));
    }

    #[test]
    fn video_defaults_on_and_can_be_disabled() {
        let enabled = compile("version: 1\nname: x\nsteps: [{ open: https://x.test }]\n").unwrap();
        assert_eq!(enabled.settings.video, VideoMode::On);

        let disabled = compile(
            "version: 1\nname: x\nsettings: { video: off }\nsteps: [{ open: https://x.test }]\n",
        )
        .unwrap();
        assert_eq!(disabled.settings.video, VideoMode::Off);
    }

    #[test]
    fn compiles_and_validates_geolocation_settings() {
        let default_accuracy = compile(
            "version: 1\nname: x\nsettings: { geolocation: { latitude: 51.5, longitude: -0.12 } }\nsteps: [{ open: https://x.test }]\n",
        )
        .unwrap();
        assert_eq!(
            default_accuracy.settings.geolocation,
            Some(Geolocation {
                latitude: 51.5,
                longitude: -0.12,
                accuracy: 0.0,
            })
        );
        assert!(
            compile("version: 1\nname: x\nsteps: [{ open: https://x.test }]\n")
                .unwrap()
                .settings
                .geolocation
                .is_none()
        );

        for (geolocation, expected) in [
            ("{ latitude: .nan, longitude: 0 }", "latitude"),
            ("{ latitude: 91, longitude: 0 }", "latitude"),
            ("{ latitude: 0, longitude: -.inf }", "longitude"),
            ("{ latitude: 0, longitude: 181 }", "longitude"),
            ("{ latitude: 0, longitude: 0, accuracy: .inf }", "accuracy"),
            ("{ latitude: 0, longitude: 0, accuracy: -1 }", "accuracy"),
        ] {
            let source = format!(
                "version: 1\nname: x\nsettings: {{ geolocation: {geolocation} }}\nsteps: [{{ open: https://x.test }}]\n"
            );
            assert!(error(&source).contains(expected), "accepted {geolocation}");
        }
    }

    #[test]
    fn validates_screenshot_names_crops_duplicates_and_secrets() {
        let valid = compile(
            "version: 1\nname: x\nsettings: { viewport: { width: 800, height: 600 } }\nsteps:\n  - screenshot: { name: full }\n  - screenshot: { name: corner_2, crop: { x: 700, y: 500, width: 100, height: 100 } }\n",
        )
        .unwrap();
        assert!(matches!(
            &valid.steps[0].operation,
            Operation::Screenshot { name, crop: None } if name == "full"
        ));

        for (source, expected) in [
            (
                "version: 1\nname: x\nsteps: [{ screenshot: { name: '../escape' } }]\n",
                "screenshot.name must be",
            ),
            (
                "version: 1\nname: x\nsteps: [{ screenshot: { name: x, crop: { x: 0, y: 0, width: 0, height: 1 } } }]\n",
                "greater than zero",
            ),
            (
                "version: 1\nname: x\nsettings: { viewport: { width: 10, height: 10 } }\nsteps: [{ screenshot: { name: x, crop: { x: 9, y: 0, width: 2, height: 1 } } }]\n",
                "fit within the 10x10 viewport",
            ),
            (
                "version: 1\nname: x\nsteps: [{ screenshot: { name: same } }, { screenshot: { name: same } }]\n",
                "duplicate screenshot name",
            ),
            (
                "version: 1\nname: x\nsteps: [{ screenshot: { name: Same } }, { screenshot: { name: same } }]\n",
                "duplicate screenshot name",
            ),
            (
                "version: 1\nname: x\nsteps: [{ screenshot: { name: Failure } }]\n",
                "cannot be 'failure'",
            ),
            (
                "version: 1\nname: x\nsteps: [{ screenshot: { name: NUL } }]\n",
                "screenshot.name must be",
            ),
        ] {
            assert!(error(source).contains(expected), "missing {expected:?}");
        }

        let source = "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ screenshot: { name: '${token}' } }]\n";
        let environment = BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]);
        let message = compile_yaml_with_env(source, "x.yaml", &BTreeMap::new(), &environment)
            .unwrap_err()
            .to_string();
        assert!(message.contains("screenshot.name cannot contain a secret"));
        assert!(!message.contains("canary-secret"));
    }

    #[test]
    fn compiles_bounded_visual_assertions_relative_to_the_containing_flow() {
        let valid = compile(
            "version: 1\nname: x\nsettings: { viewport: { width: 800, height: 600 }, video: off }\nsteps:\n  - assert:\n      screenshot:\n        baseline: fixtures/home.png\n        crop: { x: 10, y: 20, width: 400, height: 300 }\n        channel_tolerance: 4\n        max_changed_ratio: 0.01\n",
        )
        .unwrap();
        assert!(matches!(
            &valid.steps[0].operation,
            Operation::Assert(Assertion::Screenshot(VisualExpectation {
                baseline,
                crop: Some(Crop { x: 10, y: 20, width: 400, height: 300 }),
                channel_tolerance: 4,
                max_changed_ratio,
            })) if baseline == Path::new("flows/fixtures/home.png") && *max_changed_ratio == 0.01
        ));

        for (baseline, ratio, expected) in [
            ("../home.png", "0", "relative .png path"),
            ("/home.png", "0", "relative .png path"),
            ("home.jpg", "0", "relative .png path"),
            ("home.png", "1.01", "between 0 and 1"),
            ("home.png", "-.inf", "between 0 and 1"),
        ] {
            let source = format!(
                "version: 1\nname: x\nsettings: {{ video: off }}\nsteps: [{{ assert: {{ screenshot: {{ baseline: '{baseline}', max_changed_ratio: {ratio} }} }} }}]\n"
            );
            assert!(
                error(&source).contains(expected),
                "accepted {baseline} {ratio}"
            );
        }
        assert!(error(
            "version: 1\nname: x\nsettings: { viewport: { width: 8192, height: 8192 }, video: off }\nsteps: [{ assert: { screenshot: { baseline: home.png } } }]\n"
        )
        .contains("visual image dimensions"));
    }

    #[test]
    fn compiles_double_click_as_one_target_action() {
        let flow = compile(
            "version: 1\nname: x\nsteps:\n  - double_click: { target: { test_id: item } }\n",
        )
        .unwrap();
        assert!(matches!(
            &flow.steps[0].operation,
            Operation::DoubleClick {
                target: Locator {
                    strategy: LocatorStrategy::TestId(value),
                    ..
                },
                ..
            } if value.expose() == "item"
        ));
        assert!(error(
            "version: 1\nname: x\nsteps:\n  - click: { target: { css: x } }\n    double_click: { target: { css: x } }\n"
        )
        .contains("exactly one operation"));
    }

    #[test]
    fn compiles_erase_select_scroll_and_back_as_strict_single_operations() {
        let flow = compile(
            "version: 1\nname: interactions\nsteps:\n  - erase: { target: { label: Search } }\n  - select: { target: { css: select }, value: '' }\n  - scroll: { y: 500 }\n  - back: {}\n",
        )
        .unwrap();
        assert!(matches!(flow.steps[0].operation, Operation::Erase { .. }));
        assert!(matches!(
            &flow.steps[1].operation,
            Operation::Select { value, .. } if value.expose().is_empty()
        ));
        assert!(matches!(
            flow.steps[2].operation,
            Operation::Scroll { x: 0, y: 500 }
        ));
        assert!(matches!(flow.steps[3].operation, Operation::Back));

        for (source, expected) in [
            (
                "version: 1\nname: x\nsteps: [{ scroll: {} }]\n",
                "non-zero x or y",
            ),
            (
                "version: 1\nname: x\nsteps: [{ back: { target: x } }]\n",
                "unknown field",
            ),
            (
                "version: 1\nname: x\nsteps: [{ erase: { target: { css: x } }, back: {} }]\n",
                "exactly one operation",
            ),
        ] {
            assert!(error(source).contains(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn compiles_advanced_interactions_and_waits_with_bounded_defaults() {
        let flow = compile(
            "version: 1\nname: advanced\nsteps:\n  - scroll_until_visible: { target: { text: Last }, y: 400 }\n  - swipe: { target: { css: .card }, x: -120 }\n  - long_press: { target: { test_id: menu }, duration: 750ms }\n  - timeout: 30s\n    wait_until_visible: { target: { css: .late } }\n  - wait_until_stable: { target: { css: .animated } }\n",
        )
        .unwrap();

        assert!(matches!(
            flow.steps[0].operation,
            Operation::ScrollUntilVisible { x: 0, y: 400, .. }
        ));
        assert!(matches!(
            flow.steps[1].operation,
            Operation::Swipe {
                x: -120,
                y: 0,
                duration: DEFAULT_SWIPE_DURATION,
                ..
            }
        ));
        assert!(matches!(
            flow.steps[2].operation,
            Operation::LongPress { duration, .. } if duration == Duration::from_millis(750)
        ));
        assert_eq!(flow.steps[3].timeout, Duration::from_secs(30));
        assert!(matches!(
            flow.steps[3].operation,
            Operation::WaitUntilVisible { .. }
        ));
        assert!(matches!(
            flow.steps[4].operation,
            Operation::WaitUntilStable { .. }
        ));
    }

    #[test]
    fn rejects_unbounded_or_empty_advanced_gestures() {
        for (step, expected) in [
            (
                "scroll_until_visible: { target: { css: x } }",
                "non-zero x or y",
            ),
            (
                "swipe: { target: { css: x }, x: 10001 }",
                "between -10000 and 10000",
            ),
            (
                "long_press: { target: { css: x }, duration: 11s }",
                "must not exceed 10 seconds",
            ),
            (
                "swipe: { target: { css: x }, y: 1, duration: 0ms }",
                "outside the supported range",
            ),
        ] {
            let source = format!("version: 1\nname: x\nsteps:\n  - {step}\n");
            assert!(error(&source).contains(expected), "accepted {step}");
        }
    }

    #[test]
    fn clear_accepts_only_cookies_or_storage_as_a_scalar() {
        let flow =
            compile("version: 1\nname: clear\nsteps:\n  - clear: cookies\n  - clear: storage\n")
                .unwrap();
        assert!(matches!(
            flow.steps[0].operation,
            Operation::Clear(ClearTarget::Cookies)
        ));
        assert!(matches!(
            flow.steps[1].operation,
            Operation::Clear(ClearTarget::Storage)
        ));

        for value in ["cache", "Cookies", "{ cookies: true }"] {
            assert!(
                parse_yaml(&format!(
                    "version: 1\nname: clear\nsteps: [{{ clear: {value} }}]\n"
                ))
                .is_err(),
                "accepted clear value {value:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_duplicate_merge_and_alias_yaml() {
        let unknown = "version: 1\nname: x\nunknown: true\nsteps: [{ open: https://x.test }]\n";
        assert!(parse_yaml(unknown).is_err());

        let duplicate =
            "version: 1\nname: first\nname: second\nsteps: [{ open: https://x.test }]\n";
        assert!(parse_yaml(duplicate).is_err());

        let merge = r#"
defaults: &defaults
  name: merged
version: 1
<<: *defaults
steps: [{ open: https://x.test }]
"#;
        assert!(parse_yaml(merge).is_err());

        let alias = r#"
version: 1
name: alias
vars:
  first: &value hello
  second: *value
steps: [{ open: https://x.test }]
"#;
        assert!(parse_yaml(alias).is_err());
    }

    #[test]
    fn rejects_oversized_yaml_sources_before_parsing_or_file_decoding() {
        let source = "x".repeat(MAX_FLOW_SOURCE_BYTES + 1);
        assert!(error(&source).contains("flow source exceeds the maximum size"));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.yaml");
        fs::write(&path, source).unwrap();
        let message = compile_file(path, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(message.contains("flow source exceeds the maximum size"));
    }

    #[test]
    fn rejects_excess_steps_scalars_and_interpolation_growth() {
        let steps = "  - open: https://x.test\n".repeat(MAX_FLOW_STEPS + 1);
        let source = format!("version: 1\nname: x\nsteps:\n{steps}");
        assert!(error(&source).contains("steps must not exceed 10000"));

        let large = "x".repeat(MAX_SCALAR_BYTES + 1);
        let source = format!("version: 1\nname: {large}\nsteps: [{{ open: https://x.test }}]\n");
        assert!(error(&source).contains("maximum scalar size"));

        let source = "version: 1\nname: x\nvars: { chunk: { env: CHUNK } }\nsteps:\n  - fill: { target: { css: x }, value: '${chunk}${chunk}' }\n";
        let environment = BTreeMap::from([("CHUNK".to_owned(), "x".repeat(MAX_SCALAR_BYTES))]);
        let message = compile_yaml_with_env(source, "x.yaml", &BTreeMap::new(), &environment)
            .unwrap_err()
            .to_string();
        assert!(message.contains("maximum scalar size"));
    }

    #[test]
    fn accepts_timeout_ceiling_and_rejects_larger_flow_and_step_timeouts() {
        let flow = compile(
            "version: 1\nname: x\nsettings: { timeout: 60m }\nsteps: [{ timeout: 3600s, open: https://x.test }]\n",
        )
        .unwrap();
        assert_eq!(flow.settings.timeout, MAX_TIMEOUT);
        assert_eq!(flow.steps[0].timeout, MAX_TIMEOUT);

        assert!(error(
            "version: 1\nname: x\nsettings: { timeout: 61m }\nsteps: [{ open: https://x.test }]\n"
        )
        .contains("must not exceed 3600 seconds"));
        assert!(
            error("version: 1\nname: x\nsteps: [{ timeout: 3601s, open: https://x.test }]\n")
                .contains("must not exceed 3600 seconds")
        );
    }

    #[test]
    fn enforces_single_operations_assertions_and_locator_strategies() {
        assert!(error(
            "version: 1\nname: x\nsteps:\n  - open: https://x.test\n    click: { target: { css: x } }\n"
        )
        .contains("exactly one operation"));
        assert!(error(
            "version: 1\nname: x\nsteps:\n  - assert:\n      visible: { css: x }\n      hidden: { css: y }\n"
        )
        .contains("exactly one assertion"));
        assert!(
            error("version: 1\nname: x\nsteps:\n  - click:\n      target: { css: x, text: y }\n")
                .contains("exactly one strategy")
        );
    }

    #[test]
    fn compiles_flat_locator_modifiers_without_counting_them_as_strategies() {
        let flow = compile(
            "version: 1\nname: x\nsteps:\n  - click:\n      position: { x: 4, y: 7 }\n      target: { css: option, index: 0, checked: false, selected: true, focused: false, enabled: true }\n",
        )
        .unwrap();
        let Operation::Click { target, position } = &flow.steps[0].operation else {
            panic!("expected click");
        };
        assert!(matches!(target.strategy, LocatorStrategy::Css(_)));
        assert_eq!(target.index, Some(0));
        assert_eq!(target.checked, Some(false));
        assert_eq!(target.selected, Some(true));
        assert_eq!(target.focused, Some(false));
        assert_eq!(target.enabled, Some(true));
        assert_eq!(*position, Some(RelativePoint { x: 4, y: 7 }));

        assert!(
            error("version: 1\nname: x\nsteps: [{ click: { target: { index: 0 } } }]\n")
                .contains("exactly one strategy")
        );
        assert!(
            parse_yaml(
                "version: 1\nname: x\nsteps: [{ click: { target: { css: x, index: -1 } } }]\n"
            )
            .is_err()
        );
        assert!(
            parse_yaml(
                "version: 1\nname: x\nsteps: [{ click: { target: { css: x, checked: yes } } }]\n"
            )
            .is_err()
        );
        assert!(parse_yaml(
            "version: 1\nname: x\nsteps: [{ erase: { target: { css: x }, position: { x: 1, y: 1 } } }]\n"
        )
        .is_err());
    }

    #[test]
    fn compiles_recursive_relations_and_bounds_their_depth() {
        let flow = compile(
            "version: 1\nname: x\nsteps:\n  - click:\n      target:\n        css: button\n        within: { css: .panel }\n        has: { text: Save }\n        above: { test_id: footer }\n        below: { css: header }\n        left: { label: Cancel }\n        right: { role: { value: img, name: Logo } }\n",
        )
        .unwrap();
        let Operation::Click { target, .. } = &flow.steps[0].operation else {
            panic!("expected click");
        };
        assert_eq!(
            target
                .relations
                .iter()
                .map(|relation| relation.kind)
                .collect::<Vec<_>>(),
            [
                RelationKind::Within,
                RelationKind::Has,
                RelationKind::Above,
                RelationKind::Below,
                RelationKind::Left,
                RelationKind::Right,
            ]
        );

        let mut locator = "{ css: leaf }".to_owned();
        for _ in 0..=MAX_LOCATOR_DEPTH {
            locator = format!("{{ css: node, has: {locator} }}");
        }
        let source =
            format!("version: 1\nname: x\nsteps: [{{ click: {{ target: {locator} }} }}]\n");
        assert!(error(&source).contains("maximum relation depth 8"));
    }

    #[test]
    fn validates_required_values_version_ids_duration_viewport_and_keys() {
        let invalid_cases = [
            (
                "version: 2\nname: x\nsteps: [{ open: https://x.test }]\n",
                "version must be 1",
            ),
            (
                "version: 1\nname: '  '\nsteps: [{ open: https://x.test }]\n",
                "name must not be empty",
            ),
            (
                "version: 1\nname: x\nsteps: []\n",
                "steps must not be empty",
            ),
            (
                "version: 1\nname: x\nsettings: { timeout: 0s }\nsteps: [{ open: https://x.test }]\n",
                "outside the supported range",
            ),
            (
                "version: 1\nname: x\nsettings: { timeout: 1.5s }\nsteps: [{ open: https://x.test }]\n",
                "not a valid duration",
            ),
            (
                "version: 1\nname: x\nsettings: { video: on, viewport: { width: 801, height: 600 } }\nsteps: [{ open: https://x.test }]\n",
                "even viewport",
            ),
            (
                "version: 1\nname: x\nsteps:\n  - id: same\n    open: https://x.test\n  - id: same\n    open: https://x.test\n",
                "duplicate step id",
            ),
            (
                "version: 1\nname: x\nsteps:\n  - press: { target: { css: x }, key: F1 }\n",
                "unsupported key",
            ),
            (
                "version: 1\nname: x\nsteps:\n  - press: { target: { css: x }, key: Enter, modifiers: [Alt, Alt] }\n",
                "duplicate modifier",
            ),
            (
                "version: 1\nname: x\nsteps:\n  - fill: { target: { css: x }, value: '' }\n",
                "fill.value must not be empty",
            ),
        ];
        for (source, expected) in invalid_cases {
            assert!(error(source).contains(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn resolves_cli_env_defaults_and_secret_taint_without_debug_leaks() {
        let source = r#"
version: 1
name: "login-${region}"
base_url: "https://${host}"
vars:
  region: local
  host: { env: TEST_HOST, default: default.test }
  username: { env: TEST_USER }
secrets:
  password: { env: TEST_PASSWORD }
steps:
  - fill:
      target: { label: Password }
      value: "prefix-${password}"
  - fill:
      target: { label: User }
      value: "${username}"
"#;
        let cli = BTreeMap::from([("region".to_owned(), "ci".to_owned())]);
        let env = BTreeMap::from([
            ("TEST_HOST".to_owned(), "example.test".to_owned()),
            ("TEST_USER".to_owned(), "alice".to_owned()),
            ("TEST_PASSWORD".to_owned(), "canary-secret".to_owned()),
        ]);
        let flow = compile_yaml_with_env(source, "login.yaml", &cli, &env).unwrap();

        assert_eq!(flow.name, "login-ci");
        assert_eq!(
            flow.base_url.as_ref().unwrap().expose().host_str(),
            Some("example.test")
        );
        let Operation::Fill { value, .. } = &flow.steps[0].operation else {
            panic!("expected fill");
        };
        assert!(value.is_secret());
        assert_eq!(value.expose(), "prefix-canary-secret");
        assert_eq!(format!("{value:?}"), REDACTED);
        assert!(!format!("{flow:?}").contains("canary-secret"));
        assert_eq!(
            flow.redactor.redact("failed with canary-secret visible"),
            "failed with [REDACTED] visible"
        );
    }

    #[test]
    fn rejects_unresolved_unknown_empty_and_secret_identity_inputs() {
        assert!(error(
            "version: 1\nname: x\nsteps: [{ fill: { target: { css: x }, value: '${missing}' } }]\n"
        )
        .contains("unknown variable"));
        assert!(
            error("version: 1\nname: x\nvars: { empty: '' }\nsteps: [{ open: https://x.test }]\n")
                .contains("must not be empty")
        );

        let source = "version: 1\nname: '${token}'\nsecrets: { token: { env: TOKEN } }\nsteps: [{ open: https://x.test }]\n";
        let env = BTreeMap::from([("TOKEN".to_owned(), "canary-value".to_owned())]);
        let message = compile_yaml_with_env(source, "x.yaml", &BTreeMap::new(), &env)
            .unwrap_err()
            .to_string();
        assert!(message.contains("name cannot contain a secret"));
        assert!(!message.contains("canary-value"));
    }

    #[test]
    fn validates_open_and_assertion_urls() {
        assert!(
            error("version: 1\nname: x\nsteps: [{ open: /relative }]\n")
                .contains("base_url is not set")
        );
        assert!(
            error("version: 1\nname: x\nbase_url: file:///tmp\nsteps: [{ open: /x }]\n")
                .contains("http or https")
        );
        assert!(
            error("version: 1\nname: x\nsteps: [{ assert: { url: { equals: /relative } } }]\n")
                .contains("absolute URL")
        );
        assert!(
            error("version: 1\nname: x\nsteps: [{ assert: { url: { path: '//host/path' } } }]\n")
                .contains("one slash")
        );
        assert!(
            error(
                "version: 1\nname: x\nsteps: [{ assert: { url: { path: '/path#fragment' } } }]\n"
            )
            .contains("fragment")
        );
    }

    #[test]
    fn cli_values_only_override_declared_non_secret_vars() {
        let source = "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ open: https://x.test }]\n";
        let cli = BTreeMap::from([("token".to_owned(), "not-allowed".to_owned())]);
        let env = BTreeMap::from([("TOKEN".to_owned(), "secret".to_owned())]);
        let message = compile_yaml_with_env(source, "x.yaml", &cli, &env)
            .unwrap_err()
            .to_string();
        assert!(message.contains("not declared under vars"));
        assert!(!message.contains("not-allowed"));
    }

    #[test]
    fn discovers_yaml_recursively_in_stable_order_and_builds_stable_artifact_keys() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(directory.path().join("z.yml"), "").unwrap();
        fs::write(directory.path().join("a.yaml"), "").unwrap();
        fs::write(directory.path().join("ignored.txt"), "").unwrap();
        fs::write(nested.join("b.yaml"), "").unwrap();
        fs::write(nested.join("shared.subflow.yaml"), "").unwrap();
        fs::write(nested.join("shared.subflow.yml"), "").unwrap();

        let files = discover_flow_files(directory.path()).unwrap();
        let relative = files
            .iter()
            .map(|file| normalized_path(file.strip_prefix(directory.path()).expect("under root")))
            .collect::<Vec<_>>();
        assert_eq!(relative, ["a.yaml", "nested/b.yaml", "z.yml"]);

        let first = artifact_key(directory.path(), nested.join("b.yaml"));
        let second = artifact_key(directory.path(), nested.join("b.yaml"));
        assert_eq!(first, second);
        assert!(first.starts_with("nested-b-"));
        assert_ne!(
            artifact_key(directory.path(), directory.path().join("b.yaml")),
            first
        );
    }

    #[test]
    fn rejects_non_yaml_file_and_empty_directory_discovery() {
        let directory = tempfile::tempdir().unwrap();
        assert!(discover_flow_files(directory.path()).is_err());
        let text = directory.path().join("flow.txt");
        fs::write(&text, "").unwrap();
        assert!(discover_flow_files(text).is_err());
    }

    #[test]
    fn expands_nested_subflows_in_place_with_file_scoped_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let root = directory.path().join("root.yaml");
        let child = nested.join("child.subflow.yaml");
        let grandchild = directory.path().join("grandchild.subflow.yml");
        fs::write(
            &root,
            "version: 1\nname: root-${root_name}\nbase_url: https://root.test\nsettings: { timeout: 9s, video: off }\nvars: { root_name: root }\nsteps:\n  - open: /before\n  - run: ./nested/child.subflow.yaml\n  - assert: { url: { path: /after } }\n",
        )
        .unwrap();
        fs::write(
            &child,
            "version: 1\nname: child\nbase_url: https://child.test/base/\nsettings: { timeout: 2s }\nvars: { child_value: default }\nsteps:\n  - open: page\n  - run: ../grandchild.subflow.yml\n",
        )
        .unwrap();
        fs::write(
            &grandchild,
            "version: 1\nname: grandchild\nvars: { leaf: default }\nsecrets: { token: { env: TOKEN } }\nsteps:\n  - fill: { target: { css: input }, value: '${leaf}-${token}' }\n",
        )
        .unwrap();
        let cli = BTreeMap::from([
            ("root_name".to_owned(), "entry".to_owned()),
            ("child_value".to_owned(), "unused".to_owned()),
            ("leaf".to_owned(), "value".to_owned()),
        ]);
        let environment = BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]);

        let flow = compile_file_with_env(&root, &cli, &environment).unwrap();

        assert_eq!(flow.name, "root-entry");
        assert_eq!(flow.settings.timeout, Duration::from_secs(9));
        assert_eq!(flow.settings.video, VideoMode::Off);
        assert_eq!(flow.steps.len(), 4);
        assert_eq!(
            flow.steps.iter().map(|step| step.index).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(flow.steps[1].source, child);
        assert_eq!(flow.steps[1].source_index, 1);
        assert_eq!(
            fs::canonicalize(&flow.steps[2].source).unwrap(),
            fs::canonicalize(&grandchild).unwrap()
        );
        assert_eq!(flow.steps[2].source_index, 1);
        assert_eq!(flow.steps[1].timeout, Duration::from_secs(2));
        assert_eq!(flow.steps[2].timeout, DEFAULT_TIMEOUT);
        assert!(matches!(
            &flow.steps[1].operation,
            Operation::Open { url } if url.expose().as_str() == "https://child.test/base/page"
        ));
        assert!(matches!(
            &flow.steps[2].operation,
            Operation::Fill { value, .. }
                if value.expose() == "value-canary-secret" && value.is_secret()
        ));
        assert_eq!(flow.redactor.redact("canary-secret"), REDACTED);
        assert_eq!(flow.inputs["root_name"].expose(), "entry");
        assert!(!flow.inputs.contains_key("leaf"));
    }

    #[test]
    fn subflows_are_reusable_but_canonical_active_stack_cycles_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.yaml");
        let child = directory.path().join("shared.subflow.yaml");
        fs::write(
            &root,
            "version: 1\nname: root\nsettings: { video: off }\nsteps:\n  - run: ./shared.subflow.yaml\n  - run: ./shared.subflow.yaml\n",
        )
        .unwrap();
        fs::write(
            &child,
            "version: 1\nname: shared\nsteps: [{ open: https://example.test }]\n",
        )
        .unwrap();
        assert_eq!(
            compile_file(&root, &BTreeMap::new()).unwrap().steps.len(),
            2
        );

        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(
            &child,
            "version: 1\nname: cycle\nsteps: [{ run: './nested/../shared.subflow.yaml' }]\n",
        )
        .unwrap();
        let message = compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(message.contains("subflow include cycle"), "{message}");
        assert!(message.contains("shared.subflow.yaml"), "{message}");
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_subflow_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.yaml");
        let child = directory.path().join("child.subflow.yaml");
        let compile_case = |root_source: &str, child_source: &str| {
            fs::write(&root, root_source).unwrap();
            fs::write(&child, child_source).unwrap();
            compile_file(&root, &BTreeMap::new())
                .unwrap_err()
                .to_string()
        };
        let root_source = "version: 1\nname: root\nsteps: [{ run: ./child.subflow.yaml }]\n";
        for child_source in [
            "version: 1\nname: child\nsteps: []\n",
            "version: 1\nname: child\nsettings: { viewport: { width: 800, height: 600 } }\nsteps: [{ open: https://example.test }]\n",
            "version: 1\nname: child\nsettings: { video: off }\nsteps: [{ open: https://example.test }]\n",
        ] {
            let message = compile_case(root_source, child_source);
            assert!(
                message.contains("steps must not be empty")
                    || message.contains("subflows cannot set"),
                "invalid child was accepted: {message}"
            );
        }

        fs::write(
            &child,
            "version: 1\nname: child\nsteps: [{ open: https://example.test }]\n",
        )
        .unwrap();
        for (step, expected) in [
            ("{ id: x, run: ./child.subflow.yaml }", "only field"),
            ("{ timeout: 1s, run: ./child.subflow.yaml }", "only field"),
            (
                "{ run: ./child.subflow.yaml, open: https://x.test }",
                "only field",
            ),
            ("{ run: ./child.yaml }", ".subflow.yaml"),
            ("{ run: /tmp/child.subflow.yaml }", "must be relative"),
        ] {
            fs::write(&root, format!("version: 1\nname: root\nsteps: [{step}]\n")).unwrap();
            let message = compile_file(&root, &BTreeMap::new())
                .unwrap_err()
                .to_string();
            assert!(message.contains(expected), "{message}");
        }
        assert!(
            compile("version: 1\nname: memory\nsteps: [{ run: ./child.subflow.yaml }]\n")
                .unwrap_err()
                .to_string()
                .contains("require compiling a flow file")
        );
    }

    #[test]
    fn validates_expanded_uniqueness_and_reports_child_compile_locations() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.yaml");
        let child = directory.path().join("child.subflow.yaml");
        fs::write(
            &root,
            "version: 1\nname: root\nsteps:\n  - id: same\n    screenshot: { name: same }\n  - run: ./child.subflow.yaml\n",
        )
        .unwrap();
        fs::write(
            &child,
            "version: 1\nname: child\nsteps:\n  - id: same\n    open: https://example.test\n",
        )
        .unwrap();
        assert!(
            compile_file(&root, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("duplicate step id")
        );

        fs::write(
            &child,
            "version: 1\nname: child\nsteps:\n  - open: https://example.test\n  - click: { target: { css: x, text: y } }\n",
        )
        .unwrap();
        let message = compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(message.contains("child.subflow.yaml"), "{message}");
        assert!(message.contains("step 2 locator"), "{message}");

        fs::write(
            &child,
            "version: 1\nname: child\nsteps: [{ screenshot: { name: Same } }]\n",
        )
        .unwrap();
        assert!(
            compile_file(&root, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("duplicate screenshot name")
        );
    }

    #[test]
    fn enforces_expanded_step_and_subflow_depth_limits() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.yaml");
        let child = directory.path().join("many.subflow.yaml");
        let root_steps = "  - open: https://root.test\n".repeat(MAX_FLOW_STEPS / 2);
        let child_steps = "  - open: https://child.test\n".repeat(MAX_FLOW_STEPS / 2 + 1);
        fs::write(
            &root,
            format!(
                "version: 1\nname: root\nsettings: {{ video: off }}\nsteps:\n{root_steps}  - run: ./many.subflow.yaml\n"
            ),
        )
        .unwrap();
        fs::write(
            &child,
            format!("version: 1\nname: child\nsteps:\n{child_steps}"),
        )
        .unwrap();
        assert!(
            compile_file(&root, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("expanded steps must not exceed")
        );

        for depth in 0..=MAX_SUBFLOW_DEPTH {
            let path = if depth == 0 {
                root.clone()
            } else {
                directory.path().join(format!("{depth}.subflow.yaml"))
            };
            let next = depth + 1;
            fs::write(
                path,
                format!(
                    "version: 1\nname: depth-{depth}\nsteps: [{{ run: ./{next}.subflow.yaml }}]\n"
                ),
            )
            .unwrap();
        }
        fs::write(
            directory
                .path()
                .join(format!("{}.subflow.yaml", MAX_SUBFLOW_DEPTH + 1)),
            "version: 1\nname: leaf\nsteps: [{ open: https://example.test }]\n",
        )
        .unwrap();
        let message = compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(message.contains("maximum subflow depth 32"), "{message}");
    }
}
