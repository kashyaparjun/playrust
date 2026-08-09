use super::compiled::{Key, Modifier, NamedKey};
use super::raw::RawFlow;
use super::{
    FlowError, MAX_FLOW_SOURCE_BYTES, Resolved, discover_directory, invalid, is_subflow, is_yaml,
    normalized_path, require_non_empty, require_scalar_size, require_source_size, with_path,
};
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use url::Url;

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
pub(crate) fn read_flow(path: &Path) -> Result<RawFlow, FlowError> {
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
pub(crate) fn parse_absolute_url(
    context: &str,
    value: Resolved<String>,
) -> Result<Resolved<Url>, FlowError> {
    let url = Url::parse(value.expose())
        .map_err(|_| FlowError::Invalid(format!("{context} must be an absolute URL")))?;
    Ok(Resolved::new(
        validate_http_url(context, url)?,
        value.secret,
    ))
}

pub(crate) fn validate_http_url(context: &str, url: Url) -> Result<Url, FlowError> {
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return invalid(format!("{context} must use an absolute http or https URL"));
    }
    Ok(url)
}

pub(crate) fn parse_url_path(
    index: usize,
    value: Resolved<String>,
) -> Result<Resolved<String>, FlowError> {
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

pub(crate) fn parse_key(index: usize, value: &str) -> Result<Key, FlowError> {
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

pub(crate) fn parse_modifier(index: usize, value: &str) -> Result<Modifier, FlowError> {
    match value {
        "Alt" => Ok(Modifier::Alt),
        "Control" => Ok(Modifier::Control),
        "Meta" => Ok(Modifier::Meta),
        "Shift" => Ok(Modifier::Shift),
        _ => invalid(format!("step {index} has unsupported modifier {value:?}")),
    }
}
