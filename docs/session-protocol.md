# Session Protocol

`playrust session --protocol ndjson` is the version 1 local interactive-session protocol. This document is the current wire contract for agents and other local clients.

## Startup

```text
playrust session --protocol ndjson [--headed] [--browser PATH]
                    [--viewport WIDTHxHEIGHT] [--timeout DURATION]
                    [--video on|off]
                    [--dialog-policy explicit|accept|dismiss]
                    [--ffmpeg-path PATH] [--artifacts DIR]
```

The command eagerly opens one isolated browser context on `about:blank`. `--viewport` defaults to `1280x720`, `--timeout` defaults to `10s`, `--video` defaults to `on`, and `--dialog-policy` defaults to `explicit`. The viewport, timeout, video mode, and dialog policy are fixed for the session and recorded in its journal.

Video `on` starts one continuous recording when the context opens and finalizes it at close. Video `off` disables recording. Session mode does not use `retain-on-failure`; an interactive recording is evidence for the entire session. A recorder failure is reported as degraded status with warnings and any partial path, but does not make later browser commands unusable.

Dialog policy `explicit` requires a `dialog` command. `accept` and `dismiss` automatically handle every observed native dialog; automatic prompt acceptance uses an empty string. Automatic handling is journaled.

## Transport

The client writes one UTF-8 JSON object per stdin line. Playrust writes and flushes one compact JSON response per command, in request order. Diagnostics are written only to stderr.

One command envelope may contain at most 1,048,576 bytes before its `LF`. The limit includes a `CR` in `CRLF`. An oversized line is drained through its next `LF` and receives recoverable `envelope_too_large` with `id: null`. Invalid UTF-8, malformed JSON, and invalid command shapes receive recoverable `invalid_command`. If the transport remains open, the next line is processed normally. A stdin read failure or stdout write failure is fatal and may prevent a response.

EOF closes an idle session without a response. EOF during an active compatibility submission cancels and drains it, writes its `cancelled` response while stdout remains available, and terminates with exit 130 unless an infrastructure failure occurs.

## Observe-Act Loop

Every command is a JSON object with an `id` and `command`. IDs may be any JSON value and are echoed unchanged; clients should make them unique.

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

Take a snapshot before each ref-based action. After scrolling, mutating, navigating, or switching page/frame, take another snapshot and reason only from its refs. Verify the goal with explicit assertions and/or a final snapshot, export, then close.

## Snapshot

```json
{"id":10,"command":"snapshot","screenshot":"none","accessibility":true}
{"id":11,"command":"snapshot","screenshot":"viewport","accessibility":true,"since":10}
{"id":12,"command":"snapshot","screenshot":"full_page","accessibility":false}
```

`screenshot` is `none`, `viewport`, or `full_page` and defaults to `none`. `accessibility` controls compact semantic-tree capture. `since`, when present, requests a semantic diff against the retained adjacent snapshot revision.

A successful result has this shape; optional data may be absent when not requested:

```json
{
  "snapshot_revision": 11,
  "url": "https://example.com/",
  "title": "Example",
  "pages": [],
  "active_frame": null,
  "viewport": {"width": 1280, "height": 720},
  "scroll": {"x": 0, "y": 0, "document_width": 1280, "document_height": 2400},
  "pending_dialog": null,
  "capture_status": "complete",
  "screenshot": "playrust-artifacts/session-.../snapshot-....png",
  "tree": [],
  "elements": [
    {
      "ref": "e12",
      "role": "button",
      "name": "Continue",
      "value": null,
      "states": {"focused": false, "enabled": true, "editable": false},
      "visible": true,
      "bounds": {"x": 20, "y": 100, "width": 120, "height": 36}
    }
  ],
  "truncation": {"truncated": false},
  "diff": {"added": [], "changed": [], "removed": []}
}
```

Element states include relevant `checked`, `selected`, `expanded`, `pressed`, `focused`, `enabled`, and `editable` values. Bounds are viewport-relative CSS pixels. Snapshot output is bounded by semantic depth, node count, and text length; `truncation` says when data was omitted. Screenshot values are artifact paths, never base64.

When a native dialog blocks page capture, snapshot returns promptly with `pending_dialog` and `capture_status: "blocked_by_dialog"`; newly captured title, tree, elements, or screenshot may be absent.

