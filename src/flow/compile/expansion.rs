use super::super::compiled::{
    CompiledFlow, Guard, GuardKind, Operation, Viewport, When,
};
use super::super::parse::read_flow;
use super::super::raw::{RawFlow, RawRun, RawViewport, VideoMode};
use super::super::validate::{validate_count, validate_subflow_path};
use super::super::{
    FlowError, MAX_FLOW_STEPS, MAX_REPEAT, MAX_RETRIES, MAX_SUBFLOW_DEPTH, Resolved, invalid,
    with_path,
};
use super::steps::{
    CompiledWhen, compile_raw_inner, compile_run_vars, compile_when, compile_while,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct ExpansionState {
    pub(crate) active: Vec<(PathBuf, PathBuf)>,
    pub(crate) declared_cli_vars: BTreeSet<String>,
    pub(crate) browser_settings: Option<(Viewport, VideoMode)>,
}

pub(crate) fn compile_raw_expanded(
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
