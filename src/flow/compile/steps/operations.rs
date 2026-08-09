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

use super::control::CompiledWhen;
use super::control::{compile_expression, compile_run_vars, compile_when, compile_while};

pub(crate) fn compile_operation(
    step: RawStep,
    index: usize,
    source: &Path,
    base_url: Option<&Resolved<Url>>,
    viewport: Viewport,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Operation, FlowError> {
    if step.operation_count() != 1 {
        return invalid(format!("step {index} must contain exactly one operation"));
    }

    if let Some(raw) = step.open {
        let (raw_url, raw_settle) = match raw {
            RawOpen::Url(url) => (url, None),
            RawOpen::Detailed(options) => {
                let Some(url) = options.url else {
                    return invalid(format!("step {index} open requires url"));
                };
                (url, options.wait_until)
            }
        };
        let value = interpolate(&format!("step {index} open"), &raw_url, inputs)?;
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
        let settle = compile_settle(raw_settle, index, inputs)?;
        return Ok(Operation::Open {
            url: Resolved::new(url, value.secret || base_secret),
            settle,
        });
    }
    if let Some(raw) = step.click {
        if let Some(point) = raw.point {
            if raw.target.is_some() || raw.position.is_some() {
                return invalid(format!(
                    "step {index} click point cannot be combined with target or position"
                ));
            }
            if point.x >= viewport.width || point.y >= viewport.height {
                return invalid(format!(
                    "step {index} click point ({}, {}) is outside viewport {}x{}",
                    point.x, point.y, viewport.width, viewport.height
                ));
            }
            return Ok(Operation::ClickPoint { point });
        }
        let target = raw.target.ok_or_else(|| {
            FlowError::Invalid(format!(
                "step {index} click requires exactly one of target or point"
            ))
        })?;
        return Ok(Operation::Click {
            target: compile_locator(target, index, inputs)?,
            position: raw.position,
        });
    }
    if let Some(raw) = step.double_click {
        if raw.point.is_some() {
            return invalid(format!("step {index} double_click does not support point"));
        }
        return Ok(Operation::DoubleClick {
            target: compile_locator(
                raw.target.ok_or_else(|| {
                    FlowError::Invalid(format!("step {index} double_click requires target"))
                })?,
                index,
                inputs,
            )?,
            position: raw.position,
        });
    }
    if let Some(raw) = step.fill {
        let value = compile_runtime_value(&format!("step {index} fill.value"), &raw.value, inputs)?;
        if value.outputs.is_empty() {
            require_non_empty(&format!("step {index} fill.value"), value.template.expose())?;
        }
        return Ok(Operation::Fill {
            target: compile_locator(raw.target, index, inputs)?,
            value,
        });
    }
    if let Some(raw) = step.erase {
        return Ok(Operation::Erase {
            target: compile_locator(raw.target, index, inputs)?,
        });
    }
    if let Some(raw) = step.select {
        let value =
            compile_runtime_value(&format!("step {index} select.value"), &raw.value, inputs)?;
        return Ok(Operation::Select {
            target: compile_locator(raw.target, index, inputs)?,
            value,
        });
    }
    if let Some(raw) = step.scroll {
        if raw.x == 0 && raw.y == 0 {
            return invalid(format!("step {index} scroll requires a non-zero x or y"));
        }
        return Ok(Operation::Scroll { x: raw.x, y: raw.y });
    }
    if let Some(raw) = step.scroll_until_visible {
        validate_gesture_delta(index, "scroll_until_visible", raw.x, raw.y)?;
        return Ok(Operation::ScrollUntilVisible {
            target: compile_locator(raw.target, index, inputs)?,
            x: raw.x,
            y: raw.y,
        });
    }
    if let Some(raw) = step.swipe {
        validate_gesture_delta(index, "swipe", raw.x, raw.y)?;
        return Ok(Operation::Swipe {
            target: compile_locator(raw.target, index, inputs)?,
            x: raw.x,
            y: raw.y,
            duration: compile_gesture_duration(
                index,
                "swipe",
                raw.duration.as_deref(),
                DEFAULT_SWIPE_DURATION,
                inputs,
            )?,
        });
    }
    if let Some(raw) = step.long_press {
        return Ok(Operation::LongPress {
            target: compile_locator(raw.target, index, inputs)?,
            duration: compile_gesture_duration(
                index,
                "long_press",
                raw.duration.as_deref(),
                DEFAULT_LONG_PRESS_DURATION,
                inputs,
            )?,
        });
    }
    if let Some(raw) = step.pause {
        let context = format!("step {index} pause");
        let value = interpolate(&context, &raw, inputs)?;
        let duration = parse_duration(&context, value.expose())?;
        if duration > MAX_PAUSE_DURATION {
            return invalid(format!(
                "{context} must not exceed {} seconds",
                MAX_PAUSE_DURATION.as_secs()
            ));
        }
        return Ok(Operation::Pause { duration });
    }
    if let Some(raw) = step.wait_until_visible {
        return Ok(Operation::WaitUntilVisible {
            target: compile_locator(raw.target, index, inputs)?,
        });
    }
    if let Some(raw) = step.wait_until_stable {
        return Ok(Operation::WaitUntilStable {
            target: compile_locator(raw.target, index, inputs)?,
        });
    }
    if step.back.is_some() {
        return Ok(Operation::Back);
    }
    if let Some(page) = step.switch_page {
        return Ok(Operation::SwitchPage(match page {
            RawPageSwitch::Location(PageLocation::Popup) => PageSwitch::Popup,
            RawPageSwitch::Location(PageLocation::Opener) => PageSwitch::Opener,
            RawPageSwitch::Selector(raw) => match (raw.name, raw.url) {
                (Some(name), None) => {
                    let name =
                        interpolate(&format!("step {index} switch_page.name"), &name, inputs)?;
                    require_non_empty(&format!("step {index} switch_page.name"), name.expose())?;
                    PageSwitch::Name(name)
                }
                (None, Some(raw)) => {
                    let value =
                        interpolate(&format!("step {index} switch_page.url"), &raw, inputs)?;
                    require_non_empty(&format!("step {index} switch_page.url"), value.expose())?;
                    let (url, base_secret) = match Url::parse(value.expose()) {
                        Ok(url) => (
                            validate_http_url(&format!("step {index} switch_page.url"), url)?,
                            false,
                        ),
                        Err(url::ParseError::RelativeUrlWithoutBase) => {
                            let base = base_url.ok_or_else(|| {
                                FlowError::Invalid(format!(
                                    "step {index} has a relative switch_page URL but base_url is not set"
                                ))
                            })?;
                            let url = base.expose().join(value.expose()).map_err(|_| {
                                FlowError::Invalid(format!(
                                    "step {index} switch_page.url is not a valid URL"
                                ))
                            })?;
                            (
                                validate_http_url(&format!("step {index} switch_page.url"), url)?,
                                base.secret,
                            )
                        }
                        Err(_) => {
                            return invalid(format!(
                                "step {index} switch_page.url is not a valid URL"
                            ));
                        }
                    };
                    PageSwitch::Url(Resolved::new(url, value.secret || base_secret))
                }
                _ => {
                    return invalid(format!(
                        "step {index} switch_page must contain exactly one of name or url"
                    ));
                }
            },
        }));
    }
    if let Some(frame) = step.switch_frame {
        return Ok(Operation::SwitchFrame(match frame {
            RawFrameSwitch::Target(raw) => {
                FrameSwitch::Target(compile_locator(raw.target, index, inputs)?)
            }
            RawFrameSwitch::Location(FrameLocation::Main) => FrameSwitch::Main,
            RawFrameSwitch::Location(FrameLocation::Parent) => FrameSwitch::Parent,
        }));
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
    if let Some(control) = step.recording {
        return Ok(Operation::Recording(control));
    }
    if let Some(raw) = step.dialog {
        if raw.text.is_some() && raw.action != NativeDialogResponse::Accept {
            return invalid(format!(
                "step {index} dialog text is only valid with action accept"
            ));
        }
        return Ok(Operation::Dialog {
            action: raw.action,
            text: raw
                .text
                .as_deref()
                .map(|text| {
                    compile_runtime_value(&format!("step {index} dialog.text"), text, inputs)
                })
                .transpose()?,
        });
    }
    if let Some(target) = step.clear {
        return Ok(Operation::Clear(target));
    }
    if let Some(raw) = step.evaluate {
        require_scalar_size(&format!("step {index} evaluate.script"), &raw.script)?;
        require_non_empty(&format!("step {index} evaluate.script"), &raw.script)?;
        let save_as = compile_save_as(index, raw.save_as, inputs)?;
        let args = raw
            .args
            .iter()
            .enumerate()
            .map(|(offset, value)| {
                compile_runtime_value(
                    &format!("step {index} evaluate.args[{offset}]"),
                    value,
                    inputs,
                )
            })
            .collect::<Result<_, _>>()?;
        return Ok(Operation::Evaluate {
            script: raw.script,
            args,
            save_as,
        });
    }
    if let Some(raw) = step.request {
        if raw.headers.len() > MAX_HTTP_HEADERS {
            return invalid(format!(
                "step {index} request.headers must not exceed {MAX_HTTP_HEADERS} entries"
            ));
        }
        let method = raw.method.to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ) {
            return invalid(format!("step {index} request.method is unsupported"));
        }
        if !(100..=599).contains(&raw.expected_status) {
            return invalid(format!(
                "step {index} request.expected_status must be between 100 and 599"
            ));
        }
        let url = compile_runtime_value(&format!("step {index} request.url"), &raw.url, inputs)?;
        if url.outputs.is_empty() {
            parse_absolute_url(&format!("step {index} request.url"), url.template.clone())?;
        }
        let mut headers = BTreeMap::new();
        for (name, value) in raw.headers {
            require_scalar_size(&format!("step {index} request header name"), &name)?;
            require_non_empty(&format!("step {index} request header name"), &name)?;
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                FlowError::Invalid(format!("step {index} request header name is invalid"))
            })?;
            headers.insert(
                name,
                compile_runtime_value(&format!("step {index} request header"), &value, inputs)?,
            );
        }
        let body = raw
            .body
            .as_deref()
            .map(|value| {
                compile_runtime_value(&format!("step {index} request.body"), value, inputs)
            })
            .transpose()?;
        return Ok(Operation::Request {
            method,
            url,
            headers,
            body,
            expected_status: raw.expected_status,
            save_as: compile_save_as(index, raw.save_as, inputs)?,
        });
    }
    Ok(Operation::Assert(compile_assertion(
        step.assertion.expect("operation count checked"),
        index,
        source,
        viewport,
        inputs,
    )?))
}
pub(crate) fn compile_gesture_duration(
    index: usize,
    operation: &str,
    raw: Option<&str>,
    default: Duration,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Duration, FlowError> {
    let context = format!("step {index} {operation}.duration");
    let duration = raw
        .map(|value| interpolate(&context, value, inputs))
        .transpose()?
        .map(|value| parse_duration(&context, value.expose()))
        .transpose()?
        .unwrap_or(default);
    if duration > MAX_GESTURE_DURATION {
        return invalid(format!(
            "{context} must not exceed {} seconds",
            MAX_GESTURE_DURATION.as_secs()
        ));
    }
    Ok(duration)
}
pub(crate) fn compile_assertion(
    raw: RawAssertion,
    index: usize,
    source: &Path,
    viewport: Viewport,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Assertion, FlowError> {
    let assertion_count = [
        raw.visible.is_some(),
        raw.hidden.is_some(),
        raw.text.is_some(),
        raw.url.is_some(),
        raw.screenshot.is_some(),
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

    if let Some(screenshot) = raw.screenshot {
        let baseline = interpolate_non_secret(
            &format!("step {index} screenshot assertion baseline"),
            &screenshot.baseline,
            inputs,
        )?;
        let baseline = validate_baseline_path(index, source, &baseline)?;
        let crop = screenshot
            .crop
            .map(|crop| validate_crop(index, crop, viewport))
            .transpose()?;
        let (width, height) = crop
            .map(|crop| (crop.width, crop.height))
            .unwrap_or((viewport.width, viewport.height));
        crate::visual::validate_dimensions(width, height)
            .map_err(|message| FlowError::Invalid(format!("step {index} {message}")))?;
        let max_changed_ratio = screenshot.max_changed_ratio.unwrap_or(0.0);
        if !max_changed_ratio.is_finite() || !(0.0..=1.0).contains(&max_changed_ratio) {
            return invalid(format!(
                "step {index} screenshot assertion max_changed_ratio must be between 0 and 1"
            ));
        }
        return Ok(Assertion::Screenshot(VisualExpectation {
            baseline,
            crop,
            channel_tolerance: screenshot.channel_tolerance.unwrap_or(0),
            max_changed_ratio,
        }));
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
pub(crate) fn compile_locator(
    raw: RawLocator,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Locator, FlowError> {
    compile_locator_at(raw, index, inputs, 0)
}

pub(crate) fn compile_settle(
    raw: Option<RawSettle>,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
) -> Result<Option<SettleCondition>, FlowError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match (raw.visible, raw.stable) {
        (Some(target), None) => Ok(Some(SettleCondition::Visible(compile_locator(
            target, index, inputs,
        )?))),
        (None, Some(target)) => Ok(Some(SettleCondition::Stable(compile_locator(
            target, index, inputs,
        )?))),
        (Some(_), Some(_)) => invalid(format!(
            "step {index} open wait_until accepts exactly one of visible or stable"
        )),
        (None, None) => invalid(format!(
            "step {index} open wait_until requires exactly one of visible or stable"
        )),
    }
}

pub(crate) fn compile_locator_at(
    raw: RawLocator,
    index: usize,
    inputs: &BTreeMap<String, Resolved<String>>,
    depth: usize,
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
    let strategy = if let Some(value) = raw.css {
        LocatorStrategy::Css(interpolate_non_empty(&context, &value, inputs)?)
    } else if let Some(value) = raw.test_id {
        LocatorStrategy::TestId(interpolate_non_empty(&context, &value, inputs)?)
    } else if let Some(text) = raw.text {
        let (value, match_kind) = match text {
            RawTextLocator::Scalar(value) => (value, TextMatch::Exact),
            RawTextLocator::Detailed(options) => (
                options.value,
                options.match_kind.unwrap_or(TextMatch::Exact),
            ),
        };
        LocatorStrategy::Text {
            value: interpolate_non_empty(&context, &value, inputs)?,
            match_kind,
        }
    } else if let Some(value) = raw.label {
        LocatorStrategy::Label(interpolate_non_empty(&context, &value, inputs)?)
    } else {
        let role = raw.role.expect("strategy count checked");
        LocatorStrategy::Role {
            value: interpolate_non_empty(&context, &role.value, inputs)?,
            name: role
                .name
                .as_deref()
                .map(|value| interpolate_non_empty(&context, value, inputs))
                .transpose()?,
        }
    };
    let mut relations = Vec::new();
    for (kind, relation) in [
        (RelationKind::Within, raw.within),
        (RelationKind::ChildOf, raw.child_of),
        (RelationKind::Has, raw.has),
        (RelationKind::Above, raw.above),
        (RelationKind::Below, raw.below),
        (RelationKind::Left, raw.left),
        (RelationKind::Right, raw.right),
    ] {
        if let Some(relation) = relation {
            if depth == MAX_LOCATOR_DEPTH {
                return invalid(format!(
                    "step {index} locator exceeds maximum relation depth {MAX_LOCATOR_DEPTH}"
                ));
            }
            relations.push(LocatorRelation {
                kind,
                locator: Box::new(compile_locator_at(*relation, index, inputs, depth + 1)?),
            });
        }
    }
    Ok(Locator {
        strategy,
        index: raw.index,
        checked: raw.checked,
        selected: raw.selected,
        focused: raw.focused,
        enabled: raw.enabled,
        relations,
    })
}
