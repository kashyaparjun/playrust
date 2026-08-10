use super::{FlowError, MAX_TIMEOUT, invalid};
use std::time::Duration;

pub fn parse_duration(context: &str, value: &str) -> Result<Duration, FlowError> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u128)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000_u128)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000_u128)
    } else {
        return invalid(format!("{context} must use ms, s, or m"));
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return invalid(format!("{context} is not a valid duration"));
    }
    let number = number
        .parse::<u128>()
        .map_err(|_| FlowError::Invalid(format!("{context} is not a valid duration")))?;
    let milliseconds = number
        .checked_mul(multiplier)
        .filter(|value| *value > 0)
        .ok_or_else(|| FlowError::Invalid(format!("{context} is outside the supported range")))?;
    if milliseconds > MAX_TIMEOUT.as_millis() {
        return invalid(format!(
            "{context} must not exceed {} seconds",
            MAX_TIMEOUT.as_secs()
        ));
    }
    Ok(Duration::from_millis(milliseconds as u64))
}
