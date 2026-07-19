---
name: playrust-agent
description: Drives one Playrust NDJSON interactive browser session and exports its audited replay bundle. Use when an agent must inspect, act on, verify, or record a browser workflow with Playrust.
---

# Playrust Agent

Use one long-running `playrust session --protocol ndjson` process. Keep the transport tool-agnostic: write one JSON command per stdin line, parse exactly one stdout response for each request, and keep diagnostics on stderr.

## Start

```sh
playrust session --protocol ndjson --video on --dialog-policy explicit --viewport 1440x900
```

The browser context and continuous recorder start immediately. Playrust records H.264 directly to `recording.mp4`; do not transcode it or manage FFmpeg lifecycle yourself. Add `--headed`, `--browser`, `--timeout`, `--ffmpeg-path`, or `--artifacts` only when the task requires them.

## Loop

```json
{"id":1,"command":"act","action":{"open":{"url":"https://example.com"}}}
{"id":2,"command":"snapshot","screenshot":"viewport","accessibility":true}
{"id":3,"command":"act","action":{"click":{"ref":"e12"}}}
{"id":4,"command":"scroll","y":700}
{"id":5,"command":"snapshot","screenshot":"viewport","accessibility":true}
{"id":6,"command":"dialog","action":"accept"}
{"id":7,"command":"export","name":"example-run"}
{"id":8,"command":"close"}
```

1. Open the target with `act.open`.
2. Snapshot before every ref-based action.
3. Choose only refs present in the latest snapshot. Never guess refs.
4. After a successful mutation, navigation, page/frame switch, or scroll, snapshot again.
5. Use `act` for one action and `scroll` for viewport movement.
6. If `pending_dialog` appears or an action returns `dialog_pending`, use `dialog` to accept or dismiss it, then snapshot again.
7. Verify the goal with structured assertions and/or a final snapshot.
8. Send `export` before `close`; close finalizes the recording into the bundle.
9. Run `playrust check <bundle>/replay.yaml`. Replay fresh when reproducibility matters.

Large pages return a bounded snapshot with `truncation.truncated: true`. Use the refs that were returned, then scroll or act and snapshot again; do not treat truncation as a failure or fall back to raw `inspect`.

For credentials and other available environment values, send `{"env":"NAME"}` as a `fill` or `select` value. Do not put secret literals in commands when an environment variable is available.

## Recovery

- On `stale_reference` or `unknown_reference`, take a new snapshot and reason again. Never retry an old ref.
- On `dialog_pending`, handle the dialog explicitly; snapshot, export, and close remain available.
- On an ordinary recoverable action/wait/assertion failure, snapshot current state before deciding what to do next.
- On recorder warnings, continue browser work; Playrust preserves and reports partial recording state.
- On a fatal browser, transport, or artifact error, stop issuing actions and retain the reported artifacts.

## Recording

The canonical video is the bundle's `recording.mp4`, encoded as H.264 at the configured viewport. Do not convert it after capture. When media validation matters, use `ffprobe` to check codec and dimensions; extract a frame only for optional visual inspection.

## Export

Playrust's `export` command is the only source of canonical replay YAML. Never synthesize YAML from memory or reconstruct it from snapshots. Export excludes failed exploration and snapshots, lifts environment-backed values into YAML secrets, and writes `replay.yaml`, `session.ndjson`, `report.json`, the recording or documented partial result, and referenced screenshots.

`submit` and raw `inspect` remain compatibility commands. Prefer `act`/`snapshot` for interactive work; use `submit` only for an existing inline YAML flow and `inspect` only for a client that requires its deprecated raw response.
