#![allow(unused_imports)]
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
mod compile;
mod compiled;
mod duration;
mod interpolate;
mod parse;
mod raw;
mod redact;
mod validate;
pub use compile::{
    compile_file, compile_file_with_env, compile_file_with_video, compile_inline_yaml,
    compile_inline_yaml_with_video, compile_raw, compile_yaml, compile_yaml_with_env,
};
pub use compiled::*;
pub use duration::parse_duration;
pub use parse::{artifact_key, discover_flow_files, parse_yaml};
pub use raw::{
    ClearTarget, NativeDialogResponse, PageSwitch, Platform, RawFlow, RecordingControl,
    RelativePoint, TextMatch, VideoMode,
};
pub use redact::{REDACTED, Redactor};
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_FLOW_SOURCE_BYTES: usize = 1024 * 1024;
pub const MAX_FLOW_STEPS: usize = 10_000;
pub const MAX_SUBFLOW_DEPTH: usize = 32;
pub const MAX_LOCATOR_DEPTH: usize = 8;
pub const MAX_REPEAT: usize = 100;
pub const MAX_WHILE_ITERATIONS: usize = 100;
pub const MAX_EXPRESSION_DEPTH: usize = 8;
pub const MAX_EXPRESSION_NODES: usize = 64;
pub const MAX_RETRIES: usize = 10;
pub const MAX_SCALAR_BYTES: usize = 64 * 1024;
pub const MAX_RUNTIME_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_HTTP_HEADERS: usize = 100;
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const MAX_PAUSE_DURATION: Duration = Duration::from_secs(60);
pub const MAX_GESTURE_DELTA: i32 = 10_000;
pub const MAX_GESTURE_DURATION: Duration = Duration::from_secs(10);
pub const DEFAULT_SWIPE_DURATION: Duration = Duration::from_millis(300);
pub const DEFAULT_LONG_PRESS_DURATION: Duration = Duration::from_millis(500);
pub const MIN_SECRET_LEN: usize = 4;

pub(crate) fn meets_min_secret_len(value: &str) -> bool {
    value.chars().count() >= MIN_SECRET_LEN
}

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
pub(crate) fn require_source_size(length: usize) -> Result<(), FlowError> {
    if length > MAX_FLOW_SOURCE_BYTES {
        return invalid(format!(
            "flow source exceeds the maximum size of {MAX_FLOW_SOURCE_BYTES} bytes"
        ));
    }
    Ok(())
}

pub(crate) fn require_scalar_size(context: &str, value: &str) -> Result<(), FlowError> {
    if value.len() > MAX_SCALAR_BYTES {
        return invalid(format!(
            "{context} exceeds the maximum scalar size of {MAX_SCALAR_BYTES} bytes"
        ));
    }
    Ok(())
}

pub(crate) fn require_non_empty(context: &str, value: &str) -> Result<(), FlowError> {
    if value.trim().is_empty() {
        return invalid(format!("{context} must not be empty"));
    }
    Ok(())
}

pub(crate) fn discover_directory(
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), FlowError> {
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

pub(crate) fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("yaml" | "yml")
    )
}

pub(crate) fn is_subflow(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".subflow.yaml") || name.ends_with(".subflow.yml"))
}

pub(crate) fn with_path(path: &Path, error: FlowError) -> FlowError {
    match error {
        FlowError::Yaml(message) => FlowError::Yaml(format!("{}: {message}", path.display())),
        FlowError::Invalid(message) => FlowError::Invalid(format!("{}: {message}", path.display())),
        error @ FlowError::Io { .. } => error,
    }
}

pub(crate) fn normalized_path(path: &Path) -> String {
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
