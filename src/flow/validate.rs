use super::compiled::{
    CompiledFlow, CompiledStep, Crop, Expression, Guard, GuardKind, Operation, RuntimeValue,
    Viewport, When,
};
use super::raw::{RawCrop, RecordingControl, VideoMode};
use super::{
    FlowError, MAX_GESTURE_DELTA,
    invalid, is_subflow, require_non_empty, require_scalar_size,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) fn validate_subflow_path(
    run: &str,
    source: &Path,
    index: usize,
) -> Result<(), FlowError> {
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

pub(crate) fn validate_expanded_steps(steps: &mut [CompiledStep]) -> Result<(), FlowError> {
    validate_expanded_steps_with_outputs(steps, &BTreeSet::new())
}

pub(crate) fn validate_expanded_steps_with_outputs(
    steps: &mut [CompiledStep],
    available_outputs: &BTreeSet<String>,
) -> Result<(), FlowError> {
    let mut ids = BTreeSet::new();
    let mut screenshots = BTreeSet::new();
    let mut outputs = available_outputs
        .iter()
        .map(|name| (name.clone(), None))
        .collect::<BTreeMap<_, Option<(PathBuf, usize)>>>();
    let mut next_guard_id = 0;
    let mut next_loop_id = 0;
    let mut active_guards = Vec::<usize>::new();
    let mut active_loops = Vec::<Option<usize>>::new();
    for (offset, step) in steps.iter_mut().enumerate() {
        step.index = offset + 1;
        active_guards.truncate(step.guards.len());
        active_loops.truncate(step.guards.len());
        active_loops.resize(step.guards.len(), None);
        for (depth, guard) in step.guards.iter_mut().enumerate() {
            if guard.first {
                next_guard_id += 1;
                if active_guards.len() == depth {
                    active_guards.push(next_guard_id);
                } else {
                    active_guards[depth] = next_guard_id;
                }
            }
            guard.id = *active_guards.get(depth).ok_or_else(|| {
                FlowError::Invalid("invalid compiled control-flow guard".to_owned())
            })?;
            if let GuardKind::While {
                loop_id, new_loop, ..
            } = &mut guard.kind
            {
                if *new_loop {
                    next_loop_id += 1;
                    active_loops[depth] = Some(next_loop_id);
                }
                *loop_id =
                    active_loops.get(depth).copied().flatten().ok_or_else(|| {
                        FlowError::Invalid("invalid compiled while loop".to_owned())
                    })?;
            }
        }
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
        let runtime_values = step
            .guards
            .iter()
            .flat_map(|guard| expression_runtime_values(guard_expression(guard)))
            .chain(
                step.when
                    .iter()
                    .filter_map(|when| match when {
                        When::Expression(expression) => Some(expression),
                        _ => None,
                    })
                    .flat_map(expression_runtime_values),
            )
            .chain(operation_runtime_values(&step.operation));
        for name in runtime_values.flat_map(RuntimeValue::output_names) {
            if !outputs.contains_key(name) {
                return invalid(format!(
                    "step {} references unknown variable or runtime output {name:?} before it is saved",
                    step.index
                ));
            }
        }
        if let Some(name) = operation_save_as(&step.operation) {
            let producer = (step.source.clone(), step.source_index);
            if let Some(Some(existing)) = outputs.insert(name.to_owned(), Some(producer.clone()))
                && existing != producer
            {
                return invalid(format!("duplicate runtime output {name:?}"));
            }
        }
    }
    if steps.is_empty() {
        return invalid("expanded steps must not be empty");
    }
    let controls = steps
        .iter()
        .filter_map(|step| match step.operation {
            Operation::Recording(control) => Some((step.index, control)),
            _ => None,
        })
        .collect::<Vec<_>>();
    match controls.as_slice() {
        [] | [(_, RecordingControl::Start), (_, RecordingControl::Stop)] => {}
        [(step, RecordingControl::Stop), ..] => {
            return invalid(format!(
                "step {step} recording stop must follow recording start"
            ));
        }
        [(_, RecordingControl::Start)] => {
            return invalid("recording start requires one later recording stop");
        }
        _ => return invalid("a flow may contain only one recording start/stop pair"),
    }
    Ok(())
}

pub(crate) fn guard_expression(guard: &Guard) -> &Expression {
    match &guard.kind {
        GuardKind::When(expression) | GuardKind::While { expression, .. } => expression,
    }
}

pub(crate) fn expression_runtime_values(expression: &Expression) -> Vec<&RuntimeValue> {
    let mut values = Vec::new();
    collect_expression_runtime_values(expression, &mut values);
    values
}

fn collect_expression_runtime_values<'a>(
    expression: &'a Expression,
    values: &mut Vec<&'a RuntimeValue>,
) {
    match expression {
        Expression::All(children) | Expression::Any(children) => {
            for child in children {
                collect_expression_runtime_values(child, values);
            }
        }
        Expression::Not(child) => collect_expression_runtime_values(child, values),
        Expression::Equals(left, right) | Expression::NotEquals(left, right) => {
            values.extend([left, right]);
        }
        Expression::Boolean(value) => values.push(value),
    }
}

