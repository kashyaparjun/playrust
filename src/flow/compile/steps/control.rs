use super::super::super::compiled::Viewport;
use super::super::super::compiled::{
    Assertion, CompiledFlow, CompiledStep, Crop, Expression, FrameSwitch, Guard, GuardKind,
    Locator, LocatorRelation, LocatorStrategy, Operation, RelationKind, RuntimeValue,
    SettleCondition, UrlExpectation, VisualExpectation, When,
};
use super::super::super::duration::parse_duration;
use super::super::super::interpolate::{
    compile_runtime_value, compile_save_as, interpolate, interpolate_non_empty,
    interpolate_non_secret,
};
use super::super::super::parse::{
    parse_absolute_url, parse_key, parse_modifier, parse_url_path, validate_http_url,
};
use super::super::super::raw::*;
use super::super::super::redact::Redactor;
use super::super::super::validate::{
    is_windows_reserved_name, validate_baseline_path, validate_count, validate_crop,
    validate_gesture_delta, validate_input_name, validate_screenshot_name,
};
use super::super::super::{
    DEFAULT_LONG_PRESS_DURATION, DEFAULT_SWIPE_DURATION, DEFAULT_TIMEOUT, FlowError,
    MAX_EXPRESSION_DEPTH, MAX_EXPRESSION_NODES, MAX_FLOW_STEPS, MAX_GESTURE_DURATION,
    MAX_HTTP_HEADERS, MAX_LOCATOR_DEPTH, MAX_PAUSE_DURATION, MAX_REPEAT, MAX_RETRIES,
    MAX_WHILE_ITERATIONS, MIN_SECRET_LEN, Resolved, invalid, meets_min_secret_len,
    require_non_empty, require_scalar_size,
};
use crate::browser::Geolocation;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

use super::super::super::compiled::FlowSettings;
use super::operations::{
    compile_assertion, compile_gesture_duration, compile_locator, compile_locator_at,
    compile_operation, compile_settle,
};

