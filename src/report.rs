use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REPORT_VERSION: u32 = 2;
pub const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    Specification = 2,
    Automation = 3,
    Infrastructure = 4,
    Interrupted = 130,
}

impl ExitCode {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    const fn precedence(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Specification => 1,
            Self::Automation => 2,
            Self::Infrastructure => 3,
            Self::Interrupted => 4,
        }
    }
}

pub fn aggregate_exit_code(codes: impl IntoIterator<Item = ExitCode>) -> ExitCode {
    codes
        .into_iter()
        .max_by_key(|code| code.precedence())
        .unwrap_or(ExitCode::Success)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Specification,
    Input,
    BrowserLaunch,
    Protocol,
    Navigation,
    Locator,
    Actionability,
    Assertion,
    Timeout,
    BrowserCrash,
    Recording,
}

impl FailureCategory {
    pub const fn exit_code(self) -> ExitCode {
        match self {
            Self::Specification | Self::Input => ExitCode::Specification,
            Self::Navigation
            | Self::Locator
            | Self::Actionability
            | Self::Assertion
            | Self::Timeout => ExitCode::Automation,
            Self::BrowserLaunch | Self::Protocol | Self::BrowserCrash | Self::Recording => {
                ExitCode::Infrastructure
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SafeText(String);

impl SafeText {
    pub fn public(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn secret() -> Self {
        Self(REDACTED.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SafeText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut secrets: Vec<_> = secrets
            .into_iter()
            .map(Into::into)
            .filter(|secret| !secret.is_empty())
            .collect();
        secrets.sort_unstable_by(|left, right| {
            right.len().cmp(&left.len()).then_with(|| left.cmp(right))
        });
        secrets.dedup();
        Self { secrets }
    }

    pub fn sanitize(&self, value: impl AsRef<str>) -> SafeText {
        let mut value = value.as_ref().to_owned();
        for secret in &self.secrets {
            value = value.replace(secret, REDACTED);
        }
        SafeText(value)
    }

    pub fn display<'a>(&'a self, value: &'a str) -> RedactedDisplay<'a> {
        RedactedDisplay {
            redactor: self,
            value,
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

pub struct RedactedDisplay<'a> {
    redactor: &'a Redactor,
    value: &'a str,
}

impl fmt::Display for RedactedDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.redactor.sanitize(self.value).fmt(formatter)
    }
}

impl fmt::Debug for RedactedDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StepContext {
    /// One-based YAML step number.
    pub number: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locator: Option<SafeText>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Failure {
    pub category: FailureCategory,
    pub message: SafeText,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<StepContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_url: Option<SafeText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_observed: Option<SafeText>,
}

impl Failure {
    pub fn new(category: FailureCategory, message: SafeText) -> Self {
        Self {
            category,
            message,
            step: None,
            current_url: None,
            timeout_ms: None,
            last_observed: None,
        }
    }

    pub const fn exit_code(&self) -> ExitCode {
        self.category.exit_code()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactPaths {
    pub directory: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_screenshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_recording: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    Passed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowReport {
    pub name: String,
    pub path: String,
    pub duration_ms: u64,
    pub status: FlowStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
    pub artifacts: ArtifactPaths,
}

impl FlowReport {
    pub fn exit_code(&self) -> ExitCode {
        match self.status {
            FlowStatus::Passed => ExitCode::Success,
            FlowStatus::Interrupted => ExitCode::Interrupted,
            FlowStatus::Failed => {
                let code = aggregate_exit_code(self.failures.iter().map(Failure::exit_code));
                if code == ExitCode::Success {
                    ExitCode::Infrastructure
                } else {
                    code
                }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunnerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChromiumInfo {
    pub version: String,
    pub executable: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateStatus {
    Passed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AggregateReport {
    pub report_version: u32,
    pub runner: RunnerInfo,
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chromium: Option<ChromiumInfo>,
    pub duration_ms: u64,
    pub status: AggregateStatus,
    pub exit_code: i32,
    pub flows: Vec<FlowReport>,
}

impl AggregateReport {
    pub fn new(
        runner: RunnerInfo,
        schema_version: u32,
        chromium: Option<ChromiumInfo>,
        duration_ms: u64,
        flows: Vec<FlowReport>,
    ) -> Self {
        let exit_code = aggregate_exit_code(flows.iter().map(FlowReport::exit_code));
        let status = match exit_code {
            ExitCode::Success => AggregateStatus::Passed,
            ExitCode::Interrupted => AggregateStatus::Interrupted,
            _ => AggregateStatus::Failed,
        };
        Self {
            report_version: REPORT_VERSION,
            runner,
            schema_version,
            chromium,
            duration_ms,
            status,
            exit_code: exit_code.as_i32(),
            flows,
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        aggregate_exit_code(self.flows.iter().map(FlowReport::exit_code))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactPathError {
    #[error("flow path must be relative and cannot contain '..': {0}")]
    NotRelative(PathBuf),
    #[error("flow path has no file name: {0}")]
    MissingFileName(PathBuf),
}

pub fn artifact_directory(
    root: &Path,
    relative_flow_path: &Path,
) -> Result<PathBuf, ArtifactPathError> {
    let normalized = normalize_relative_path(relative_flow_path)?;
    let file_name = relative_flow_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ArtifactPathError::MissingFileName(relative_flow_path.to_owned()))?;
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(file_name);
    let slug = safe_slug(stem);
    let digest = Sha256::digest(normalized.as_bytes());
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(root.join(format!("flow-{slug}-{hash}")))
}

fn normalize_relative_path(path: &Path) -> Result<String, ArtifactPathError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ArtifactPathError::NotRelative(path.to_owned()));
            }
        }
    }
    if parts.is_empty() {
        return Err(ArtifactPathError::MissingFileName(path.to_owned()));
    }
    Ok(parts.join("/"))
}

fn safe_slug(value: &str) -> String {
    let mut slug = String::with_capacity(value.len().min(64));
    let mut separator = false;
    for character in value.chars() {
        if slug.len() >= 64 {
            break;
        }
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "unnamed".to_owned()
    } else {
        slug
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteReportError {
    #[error("could not create artifacts directory {path}: {source}")]
    CreateDirectory { path: PathBuf, source: io::Error },
    #[error("could not create temporary report in {path}: {source}")]
    CreateTemporary { path: PathBuf, source: io::Error },
    #[error("could not serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("could not flush report: {0}")]
    Flush(io::Error),
    #[error("could not publish report to {path}: {source}")]
    Publish { path: PathBuf, source: io::Error },
}

pub fn write_aggregate_report(
    artifacts_root: &Path,
    report: &AggregateReport,
) -> Result<PathBuf, WriteReportError> {
    fs::create_dir_all(artifacts_root).map_err(|source| WriteReportError::CreateDirectory {
        path: artifacts_root.to_owned(),
        source,
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(artifacts_root).map_err(|source| {
        WriteReportError::CreateTemporary {
            path: artifacts_root.to_owned(),
            source,
        }
    })?;
    serde_json::to_writer_pretty(&mut temporary, report)?;
    temporary
        .write_all(b"\n")
        .map_err(WriteReportError::Flush)?;
    temporary.flush().map_err(WriteReportError::Flush)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(WriteReportError::Flush)?;

    let destination = artifacts_root.join("report.json");
    temporary
        .persist(&destination)
        .map_err(|error| WriteReportError::Publish {
            path: destination.clone(),
            source: error.error,
        })?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(status: FlowStatus, category: Option<FailureCategory>) -> FlowReport {
        FlowReport {
            name: "flow".to_owned(),
            path: "flows/flow.yaml".to_owned(),
            duration_ms: 12,
            status,
            failures: category
                .map(|category| Failure::new(category, SafeText::public("failed")))
                .into_iter()
                .collect(),
            artifacts: ArtifactPaths::default(),
        }
    }

    #[test]
    fn aggregate_exit_codes_follow_documented_precedence() {
        assert_eq!(aggregate_exit_code([]), ExitCode::Success);
        assert_eq!(
            aggregate_exit_code([
                ExitCode::Specification,
                ExitCode::Automation,
                ExitCode::Infrastructure,
            ]),
            ExitCode::Infrastructure
        );
        assert_eq!(
            aggregate_exit_code([ExitCode::Infrastructure, ExitCode::Interrupted]),
            ExitCode::Interrupted
        );
        assert_eq!(
            flow(FlowStatus::Failed, None).exit_code(),
            ExitCode::Infrastructure
        );
    }

    #[test]
    fn artifact_directories_are_safe_stable_and_path_specific() {
        let root = Path::new("artifacts");
        let first = artifact_directory(root, Path::new("admin/Login flow.yaml")).unwrap();
        let repeated = artifact_directory(root, Path::new("admin/Login flow.yaml")).unwrap();
        let other = artifact_directory(root, Path::new("public/Login flow.yaml")).unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, other);
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("flow-login-flow-"));
        assert!(
            name.chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        );
        assert!(artifact_directory(root, Path::new("../flow.yaml")).is_err());
        assert!(artifact_directory(root, Path::new("/flow.yaml")).is_err());
    }

    #[test]
    fn redactor_keeps_secrets_out_of_display_debug_and_json() {
        let redactor = Redactor::new(["token", "token-long", ""]);
        let safe = redactor.sanitize("url?key=token-long and token");

        assert_eq!(safe.as_str(), "url?key=[REDACTED] and [REDACTED]");
        assert!(!format!("{:?}", redactor).contains("token"));
        assert!(!format!("{:?}", redactor.display("value=token")).contains("token"));
        assert!(!serde_json::to_string(&safe).unwrap().contains("token"));
    }

    #[test]
    fn writes_versioned_aggregate_report_without_secrets() {
        let redactor = Redactor::new(["canary-secret"]);
        let mut failed = flow(FlowStatus::Failed, Some(FailureCategory::Assertion));
        failed.failures[0].message = redactor.sanitize("expected canary-secret");
        failed.artifacts.screenshots = vec!["artifacts/home.png".to_owned()];
        let report = AggregateReport::new(
            RunnerInfo {
                name: "playrust".to_owned(),
                version: "0.1.0".to_owned(),
            },
            1,
            None,
            12,
            vec![failed],
        );
        let directory = tempfile::tempdir().unwrap();

        let path = write_aggregate_report(directory.path(), &report).unwrap();
        let json = fs::read_to_string(path).unwrap();
        let decoded: AggregateReport = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.report_version, REPORT_VERSION);
        assert_eq!(decoded.status, AggregateStatus::Failed);
        assert_eq!(decoded.exit_code, ExitCode::Automation.as_i32());
        assert_eq!(
            decoded.flows[0].artifacts.screenshots,
            ["artifacts/home.png"]
        );
        assert!(!json.contains("canary-secret"));
        assert!(json.ends_with('\n'));
    }
}