pub(crate) fn validate_page_switching_video(flow: &CompiledFlow) -> Result<(), FlowError> {
    if (flow.settings.video != VideoMode::Off
        || flow
            .steps
            .iter()
            .any(|step| matches!(step.operation, Operation::Recording(_))))
        && let Some(step) = flow
            .steps
            .iter()
            .find(|step| matches!(step.operation, Operation::SwitchPage(_)))
    {
        return invalid(format!(
            "step {} switch_page requires settings.video: off and no recording controls because recording cannot safely move between pages",
            step.index
        ));
    }
    Ok(())
}

pub(crate) fn operation_runtime_values(
    operation: &Operation,
) -> Box<dyn Iterator<Item = &RuntimeValue> + '_> {
    match operation {
        Operation::Fill { value, .. } | Operation::Select { value, .. } => {
            Box::new(std::iter::once(value))
        }
        Operation::Dialog {
            text: Some(value), ..
        } => Box::new(std::iter::once(value)),
        Operation::Evaluate { args, .. } => Box::new(args.iter()),
        Operation::Request {
            url, headers, body, ..
        } => Box::new(
            std::iter::once(url)
                .chain(headers.values())
                .chain(body.iter()),
        ),
        _ => Box::new(std::iter::empty()),
    }
}

pub(crate) fn operation_save_as(operation: &Operation) -> Option<&str> {
    match operation {
        Operation::Evaluate { save_as, .. } | Operation::Request { save_as, .. } => {
            save_as.as_deref()
        }
        _ => None,
    }
}
pub(crate) fn validate_gesture_delta(
    index: usize,
    operation: &str,
    x: i32,
    y: i32,
) -> Result<(), FlowError> {
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

pub(crate) fn validate_screenshot_name(index: usize, name: &str) -> Result<(), FlowError> {
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

pub(crate) fn is_windows_reserved_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            })
        })
}

pub(crate) fn validate_crop(
    index: usize,
    crop: RawCrop,
    viewport: Viewport,
) -> Result<Crop, FlowError> {
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
pub(crate) fn validate_baseline_path(
    index: usize,
    source: &Path,
    value: &str,
) -> Result<PathBuf, FlowError> {
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
pub(crate) fn validate_input_name(name: &str) -> Result<(), FlowError> {
    require_scalar_size("input name", name)?;
    let mut bytes = name.bytes();
    if !matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return invalid(format!("invalid input name {name:?}"));
    }
    Ok(())
}
pub(crate) fn validate_count(
    field: &str,
    index: usize,
    value: usize,
    maximum: usize,
) -> Result<usize, FlowError> {
    if value == 0 || value > maximum {
        return invalid(format!(
            "step {index} {field} must be between 1 and {maximum}",
        ));
    }
    Ok(value)
}