### Reference lifetime

References are capabilities for the latest observation, not selectors. They are session-unique monotonically increasing IDs (`e1`, `e2`, ...), scoped to one snapshot revision, and never reused. Only the latest snapshot's ref table is active.

A ref becomes stale after any newer snapshot, successful mutating command, navigation, page/frame switch, or node detachment. An unknown ID returns `unknown_reference`. A known invalidated, detached, or page-mismatched ID returns `stale_reference` with the latest snapshot revision. Clients must snapshot again; they must not retry with a guessed or stale ref.

Before dispatch, Playrust revalidates the durable locator stored with the ref for uniqueness and actionability. Replay locators use the strongest unique choice available at observation time: test ID, accessible role/name, label, stable CSS, relational locator, then index. Backend node IDs are metadata, not replay locators.

## Act

`act` executes exactly one supported action through the same browser semantics, timeout, redaction, frame routing, and failure categories as YAML. Ref-targeted actions use `ref` instead of a YAML `target`.

```json
{"id":20,"command":"act","action":{"open":{"url":"https://example.com/login"}}}
{"id":21,"command":"act","action":{"click":{"ref":"e12"}}}
{"id":22,"command":"act","action":{"double_click":{"ref":"e13"}}}
{"id":23,"command":"act","action":{"fill":{"ref":"e14","value":"alice"}}}
{"id":24,"command":"act","action":{"fill":{"ref":"e15","value":{"env":"TEST_PASSWORD"}}}}
{"id":25,"command":"act","action":{"erase":{"ref":"e14"}}}
{"id":26,"command":"act","action":{"select":{"ref":"e16","value":{"env":"TEST_REGION"}}}}
{"id":27,"command":"act","action":{"press":{"ref":"e14","key":"Enter","modifiers":[]}}}
{"id":28,"command":"act","action":{"back":{}}}
{"id":29,"command":"act","action":{"switch_page":{"name":"checkout"}}}
{"id":30,"command":"act","action":{"switch_frame":{"ref":"e20"}}}
{"id":31,"command":"act","action":{"switch_frame":"parent"}}
{"id":32,"command":"act","action":{"wait_until_visible":{"ref":"e21"}}}
{"id":33,"command":"act","action":{"wait_until_stable":{"ref":"e21"}}}
{"id":34,"command":"act","action":{"pause":"1500ms"}}
{"id":35,"command":"act","action":{"assert":{"visible":{"ref":"e21"}}}}
```

The supported action names are `open`, `click`, `double_click`, `fill`, `erase`, `select`, `press`, `back`, `switch_page`, `switch_frame`, `wait_until_visible`, `wait_until_stable`, `pause`, and structured `assert`. Page/frame selector variants and assertion predicates follow their YAML V1 shapes, replacing an element target with `ref`. `pause` accepts the same positive duration syntax and `60s` maximum as YAML, keeps a continuous session recording running while it waits, and is preserved by export. `act` deliberately excludes arbitrary evaluation, HTTP requests, storage clearing, recording controls, and scrolling.

`fill.value` and `select.value` accept a literal string or `{"env":"NAME"}`. Playrust resolves environment-backed values internally, marks them secret, and records/exports only the environment-variable name. Use an environment value whenever one is available for credentials.

A successful result reports the action name, durable locator summary when applicable, current URL/title, pending dialog, created outputs/artifacts, and elapsed time:

```json
{"action":"click","locator":{"role":{"value":"button","name":"Continue"}},"url":"https://example.com/next","title":"Next","pending_dialog":null,"outputs":[],"artifacts":[],"elapsed_ms":84}
```

Mutating actions are dispatched once and are not automatically retried. Waits and assertions retain their bounded polling behavior.

## Scroll

```json
{"id":40,"command":"scroll","y":700}
{"id":41,"command":"scroll","x":-200,"y":0}
```

`x` defaults to `0`; at least one axis must be non-zero. Success returns the resulting scroll position and invalidates current refs:

```json
{"scroll":{"x":0,"y":700,"document_width":1280,"document_height":2400}}
```

Snapshot again before the next ref-based action.

## Dialogs

