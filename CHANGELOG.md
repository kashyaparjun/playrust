# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.3.0] - 2026-10-19

### Added

- `playrust session --live-frame PATH` keeps the newest recording frame at `PATH` as a JPEG, replaced atomically at most ten times a second, for a person watching the session.
- The session `input` command sends a person's click, move, wheel, text or key event at viewport coordinates. Inputs invalidate refs, are journaled as `manual_input` without typed text, and are counted as `manual_inputs` by `export`, since a replay cannot repeat them.
- `report.json` (`report_version` 4) lists each flow's top-level steps under `steps` with a redacted `label`, `started_ms` measured from the recording's first frame (or flow start without a recording), `duration_ms`, and `status`, so players can mark and seek steps in `recording.mp4`.
- Encoding-aware secret redaction: diagnostics and protocol output redact the raw secret plus percent-encoded forms (component `%20` and form `+` space encodings, upper- and lowercase hex) and base64 forms (standard and URL-safe alphabets, padded and unpadded), longest-first.

### Changed

- **Breaking:** Declared `secrets` shorter than four characters now fail `playrust check` and flow compile. The error names the secret variable, never its value. Flows that relied on short declared secrets must use longer values or remove those secrets.

### Notes

- Runtime outputs shorter than four characters remain secret in storage and export but are not redacted from diagnostics, to avoid over-redacting common short substrings. Bare string outputs are skipped by value length before registration; JSON quoting does not make a short string register for redaction.