pub(crate) fn compile_raw_inner(
    raw: RawFlow,
    source_path: PathBuf,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    passed_vars: &BTreeMap<String, Resolved<String>>,
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

    let manual_recording = raw.steps.iter().any(|step| step.recording.is_some());
    let inputs = resolve_inputs(
        &raw.vars,
        &raw.secrets,
        cli_vars,
        environment,
        passed_vars,
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
    let overlays = raw.settings.overlays.unwrap_or_default();

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
        overlays,
    };
    let mut ids = BTreeSet::new();
    let mut screenshot_names = BTreeSet::new();
    let mut steps = Vec::with_capacity(raw.steps.len());
    for (offset, step) in raw.steps.into_iter().enumerate() {
        let index = step.source_index.unwrap_or(offset + 1);
        let repeat = validate_count("repeat", index, step.repeat.unwrap_or(1), MAX_REPEAT)?;
        let while_control = step
            .r#while
            .clone()
            .map(|control| compile_while(control, index, &inputs))
            .transpose()?;
        let retries = step
            .retry
            .map(|value| validate_count("retry", index, value, MAX_RETRIES))
            .transpose()?
            .unwrap_or(0);
        if retries > 0 && step.assertion.is_none() {
            return invalid(format!(
                "step {index} retry is only supported for assertions"
            ));
        }
        if retries > 0 && step.when.is_some() {
            return invalid(format!("step {index} cannot combine when and retry"));
        }
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
            if repeat > 1 || while_control.is_some() {
                return invalid(format!(
                    "step {index} cannot combine id and repeat or while"
                ));
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
        let when = compile_when(step.when.clone(), index, &inputs)?;
        let operation = compile_operation(
            step,
            index,
            &source_path,
            base_url.as_ref(),
            viewport,
            &inputs,
        )?;
        let skip = matches!(when, CompiledWhen::Skip);
        let when = match when {
            CompiledWhen::Runtime(when) => Some(when),
            CompiledWhen::Always | CompiledWhen::Skip => None,
        };
        if skip {
            continue;
        }
        if let Operation::Screenshot { name, .. } = &operation
            && !screenshot_names.insert(name.to_ascii_lowercase())
        {
            return invalid(format!("duplicate screenshot name {name:?}"));
        }
        for _ in 0..repeat {
            let iterations = while_control
                .as_ref()
                .map_or(1, |(_, max_iterations)| *max_iterations);
            for iteration in 0..iterations {
                let guards = while_control
                    .as_ref()
                    .map(|(expression, _)| {
                        vec![Guard {
                            id: 0,
                            first: true,
                            kind: GuardKind::While {
                                loop_id: 0,
                                new_loop: iteration == 0,
                                expression: expression.clone(),
                            },
                        }]
                    })
                    .unwrap_or_default();
                steps.push(CompiledStep {
                    index,
                    source: source_path.clone(),
                    source_index: index,
                    id: id.clone(),
                    timeout: step_timeout,
                    when: when.clone(),
                    guards,
                    retries,
                    operation: operation.clone(),
                });
            }
        }
        if steps.len() > MAX_FLOW_STEPS {
            return invalid(format!("expanded steps must not exceed {MAX_FLOW_STEPS}"));
        }
    }

    Ok(CompiledFlow {
        source: source_path,
        name,
        base_url,
        settings,
        inputs,
        steps,
        manual_recording,
        redactor,
    })
}
pub(crate) enum CompiledWhen {
    Always,
    Skip,
    Runtime(When),
}

pub(crate) fn compile_when(
    raw: Option<RawWhen>,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<CompiledWhen, FlowError> {
    let Some(raw) = raw else {
        return Ok(CompiledWhen::Always);
    };
    let count = [
        raw.visible.is_some(),
        raw.hidden.is_some(),
        raw.variable.is_some(),
        raw.platform.is_some(),
        raw.expression.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if count != 1 {
        return invalid(format!(
            "step {index} when must contain exactly one predicate"
        ));
    }
    if let Some(locator) = raw.visible {
        return Ok(CompiledWhen::Runtime(When::Visible(compile_locator(
            locator, index, inputs,
        )?)));
    }
    if let Some(locator) = raw.hidden {
        return Ok(CompiledWhen::Runtime(When::Hidden(compile_locator(
            locator, index, inputs,
        )?)));
    }
    if raw.platform.is_some() {
        return Ok(CompiledWhen::Always);
    }
    if let Some(expression) = raw.expression {
        return Ok(CompiledWhen::Runtime(When::Expression(compile_expression(
            expression, index, inputs,
        )?)));
    }
    let predicate = raw.variable.expect("predicate count checked");
    validate_input_name(&predicate.name)?;
    let actual = inputs.get(&predicate.name).ok_or_else(|| {
        FlowError::Invalid(format!(
            "step {index} when.variable references unknown variable {:?}",
            predicate.name
        ))
    })?;
    if actual.is_secret() {
        return invalid(format!(
            "step {index} when.variable must reference a non-secret variable"
        ));
    }
    let expected = interpolate(
        &format!("step {index} when.variable.equals"),
        &predicate.equals,
        inputs,
    )?;
    if expected.is_secret() {
        return invalid(format!(
            "step {index} when.variable.equals cannot contain a secret"
        ));
    }
    Ok(if actual.expose() == expected.expose() {
        CompiledWhen::Always
    } else {
        CompiledWhen::Skip
    })
}

pub(crate) fn compile_while(
    control: RawWhile,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<(Expression, usize), FlowError> {
    let max_iterations = validate_count(
        "while.max_iterations",
        index,
        control.max_iterations,
        MAX_WHILE_ITERATIONS,
    )?;
    Ok((
        compile_expression(control.expression, index, inputs)?,
        max_iterations,
    ))
}

pub(crate) fn compile_expression(
    raw: RawExpression,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Expression, FlowError> {
    let mut nodes = 0;
    compile_expression_inner(raw, index, inputs, 0, &mut nodes)
}

pub(crate) fn compile_expression_inner(
    raw: RawExpression,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
    depth: usize,
    nodes: &mut usize,
) -> Result<Expression, FlowError> {
    if depth > MAX_EXPRESSION_DEPTH {
        return invalid(format!(
            "step {index} expression exceeds maximum depth {MAX_EXPRESSION_DEPTH}"
        ));
    }
    *nodes += 1;
    if *nodes > MAX_EXPRESSION_NODES {
        return invalid(format!(
            "step {index} expression exceeds maximum size {MAX_EXPRESSION_NODES}"
        ));
    }
    let count = [
        raw.all.is_some(),
        raw.any.is_some(),
        raw.not.is_some(),
        raw.equals.is_some(),
        raw.not_equals.is_some(),
        raw.boolean.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if count != 1 {
        return invalid(format!(
            "step {index} expression must contain exactly one operator"
        ));
    }
    let is_all = raw.all.is_some();
    if let Some(children) = raw.all.or(raw.any) {
        if children.is_empty() {
            return invalid(format!("step {index} expression list must not be empty"));
        }
        let children = children
            .into_iter()
            .map(|child| compile_expression_inner(child, index, inputs, depth + 1, nodes))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(if is_all {
            Expression::All(children)
        } else {
            Expression::Any(children)
        });
    }
    if let Some(child) = raw.not {
        return Ok(Expression::Not(Box::new(compile_expression_inner(
            *child,
            index,
            inputs,
            depth + 1,
            nodes,
        )?)));
    }
    let is_equals = raw.equals.is_some();
    let comparison = raw.equals.or(raw.not_equals);
    if let Some(comparison) = comparison {
        let left = compile_runtime_value(
            &format!("step {index} expression.left"),
            &comparison.left,
            inputs,
        )?;
        let right = compile_runtime_value(
            &format!("step {index} expression.right"),
            &comparison.right,
            inputs,
        )?;
        return Ok(if is_equals {
            Expression::Equals(left, right)
        } else {
            Expression::NotEquals(left, right)
        });
    }
    Ok(Expression::Boolean(compile_runtime_value(
        &format!("step {index} expression.boolean"),
        raw.boolean.as_deref().expect("operator count checked"),
        inputs,
    )?))
}

pub(crate) fn compile_run_vars(
    vars: &BTreeMap<String, String>,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<BTreeMap<String, Resolved<String>>, FlowError> {
    vars.iter()
        .map(|(name, value)| {
            validate_input_name(name)?;
            let value = interpolate(&format!("step {index} run.vars.{name}"), value, inputs)?;
            require_non_empty(&format!("step {index} run.vars.{name}"), value.expose())?;
            Ok((name.clone(), value))
        })
        .collect()
}
fn resolve_inputs(
    vars: &BTreeMap<String, RawVariable>,
    secrets: &BTreeMap<String, RawSecret>,
    cli_vars: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    passed_vars: &BTreeMap<String, Resolved<String>>,
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
    if let Some(name) = passed_vars.keys().find(|name| !vars.contains_key(*name)) {
        return invalid(format!("run variable {name:?} is not declared under vars"));
    }

    let mut resolved = BTreeMap::new();
    for (name, raw) in vars {
        let value = if let Some(value) = passed_vars.get(name) {
            require_scalar_size(&format!("vars.{name}"), value.expose())?;
            require_non_empty(&format!("vars.{name}"), value.expose())?;
            resolved.insert(name.clone(), value.clone());
            continue;
        } else if let Some(value) = cli_vars.get(name) {
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
        if !meets_min_secret_len(&value) {
            return invalid(format!(
                "secrets.{name} must be at least {MIN_SECRET_LEN} characters so it does not over-redact diagnostics"
            ));
        }
        resolved.insert(name.clone(), Resolved::new(value, true));
    }
    Ok(resolved)
}