Pending native `alert`, `confirm`, `prompt`, and `beforeunload` dialogs are protocol state, not automation failures. Metadata includes the dialog type, redacted message, optional default prompt, URL when available, and opening revision/timestamp.

```json
{"id":50,"command":"dialog","action":"accept"}
{"id":51,"command":"dialog","action":"accept","text":"prompt response"}
{"id":52,"command":"dialog","action":"dismiss"}
```

`text` is valid only when accepting a prompt. Invalid combinations are rejected without changing the dialog. Success reports the handled dialog and response. An action that opens a dialog succeeds and includes it as `pending_dialog` in that action's result.

Under the default explicit policy, `act`, `scroll`, and `submit` return recoverable `dialog_pending` until the dialog is handled. `snapshot`, `dialog`, `export`, and `close` remain available. Close attempts to dismiss a pending dialog before browser and recording shutdown.

## Export And Close

```json
{"id":60,"command":"export","name":"checkout-run"}
{"id":61,"command":"close"}
```

`export` validates its safe bundle name, derives replay steps from the successful journal, validates the generated YAML with Playrust's compiler, writes or updates `<artifacts>/<name>/`, and registers that bundle as the close destination. It is nonterminal and returns bundle paths with `recording_pending: true` while the session remains open:

```json
{"name":"checkout-run","bundle":"playrust-artifacts/checkout-run","replay":"playrust-artifacts/checkout-run/replay.yaml","journal":"playrust-artifacts/checkout-run/session.ndjson","report":"playrust-artifacts/checkout-run/report.json","screenshots":[],"recording_pending":true}
```

Always export before close. `close` finalizes recording and the report into the registered bundle and returns final recording status, path, and warnings:

```json
{"closed":true,"bundle":"playrust-artifacts/checkout-run","recording":{"status":"complete","path":"playrust-artifacts/checkout-run/recording.mp4","warnings":[]}}
```

If no export was requested, close finalizes into `<artifacts>/session-<id>/`. Finalization is idempotent. A recording failure preserves a documented partial result such as `recording.partial.mp4`; it does not erase the successful browser trace.

An exported bundle contains:

```text
<artifacts>/<name>/
  replay.yaml
  session.ndjson
  report.json
  recording.mp4               # or documented partial-recording result
  <referenced screenshots>
```

`session.ndjson` is an append-only, sequence-numbered, redacted audit journal covering settings, commands and outcomes, durable locators, navigations, dialogs, waits/assertions, pauses, variable/secret metadata, warnings, and artifact references. `replay.yaml` contains only successful explicit opens/actions, necessary scrolls, explicit waits and pauses, dialog responses, and assertions. Batch YAML handles those responses with `dialog: { action: accept }`, `dialog: { action: dismiss }`, or an accepted prompt with `text`. Snapshots, failed exploratory actions, duplicate observed navigations, recorder internals, and protocol bookkeeping are excluded.

Environment-backed values become deterministic YAML secret entries. Plaintext secret values do not appear in protocol responses, the journal, reports, or replay YAML. Diagnostic and protocol redaction matches the raw secret and its percent-encoded forms (component `%20` and form `+` space encodings) plus base64 forms (standard and URL-safe alphabets, padded and unpadded). Declared secrets must be at least four characters or compile/`check` rejects them by name (never by value); runtime outputs shorter than four characters remain secret in storage and export but are not redacted from diagnostics. Validate the canonical flow with `playrust check <bundle>/replay.yaml`; optionally run it in a fresh browser with the same environment variables.

## Compatibility Commands

Protocol v1 retains the original commands:

```json
{"id":"s1","command":"submit","flow":"version: 1\nname: example\nsteps: [{ open: https://example.com }]\n","variables":{"region":"west"}}
{"id":"i1","command":"inspect","accessibility":true,"screenshot":true}
{"id":"o1","command":"output","name":"saved_value"}
{"id":"x1","command":"cancel"}
```

`submit` accepts one complete inline YAML V1 flow. Its viewport and geolocation must match the established session settings. Browser state, active page/frame, JavaScript state, and runtime outputs persist, but a submission does not start or stop the session's continuous recorder. Filesystem subflows and filesystem screenshot baselines remain unsupported in inline submissions.

