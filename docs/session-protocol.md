# Session Protocol

`playrust session --protocol ndjson` is the version 1 local persistent-session protocol. It is intended as the process boundary for local clients and future cloud wrappers.

## Transport

The client writes one UTF-8 JSON object per stdin line. Playrust writes one compact JSON response per stdout line and flushes it before processing continues. Diagnostics are written only to stderr.

One command envelope may contain at most 1,048,576 bytes before its `LF`. The limit includes a `CR` in `CRLF`. The reader appends at most that many bytes to its command buffer; Tokio's `BufReader` may separately hold its fixed internal buffer. An oversized envelope is drained through its next `LF` without retaining the excess and receives a recoverable `envelope_too_large` error with `id: null`. Invalid UTF-8, malformed JSON, and invalid command shapes receive a recoverable `invalid_command` error. If the transport remains open, the next line is processed normally. An stdin read failure or stdout write failure is fatal and may prevent a response.

EOF while idle closes the session without a response. EOF during a submission cancels and drains that submission before termination.

## Commands

Every valid command is a JSON object with an `id` and `command`. An ID may be any JSON value and is echoed unchanged; clients should use unique string or integer IDs.

```json
{"id":"s1","command":"submit","flow":"version: 1\nname: example\nsettings: { video: off }\nsteps: [{ open: https://example.com }]\n","variables":{"region":"west"}}
{"id":"i1","command":"inspect","accessibility":true,"screenshot":true}
{"id":"o1","command":"output","name":"saved_value"}
{"id":"x1","command":"cancel"}
{"id":"c1","command":"close"}
```

`submit.flow` is a complete inline YAML V1 flow. `variables` defaults to an empty object. Filesystem subflows and filesystem screenshot baselines are not accepted. The first valid submission fixes viewport and geolocation for the session. Browser context state, pages, active page/frame, page JavaScript state, and runtime outputs persist; execution control state, recording, and artifacts do not.

`inspect.accessibility` and `inspect.screenshot` default to `false`. Inspection returns the active URL and title, up to 100 context-owned pages, the active frame URL when inside a frame, and requested inspection data. Accessibility output is bounded to depth 8, 500 nodes, and 256 KiB. A screenshot result is an artifact path, not image bytes.

`output.name` returns one persisted runtime output. `cancel` is valid only while a submission is active. `close` is valid while idle or active.

## Responses

Every response has this envelope:

```json
{"id":"s1","ok":true,"session_id":"stable-id","revision":1,"result":{}}
{"id":"s1","ok":false,"session_id":"stable-id","revision":1,"error":{"code":"validation","message":"...","details":{}}}
```

`session_id` is stable for the process. `revision` starts at 1 and increases by exactly one in response order. Exactly one of `result` or `error` is present. `error.details` is optional.

Successful result shapes are:

| Command | Result |
| --- | --- |
| `submit` | A complete `FlowReport` object. |
| `inspect` | `{url,title,pages,active_frame?,accessibility?,screenshot?}`. |
| `output` | `{name,value}`. |
| `cancel` | `{cancelling:true}`. |
| `close` | `{closed:true}`. |

Submission failures use `error.details` for the complete `FlowReport`. Stable error codes are:

| Code | Meaning | Session remains usable |
| --- | --- | --- |
| `invalid_command` | Invalid UTF-8, JSON, or command shape. | Yes |
| `envelope_too_large` | Command exceeds 1 MiB. | Yes |
| `validation` | Inline flow or variables are invalid. | Yes |
| `settings_conflict` | Viewport or geolocation differs from the established session. | Yes |
| `busy` | A non-cancel/non-close command arrived during submission. | Yes |
| `not_active` | `cancel` arrived while idle. | Yes |
| `not_started` | `inspect` arrived before the first valid submission. | Yes |
| `output_not_found` | The named runtime output is unavailable. | Yes |
| `submission_failed` | Recoverable automation or assertion failure. | Yes |
| `cancelled` | Submission drained after cancellation. | No |
| `browser` | Browser launch, protocol, crash, fatal submission, inspection, or disposal failure. | No |
| `artifacts` | Session report persistence failed. | No |

## Ordering And Termination

Only one submission executes at a time. While it is active, commands are handled in input order. Non-cancel/non-close commands receive `busy`. A malformed or oversized line receives its own error and does not cancel the submission.

`cancel` is acknowledged immediately with `{cancelling:true}`. The submission then drains and receives `cancelled`; the session disposes its router/context and terminates with exit 130.

An idle `close` disposes the OOPIF router and browser context before returning `{closed:true}`. During submission, `close` has no interim acknowledgement: it requests cancellation, waits for the submission to drain, writes the submission's `cancelled` response, disposes the router/context, then writes the one and only close response. A close during submission therefore terminates with exit 130 unless disposal or shutdown itself fails. No later command is accepted after a close is pending.

Malformed commands, validation failures, settings conflicts, missing outputs, and ordinary automation/assertion failures are recoverable. A later successful close returns exit 0. Cancellation returns exit 130. Browser launch/crash, CDP protocol, recording, artifact, inspection, disposal, browser shutdown, and transport failures are fatal and return exit 4. A fatal infrastructure failure takes precedence over cancellation when both occur.

## Versioning

Version 1 is selected by `--protocol ndjson`; commands do not carry a per-envelope version. Within version 1, new optional request fields, result fields, error detail fields, commands, and error codes may be added. Clients must ignore unknown response fields and treat unknown error codes as errors according to whether the process remains open. Existing field meanings, response ordering, limits, and terminal exit meanings will not change within version 1.

A future incompatible wire contract must use a new explicit protocol selector rather than silently changing `ndjson`. Cloud wrappers should preserve client IDs, response order, revisions, limits, fatality, and close/cancel semantics when translating this protocol.
