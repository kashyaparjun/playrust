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

pub const REDACTED: &str = "[REDACTED]";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum accepted YAML flow source size in bytes (1 MiB).
pub const MAX_FLOW_SOURCE_BYTES: usize = 1024 * 1024;
/// Maximum number of steps in one flow.
pub const MAX_FLOW_STEPS: usize = 10_000;
/// Maximum size of a YAML scalar or interpolated value in bytes (64 KiB).
pub const MAX_SCALAR_BYTES: usize = 64 * 1024;
/// Maximum timeout accepted for flow settings or an individual step.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);

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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawViewport {
    pub width: u32,
    pub height: u32,
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
    pub id: Option<String>,
    pub timeout: Option<String>,
    pub open: Option<String>,
    pub click: Option<RawTargetAction>,
    pub double_click: Option<RawTargetAction>,
    pub fill: Option<RawFill>,
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
pub struct RawFill {
    pub target: RawLocator,
    pub value: String,
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
#[serde(rename_all = "lowercase")]
pub enum ClearTarget {
    Cookies,
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledFlow {
    pub source: PathBuf,
    pub name: String,
    pub base_url: Option<Resolved<Url>>,
    pub settings: FlowSettings,
    pub inputs: BTreeMap<String, Resolved<String>>,
    pub steps: Vec<CompiledStep>,
    pub redactor: Redactor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowSettings {
    pub timeout: Duration,
    pub viewport: Viewport,
    pub video: VideoMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledStep {
    pub index: usize,
    pub id: Option<String>,
    pub timeout: Duration,
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Open {
        url: Resolved<Url>,
    },
    Click {
        target: Locator,
    },
    DoubleClick {
        target: Locator,
    },
    Fill {
        target: Locator,
        value: Resolved<String>,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assertion {
    Visible(Locator),
    Hidden(Locator),
    Text {
        target: Locator,
        expected: Resolved<String>,
        match_kind: TextMatch,
    },
    Url(UrlExpectation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlExpectation {
    Equals(Resolved<Url>),
    Path(Resolved<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locator {
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
    let path = path.as_ref();
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
    compile_yaml(&source, path, cli_vars)
}

pub fn compile_raw(
    raw: RawFlow,
    source_path: impl Into<PathBuf>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
) -> Result<CompiledFlow, FlowError> {
    if raw.version != 1 {
        return invalid("version must be 1");
    }
    if raw.steps.is_empty() {
        return invalid("steps must not be empty");
    }
    if raw.steps.len() > MAX_FLOW_STEPS {
        return invalid(format!("steps must not exceed {MAX_FLOW_STEPS}"));
    }

    let inputs = resolve_inputs(&raw.vars, &raw.secrets, cli_vars, environment)?;
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
    let video = raw.settings.video.unwrap_or(VideoMode::Off);
    if video != VideoMode::Off
        && (!viewport.width.is_multiple_of(2) || !viewport.height.is_multiple_of(2))
    {
        return invalid("video requires even viewport width and height");
    }

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
    };
    let mut ids = BTreeSet::new();
    let mut screenshot_names = BTreeSet::new();
    let mut steps = Vec::with_capacity(raw.steps.len());
    for (offset, step) in raw.steps.into_iter().enumerate() {
        let index = offset + 1;
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
        let operation = compile_operation(step, index, base_url.as_ref(), viewport, &inputs)?;
        if let Operation::Screenshot { name, .. } = &operation
            && !screenshot_names.insert(name.to_ascii_lowercase())
        {
            return invalid(format!("duplicate screenshot name {name:?}"));
        }
        steps.push(CompiledStep {
            index,
            id,
            timeout: step_timeout,
            operation,
        });
    }

    Ok(CompiledFlow {
        source: source_path.into(),
        name,
        base_url,
        settings,
        inputs,
        steps,
        redactor,
    })
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
    if let Some(name) = cli_vars.keys().find(|name| !vars.contains_key(*name)) {
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
    base_url: Option<&Resolved<Url>>,
    viewport: Viewport,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Operation, FlowError> {
    let operation_count = [
        step.open.is_some(),
        step.click.is_some(),
        step.double_click.is_some(),
        step.fill.is_some(),
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
        });
    }
    if let Some(raw) = step.double_click {
        return Ok(Operation::DoubleClick {
            target: compile_locator(raw.target, index, inputs)?,
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
        inputs,
    )?))
}

fn validate_screenshot_name(index: usize, name: &str) -> Result<(), FlowError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.eq_ignore_ascii_case("failure")
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
            "step {index} screenshot.name must be 1-64 ASCII letters, numbers, '-' or '_', starting and ending with a letter or number, and cannot be 'failure'"
        ));
    }
    Ok(())
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
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Assertion, FlowError> {
    let assertion_count = [
        raw.visible.is_some(),
        raw.hidden.is_some(),
        raw.text.is_some(),
        raw.url.is_some(),
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

fn compile_locator(
    raw: RawLocator,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
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
    let locator = if let Some(value) = raw.css {
        Locator::Css(interpolate_non_empty(&context, &value, inputs)?)
    } else if let Some(value) = raw.test_id {
        Locator::TestId(interpolate_non_empty(&context, &value, inputs)?)
    } else if let Some(text) = raw.text {
        let (value, match_kind) = match text {
            RawTextLocator::Scalar(value) => (value, TextMatch::Exact),
            RawTextLocator::Detailed(options) => (
                options.value,
                options.match_kind.unwrap_or(TextMatch::Exact),
            ),
        };
        Locator::Text {
            value: interpolate_non_empty(&context, &value, inputs)?,
            match_kind,
        }
    } else if let Some(value) = raw.label {
        Locator::Label(interpolate_non_empty(&context, &value, inputs)?)
    } else {
        let role = raw.role.expect("strategy count checked");
        Locator::Role {
            value: interpolate_non_empty(&context, &role.value, inputs)?,
            name: role
                .name
                .as_deref()
                .map(|value| interpolate_non_empty(&context, value, inputs))
                .transpose()?,
        }
    };
    Ok(locator)
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
        } else if file_type.is_file() && is_yaml(&entry.path()) {
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
                target: Locator::Text {
                    match_kind: TextMatch::Contains,
                    ..
                }
            }
        ));
        assert!(matches!(
            &flow.steps[14].operation,
            Operation::Assert(Assertion::Url(UrlExpectation::Path(path)))
                if path.expose() == "/dashboard?q=a%20b"
        ));
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
    fn compiles_double_click_as_one_target_action() {
        let flow = compile(
            "version: 1\nname: x\nsteps:\n  - double_click: { target: { test_id: item } }\n",
        )
        .unwrap();
        assert!(matches!(
            &flow.steps[0].operation,
            Operation::DoubleClick {
                target: Locator::TestId(value)
            } if value.expose() == "item"
        ));
        assert!(error(
            "version: 1\nname: x\nsteps:\n  - click: { target: { css: x } }\n    double_click: { target: { css: x } }\n"
        )
        .contains("exactly one operation"));
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
}