`inspect` is deprecated in favor of `snapshot`, but retains its existing request and response shape through protocol v1: `{url,title,pages,active_frame?,accessibility?,screenshot?}`. Its screenshot is an artifact path. Raw accessibility remains bounded to depth 8, 500 nodes, and 256 KiB.

`output.name` returns `{name,value}` for a persisted runtime output. `cancel` is valid only while a compatibility submission is active. While a submission runs, other ordering and cancellation rules remain compatible: `cancel` and `close` are accepted and other commands return `busy`.

## Responses And Errors

Every response has this envelope:

```json
{"id":1,"ok":true,"session_id":"stable-id","revision":1,"result":{}}
{"id":1,"ok":false,"session_id":"stable-id","revision":1,"error":{"code":"validation","message":"...","details":{}}}
```

`session_id` is stable for the process. Envelope `revision` starts at 1 and increases by exactly one in response order. Snapshot revisions are a separate observation counter. Exactly one of `result` or `error` is present; `error.details` is optional.

Stable protocol errors include:

| Code | Meaning | Session remains usable |
| --- | --- | --- |
| `invalid_command` | Invalid UTF-8, JSON, field, or command shape. | Yes |
| `envelope_too_large` | Command exceeds 1 MiB. | Yes |
| `validation` | Inline flow, variable, action, or value is invalid. | Yes |
| `settings_conflict` | Submission viewport/geolocation conflicts with session settings. | Yes |
| `busy` | Command is not allowed during an active submission. | Yes |
| `not_active` | `cancel` arrived while no submission was active. | Yes |
| `not_started` | Browser session handle is unavailable. | Yes |
| `output_not_found` | Named runtime output is unavailable. | Yes |
| `dialog_pending` | A native dialog must be handled before mutation. | Yes |
| `unknown_reference` | Ref was never issued by this session. | Yes |
| `stale_reference` | Ref no longer belongs to the current observation. | Yes |
| `snapshot_unavailable` | Requested `since` baseline is unavailable or non-adjacent. | Yes |
| `export_invalid` | Journal cannot be compiled into canonical replay YAML. | Yes |
| `action_failed` | Recoverable interactive action, wait, assertion, or navigation failure. | Yes |
| `submission_failed` | Recoverable compatibility submission failure. | Yes |
| `cancelled` | Submission drained after cancellation. | No |
| `browser` | Browser launch, crash, connection, protocol, or disposal failed. | No |
| `artifacts` | Journal or artifact persistence failed. | No |

Ordinary action, wait, assertion, navigation, stale-ref, and export validation failures are recoverable command errors. Browser crash/CDP connection loss, transport failure, and journal/artifact failure are fatal. Recorder degradation is reported in recording status and warnings rather than making later browser commands fatal. `not_started` is defense-in-depth when a command needs an open browser session but the handle is gone; the process usually keeps accepting commands. A `close` that finds no session still ends the process after the error response.

## Ordering And Termination

Commands receive one response each in request order. A malformed or oversized line receives its own error and does not cancel active work.

`cancel` is acknowledged immediately with `{cancelling:true}`. That commits the active compatibility submission to terminal cancellation even if it concurrently finishes. The submission then receives `cancelled`; the process terminates with exit 130 unless an infrastructure failure wins.

An idle close dismisses any pending dialog, finalizes the recorder and artifacts, disposes the router/browser context, and returns its one close response. During a compatibility submission, close requests cancellation, drains the submission, writes its `cancelled` response, performs shutdown, then writes the close response. No command is accepted after close is pending.

Exit codes remain `0` for a successful close, `2` for invalid startup input, `3` for automation/assertion failure where applicable, `4` for fatal infrastructure/artifact failure, and `130` for interruption or committed cancellation.

## Versioning

Version 1 is selected by `--protocol ndjson`; commands do not carry a per-envelope version. Within version 1, optional request/result/error fields, commands, and error codes may be added. Clients must ignore unknown response fields and treat unknown error codes as errors according to whether the process remains open. Existing field meanings, response ordering, limits, and terminal exit meanings do not change within version 1.

An incompatible wire contract requires a new explicit protocol selector. Wrappers must preserve client IDs, response order, revisions, limits, fatality, and close/cancel semantics.
