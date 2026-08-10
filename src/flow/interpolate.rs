use super::compiled::{RuntimeValue, RuntimeValuePart};
use super::validate::validate_input_name;
use super::{
    FlowError, MAX_SCALAR_BYTES, Resolved, invalid, require_non_empty, require_scalar_size,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn interpolate_non_empty(
    context: &str,
    source: &str,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Resolved<String>, FlowError> {
    let value = interpolate(context, source, inputs)?;
    require_non_empty(context, value.expose())?;
    Ok(value)
}

pub(crate) fn compile_save_as(
    index: usize,
    save_as: Option<String>,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Option<String>, FlowError> {
    let Some(save_as) = save_as else {
        return Ok(None);
    };
    require_scalar_size(&format!("step {index} save_as"), &save_as)?;
    validate_input_name(&save_as)
        .map_err(|_| FlowError::Invalid(format!("step {index} save_as is not a valid name")))?;
    if inputs.contains_key(&save_as) {
        return invalid(format!(
            "step {index} save_as conflicts with an input named {save_as:?}"
        ));
    }
    Ok(Some(save_as))
}

pub(crate) fn compile_runtime_value(
    context: &str,
    source: &str,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<RuntimeValue, FlowError> {
    require_scalar_size(context, source)?;
    let mut output = String::with_capacity(source.len());
    let mut outputs = BTreeSet::new();
    let mut parts = Vec::new();
    let mut secret = false;
    let mut remaining = source;
    while let Some(start) = remaining.find("${") {
        let literal = &remaining[..start];
        push_interpolated(context, &mut output, literal)?;
        if !literal.is_empty() {
            parts.push(RuntimeValuePart::Literal(Resolved::new(
                literal.to_owned(),
                false,
            )));
        }
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .ok_or_else(|| FlowError::Invalid(format!("{context} has an unterminated variable")))?;
        let name = &after_start[..end];
        validate_input_name(name).map_err(|_| {
            FlowError::Invalid(format!("{context} contains invalid variable reference"))
        })?;
        if let Some(value) = inputs.get(name) {
            push_interpolated(context, &mut output, value.expose())?;
            secret |= value.secret;
            parts.push(RuntimeValuePart::Literal(value.clone()));
        } else {
            outputs.insert(name.to_owned());
            push_interpolated(context, &mut output, &format!("${{{name}}}"))?;
            parts.push(RuntimeValuePart::Output(name.to_owned()));
        }
        remaining = &after_start[end + 1..];
    }
    push_interpolated(context, &mut output, remaining)?;
    if !remaining.is_empty() {
        parts.push(RuntimeValuePart::Literal(Resolved::new(
            remaining.to_owned(),
            false,
        )));
    }
    Ok(RuntimeValue {
        template: Resolved::new(output, secret),
        parts,
        outputs,
    })
}

pub(crate) fn interpolate_non_secret(
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

pub(crate) fn interpolate(
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

pub(crate) fn push_interpolated(
    context: &str,
    output: &mut String,
    value: &str,
) -> Result<(), FlowError> {
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
