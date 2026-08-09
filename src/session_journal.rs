#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;
use thiserror::Error;

const REDACTED: &str = "[REDACTED]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ReplayValue {
    Literal(String),
    Environment(SecretReference),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretReference {
    pub env: String,
}

impl SecretReference {
    pub fn from_env(name: impl Into<String>) -> Self {
        Self { env: name.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableLocator {
    pub strategy: LocatorStrategy,
    pub index: Option<usize>,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub focused: Option<bool>,
    pub enabled: Option<bool>,
    pub relations: BTreeMap<RelationKind, Box<DurableLocator>>,
}

impl DurableLocator {
    pub fn new(strategy: LocatorStrategy) -> Self {
        Self {
            strategy,
            index: None,
            checked: None,
            selected: None,
            focused: None,
            enabled: None,
            relations: BTreeMap::new(),
        }
    }
}

impl Serialize for DurableLocator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        match &self.strategy {
            LocatorStrategy::Css(value) => map.serialize_entry("css", value)?,
            LocatorStrategy::TestId(value) => map.serialize_entry("test_id", value)?,
            LocatorStrategy::Text { value, match_kind } => {
                map.serialize_entry("text", &TextLocator { value, match_kind })?
            }
            LocatorStrategy::Label(value) => map.serialize_entry("label", value)?,
            LocatorStrategy::Role { value, name } => {
                map.serialize_entry("role", &RoleLocator { value, name })?
            }
        }
        if let Some(value) = self.index {
            map.serialize_entry("index", &value)?;
        }
        if let Some(value) = self.checked {
            map.serialize_entry("checked", &value)?;
        }
        if let Some(value) = self.selected {
            map.serialize_entry("selected", &value)?;
        }
        if let Some(value) = self.focused {
            map.serialize_entry("focused", &value)?;
        }
        if let Some(value) = self.enabled {
            map.serialize_entry("enabled", &value)?;
        }
        for (kind, locator) in &self.relations {
            map.serialize_entry(kind.as_str(), locator)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorStrategy {
    Css(String),
    TestId(String),
    Text {
        value: String,
        match_kind: TextMatch,
    },
    Label(String),
    Role {
        value: String,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationKind {
    Within,
    ChildOf,
    Has,
    Above,
    Below,
    Left,
    Right,
}

impl RelationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Within => "within",
            Self::ChildOf => "child_of",
            Self::Has => "has",
            Self::Above => "above",
            Self::Below => "below",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TextMatch {
    Exact,
    Contains,
}

#[derive(Serialize)]
struct TextLocator<'a> {
    value: &'a str,
    #[serde(rename = "match")]
    match_kind: &'a TextMatch,
}

#[derive(Serialize)]
struct RoleLocator<'a> {
    value: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: &'a Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayOperation {
    Open(ReplayValue),
    Click(TargetAction),
    DoubleClick(TargetAction),
    Fill(ValueAction),
    Erase(TargetAction),
    Select(ValueAction),
    Press(PressAction),
    Back(EmptyAction),
    SwitchPage(Value),
    SwitchFrame(Value),
    WaitUntilVisible(TargetAction),
    WaitUntilStable(TargetAction),
    Scroll(ScrollAction),
    Dialog(DialogAction),
    Assert(Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TargetAction {
    pub target: DurableLocator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueAction {
    pub target: DurableLocator,
    pub value: ReplayValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PressAction {
    pub target: DurableLocator,
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EmptyAction {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ScrollAction {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub x: i64,
    pub y: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DialogAction {
    pub action: DialogResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ReplayValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogResponse {
    Accept,
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActionOutcome {
    Success,
    Failed { error: String },
}

impl ActionOutcome {
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Success)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEvent {
    Settings {
        settings: Value,
    },
    Snapshot {
        snapshot_revision: u64,
        summary: Value,
    },
    Action {
        operation: Box<ReplayOperation>,
        outcome: ActionOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        durable_locator: Option<DurableLocator>,
    },
    ReplayStep {
        step: Value,
        outcome: ActionOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        durable_locator: Option<Value>,
    },
    DialogHandled {
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<Value>,
    },
    ObservedNavigation {
        url: String,
    },
    Submission {
        outcome: ActionOutcome,
    },
    RecorderWarning {
        warning: String,
    },
    Artifact {
        kind: String,
        path: PathBuf,
    },
    Export {
        name: String,
    },
    Close {
        summary: Value,
    },
}

impl JournalEvent {
    fn replay_operation(&self) -> Option<&ReplayOperation> {
        match self {
            Self::Action {
                operation, outcome, ..
            } if outcome.succeeded() => Some(operation.as_ref()),
            _ => None,
        }
    }

    fn replay_step(&self) -> Result<Option<Value>, ExportError> {
        match self {
            Self::ReplayStep { step, outcome, .. } if outcome.succeeded() => Ok(Some(step.clone())),
            Self::DialogHandled { action, text } => {
                let mut dialog = serde_json::Map::new();
                dialog.insert("action".to_owned(), Value::String(action.clone()));
                if let Some(text) = text {
                    dialog.insert("text".to_owned(), text.clone());
                }
                Ok(Some(serde_json::json!({ "dialog": dialog })))
            }
            _ => self
                .replay_operation()
                .map(serde_json::to_value)
                .transpose()
                .map_err(ExportError::StepJson),
        }
    }
}

#[derive(Serialize)]
struct JournalEnvelope<'a> {
    sequence: u64,
    #[serde(flatten)]
    event: &'a JournalEvent,
}

pub struct JournalWriter {
    writer: BufWriter<File>,
    next_sequence: u64,
    secrets: Vec<String>,
}

impl JournalWriter {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let next_sequence = read_next_sequence(path)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            next_sequence,
            secrets: Vec::new(),
        })
    }

    pub fn register_secret(&mut self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.is_empty() || self.secrets.contains(&secret) {
            return;
        }
        self.secrets.push(secret);
        self.secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    }

    pub fn append(&mut self, event: &JournalEvent) -> io::Result<u64> {
        let sequence = self.next_sequence;
        let mut value =
            serde_json::to_value(JournalEnvelope { sequence, event }).map_err(io::Error::other)?;
        redact_value(&mut value, &self.secrets);
        let mut line = serde_json::to_vec(&value).map_err(io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("journal sequence exhausted"))?;
        Ok(sequence)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn read_next_sequence(path: &Path) -> io::Result<u64> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(1),
        Err(error) => return Err(error),
    };
    let mut next = 1_u64;
    for line in BufReader::new(file).lines() {
        let value: Value = serde_json::from_str(&line?).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid journal: {error}"),
            )
        })?;
        let sequence = value
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing sequence"))?;
        if sequence != next {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected journal sequence {next}, found {sequence}"),
            ));
        }
        next = next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("journal sequence exhausted"))?;
    }
    Ok(next)
}

fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                *text = text.replace(secret, REDACTED);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, secrets);
            }
        }
        Value::Object(map) => {
            let old = std::mem::take(map);
            for (mut key, mut value) in old {
                for secret in secrets {
                    key = key.replace(secret, REDACTED);
                }
                redact_value(&mut value, secrets);
                map.insert(key, value);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayExport {
    pub yaml: String,
    pub step_count: usize,
    pub secret_count: usize,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("bundle name must be a safe path component")]
    UnsafeName,
    #[error("replay has no successful explicit operations")]
    EmptyReplay,
    #[error("failed to serialize replay step: {0}")]
    StepJson(#[from] serde_json::Error),
    #[error("failed to serialize replay YAML: {0}")]
    Yaml(#[from] serde_saphyr::ser_error::Error),
    #[error("failed to publish replay to {path}: {source}")]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Serialize)]
struct ReplayFlow {
    version: u32,
    name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    secrets: BTreeMap<String, SecretReference>,
    steps: Vec<Value>,
}

pub fn build_replay_yaml(name: &str, events: &[JournalEvent]) -> Result<ReplayExport, ExportError> {
    if !is_safe_bundle_name(name) {
        return Err(ExportError::UnsafeName);
    }
    let mut steps = Vec::new();
    for event in events {
        if let Some(step) = event.replay_step()? {
            steps.push(step);
        }
    }
    if steps.is_empty() {
        return Err(ExportError::EmptyReplay);
    }

    let mut lifter = SecretLifter::default();
    for step in &mut steps {
        lifter.lift(step, &mut Vec::new());
    }
    let flow = ReplayFlow {
        version: 1,
        name: name.to_owned(),
        secrets: lifter.secrets,
        steps,
    };
    let yaml = serde_saphyr::to_string_with_options(
        &flow,
        serde_saphyr::ser_options! {
            indent_step: 2,
            compact_list_indent: true,
            quote_all: true,
        },
    )?;
    Ok(ReplayExport {
        step_count: flow.steps.len(),
        secret_count: flow.secrets.len(),
        yaml,
    })
}

#[derive(Default)]
struct SecretLifter {
    secrets: BTreeMap<String, SecretReference>,
    names_by_env: HashMap<String, String>,
}

impl SecretLifter {
    fn lift(&mut self, value: &mut Value, path: &mut Vec<String>) {
        if let Some(environment) = environment_reference(value) {
            let logical_name = self.logical_name(path, &environment);
            *value = Value::String(format!("${{{logical_name}}}"));
            return;
        }
        match value {
            Value::Array(values) => {
                for (index, value) in values.iter_mut().enumerate() {
                    path.push(index.to_string());
                    self.lift(value, path);
                    path.pop();
                }
            }
            Value::Object(map) => {
                for (key, value) in map {
                    path.push(key.clone());
                    self.lift(value, path);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    fn logical_name(&mut self, path: &[String], environment: &str) -> String {
        if let Some(name) = self.names_by_env.get(environment) {
            return name.clone();
        }
        let field = path
            .iter()
            .rev()
            .find(|part| part.bytes().any(|byte| byte.is_ascii_alphabetic()))
            .map(String::as_str)
            .unwrap_or("secret");
        let base = identifier(&format!("{field}_{environment}"));
        let mut name = base.clone();
        let mut suffix = 2;
        while self.secrets.contains_key(&name) {
            name = format!("{base}_{suffix}");
            suffix += 1;
        }
        self.secrets.insert(
            name.clone(),
            SecretReference::from_env(environment.to_owned()),
        );
        self.names_by_env
            .insert(environment.to_owned(), name.clone());
        name
    }
}

fn environment_reference(value: &Value) -> Option<String> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    map.get("env")?.as_str().map(str::to_owned)
}

fn identifier(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            result.push(byte.to_ascii_lowercase() as char);
        } else if !result.ends_with('_') {
            result.push('_');
        }
    }
    let result = result.trim_matches('_');
    if result.is_empty() {
        "secret".to_owned()
    } else if result.as_bytes()[0].is_ascii_digit() {
        format!("secret_{result}")
    } else {
        result.to_owned()
    }
}

pub fn is_safe_bundle_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn publish_replay_atomic(path: impl AsRef<Path>, yaml: &str) -> Result<(), ExportError> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ExportError::Publish {
        path: path.to_owned(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| ExportError::Publish {
            path: path.to_owned(),
            source,
        })?;
    temporary
        .write_all(yaml.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| ExportError::Publish {
            path: path.to_owned(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| ExportError::Publish {
            path: path.to_owned(),
            source: error.error,
        })?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn test_id(value: &str) -> DurableLocator {
        DurableLocator::new(LocatorStrategy::TestId(value.to_owned()))
    }

    fn success(operation: ReplayOperation) -> JournalEvent {
        JournalEvent::Action {
            operation: Box::new(operation),
            outcome: ActionOutcome::Success,
            durable_locator: None,
        }
    }

    #[test]
    fn journal_is_sequence_numbered_and_redacted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.ndjson");
        let mut journal = JournalWriter::open(&path).unwrap();
        journal.register_secret("tok\"en");
        assert_eq!(
            journal
                .append(&JournalEvent::RecorderWarning {
                    warning: "bad tok\"en and tok\"en".to_owned(),
                })
                .unwrap(),
            1
        );
        drop(journal);

        let source = fs::read_to_string(&path).unwrap();
        assert!(!source.contains("tok"));
        let line: Value = serde_json::from_str(source.trim()).unwrap();
        assert_eq!(line["sequence"], 1);
        assert_eq!(line["warning"], "bad [REDACTED] and [REDACTED]");

        let mut reopened = JournalWriter::open(&path).unwrap();
        assert_eq!(
            reopened
                .append(&JournalEvent::Export {
                    name: "bundle".to_owned(),
                })
                .unwrap(),
            2
        );
    }

    #[test]
    fn bundle_names_are_safe_path_components() {
        for valid in ["run", "wonderway-run_2", "A1"] {
            assert!(is_safe_bundle_name(valid), "{valid}");
        }
        for invalid in ["", ".", "..", ".hidden", "../run", "a/b", "a b"] {
            assert!(!is_safe_bundle_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn export_is_deterministic_and_structured() {
        let events = vec![
            JournalEvent::Snapshot {
                snapshot_revision: 1,
                summary: Value::Null,
            },
            success(ReplayOperation::Open(ReplayValue::Literal(
                "https://example.com".to_owned(),
            ))),
            success(ReplayOperation::Click(TargetAction {
                target: test_id("submit"),
            })),
            JournalEvent::ObservedNavigation {
                url: "https://example.com/done".to_owned(),
            },
            success(ReplayOperation::Scroll(ScrollAction { x: 0, y: 600 })),
            success(ReplayOperation::WaitUntilVisible(TargetAction {
                target: test_id("result"),
            })),
            JournalEvent::ReplayStep {
                step: serde_json::json!({ "pause": "1500ms" }),
                outcome: ActionOutcome::Success,
                durable_locator: None,
            },
            success(ReplayOperation::Dialog(DialogAction {
                action: DialogResponse::Accept,
                text: None,
            })),
            success(ReplayOperation::Assert(serde_json::json!({
                "visible": { "test_id": "result" }
            }))),
        ];
        let first = build_replay_yaml("example-run", &events).unwrap();
        let second = build_replay_yaml("example-run", &events).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.step_count, 7);
        assert!(first.yaml.contains("test_id: 'submit'"));
        assert!(!first.yaml.contains("snapshot_revision"));
        assert!(!first.yaml.contains("/done"));
        assert!(first.yaml.contains("dialog:"));
        assert!(first.yaml.contains("pause: '1500ms'"));

        #[derive(Deserialize)]
        struct FlowShape {
            version: u32,
            name: String,
            steps: Vec<Value>,
        }
        let flow: FlowShape = serde_saphyr::from_str(&first.yaml).unwrap();
        assert_eq!(flow.version, 1);
        assert_eq!(flow.name, "example-run");
        assert_eq!(flow.steps.len(), 7);
    }

    #[test]
    fn failed_actions_do_not_enter_replay() {
        let events = vec![
            JournalEvent::Action {
                operation: Box::new(ReplayOperation::Click(TargetAction {
                    target: test_id("missing"),
                })),
                outcome: ActionOutcome::Failed {
                    error: "not found".to_owned(),
                },
                durable_locator: Some(test_id("missing")),
            },
            success(ReplayOperation::Back(EmptyAction::default())),
        ];
        let replay = build_replay_yaml("failed-excluded", &events).unwrap();
        assert_eq!(replay.step_count, 1);
        assert!(!replay.yaml.contains("missing"));
        assert!(!replay.yaml.contains("not found"));
    }

    #[test]
    fn environment_secrets_are_lifted_without_resolved_values() {
        let events = vec![success(ReplayOperation::Fill(ValueAction {
            target: test_id("password"),
            value: ReplayValue::Environment(SecretReference::from_env("APP_PASSWORD")),
        }))];
        let replay = build_replay_yaml("secret-run", &events).unwrap();
        assert_eq!(replay.secret_count, 1);
        assert!(replay.yaml.contains("env: 'APP_PASSWORD'"));
        assert!(replay.yaml.contains("value: '${value_app_password}'"));
        assert!(!replay.yaml.contains("resolved-password"));
    }

    #[test]
    fn replay_publication_replaces_complete_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replay.yaml");
        fs::write(&path, "old").unwrap();
        publish_replay_atomic(&path, "new\ncontent\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "new\ncontent\n");
    }
}
