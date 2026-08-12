# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Dual-licensed under MIT OR Apache-2.0. The CLI and NDJSON session protocol are the supported compatibility surfaces; the Rust library API is internal.
- Encoding-aware secret redaction: diagnostics and protocol output redact the raw secret plus percent-encoded forms (component `%20` and form `+` space encodings, upper- and lowercase hex) and base64 forms (standard and URL-safe alphabets, padded and unpadded), longest-first.

### Changed

- **Breaking:** Declared `secrets` shorter than four characters now fail `playrust check` and flow compile. The error names the secret variable, never its value. Flows that relied on short declared secrets must use longer values or remove those secrets.

### Notes

- Runtime outputs shorter than four characters remain secret in storage and export but are not redacted from diagnostics, to avoid over-redacting common short substrings. Bare string outputs are skipped by value length before registration; JSON quoting does not make a short string register for redaction.
