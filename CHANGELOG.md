# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Encoding-aware secret redaction: diagnostics and protocol output redact the raw secret plus percent-encoded forms (component `%20` and form `+` space encodings, upper- and lowercase hex) and base64 forms (standard and URL-safe alphabets, padded and unpadded), longest-first.
- Pinned, checksum-verified FFmpeg/ffprobe provisioning (BtbN Linux/Windows, evermeet macOS x86_64, martin-riedl macOS arm64). Auto-installs when video is enabled. GPL static builds are invoked as a subprocess, not linked.

### Changed

- Browser and live-network e2e tests self-skip when Chrome, ffmpeg, or `PLAYRUST_LIVE_E2E` is missing, instead of being `#[ignore]`-gated. Set `PLAYRUST_REQUIRE_BROWSER=1` to turn skips into failures (CI browser jobs do this).
- **Breaking:** Declared `secrets` shorter than four characters now fail `playrust check` and flow compile. The error names the secret variable, never its value. Flows that relied on short declared secrets must use longer values or remove those secrets.

### Notes

- Runtime outputs shorter than four characters remain secret in storage and export but are not redacted from diagnostics, to avoid over-redacting common short substrings. Bare string outputs are skipped by value length before registration; JSON quoting does not make a short string register for redaction.
