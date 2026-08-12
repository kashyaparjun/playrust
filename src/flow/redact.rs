#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fmt;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};

pub const REDACTED: &str = "[REDACTED]";
/// Minimum length a declared or runtime secret must reach before it is
/// registered with the redactor. Shorter values are rejected at compile time
/// (declared secrets) or silently skipped (runtime outputs) to avoid
/// over-redacting common short substrings.
pub const MIN_SECRET_LEN: usize = 4;

pub(crate) fn meets_min_secret_len(value: &str) -> bool {
    value.chars().count() >= MIN_SECRET_LEN
}

#[derive(Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    pub(crate) value: T,
    pub(crate) secret: bool,
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

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Redactor {
    variants: Vec<String>,
}

impl Redactor {
    pub fn redact(&self, text: &str) -> String {
        self.variants.iter().fold(text.to_owned(), |text, variant| {
            text.replace(variant, REDACTED)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    pub(crate) fn extend(&mut self, other: &Self) {
        self.variants.extend(other.variants.iter().cloned());
        self.sort_and_dedupe();
    }

    pub(crate) fn add_secret(&mut self, secret: String) {
        if meets_min_secret_len(&secret) {
            self.register_variants(&secret);
            self.sort_and_dedupe();
        }
    }

    /// Register a JSON-serialized runtime output for redaction.
    /// When `bare_string` is `Some`, length is measured on that bare string so
    /// JSON quotes do not promote a short value past `MIN_SECRET_LEN`.
    pub(crate) fn add_serialized_secret(&mut self, serialized: String, bare_string: Option<&str>) {
        match bare_string {
            Some(bare) if !meets_min_secret_len(bare) => {}
            _ => self.add_secret(serialized),
        }
    }

    pub(crate) fn from_inputs(inputs: &BTreeMap<String, Resolved<String>>) -> Self {
        let mut redactor = Self::default();
        for value in inputs.values().filter(|value| value.secret) {
            redactor.add_secret(value.value.clone());
        }
        redactor
    }

    fn register_variants(&mut self, secret: &str) {
        self.variants.push(secret.to_owned());
        // Percent-encoding hex digits are case-insensitive in URLs, but
        // str::replace is not — register both cases for each space form.
        for mode in PERCENT_ENCODINGS {
            self.variants.push(mode.encode(secret));
        }
        self.variants.push(STANDARD.encode(secret.as_bytes()));
        self.variants
            .push(STANDARD_NO_PAD.encode(secret.as_bytes()));
        self.variants.push(URL_SAFE.encode(secret.as_bytes()));
        self.variants
            .push(URL_SAFE_NO_PAD.encode(secret.as_bytes()));
    }

    fn sort_and_dedupe(&mut self) {
        self.variants
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        self.variants.dedup();
    }
}

#[derive(Clone, Copy)]
enum PercentEncoding {
    ComponentUpper,
    ComponentLower,
    FormUpper,
    FormLower,
}

const PERCENT_ENCODINGS: [PercentEncoding; 4] = [
    PercentEncoding::ComponentUpper,
    PercentEncoding::ComponentLower,
    PercentEncoding::FormUpper,
    PercentEncoding::FormLower,
];

impl PercentEncoding {
    fn encode(self, value: &str) -> String {
        let form = matches!(self, Self::FormUpper | Self::FormLower);
        let lower_hex = matches!(self, Self::ComponentLower | Self::FormLower);
        let mut encoded = String::with_capacity(value.len());
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                encoded.push(byte as char);
            } else if form && byte == b' ' {
                encoded.push('+');
            } else if lower_hex {
                encoded.push_str(&format!("%{byte:02x}"));
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        encoded
    }
}

impl fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Redactor")
            .field("variant_count", &self.variants.len())
            .finish()
    }
}
