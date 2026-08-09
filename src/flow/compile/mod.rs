use super::compiled::CompiledFlow;
use super::parse::{parse_yaml, read_flow};
use super::raw::{RawFlow, VideoMode};
use super::validate::{
    validate_expanded_steps, validate_expanded_steps_with_outputs, validate_page_switching_video,
};
use super::{FlowError, MAX_FLOW_SOURCE_BYTES, invalid, require_source_size, with_path};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
mod expansion;
mod steps;
pub(crate) use expansion::{ExpansionState, compile_raw_expanded};
pub(crate) use steps::compile_raw_inner;

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
