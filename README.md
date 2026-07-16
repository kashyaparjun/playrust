# Playrust

Playrust is a YAML-based web testing automation tool written in Rust. It runs sequential flows in isolated Chromium browser contexts, waits for actions and assertions, and produces machine-readable artifacts.

## Demo

[![Playrust browser testing demo](docs/playrust-demo.gif)](docs/playrust-demo.mp4)

[Watch the full MP4](docs/playrust-demo.mp4). The demo shows the YAML flow, Chromium launch, and one continuous test covering sign-in, task management, and interactive controls.

## Install

Build from source with Rust 1.89 or newer:

```sh
cargo build --release
cargo install --path .
playrust browser install
```

`playrust run` automatically installs pinned Chrome for Testing `151.0.7922.34` if needed. You can instead provide that exact build with `--browser PATH` or `PLAYRUST_CHROME`. Supported installation targets are Linux x86_64, macOS arm64/x86_64, and Windows x86_64.

## Quick start

Serve the included page in one terminal:

```sh
python3 -m http.server 8000 --directory examples
```

Then validate and run the flow:

```sh
export PLAYRUST_EXAMPLE_PASSWORD=local-secret
playrust check examples/example.yaml --var username=alice
playrust run examples/example.yaml --var username=alice
```

`check` validates YAML, inputs, URLs, durations, keys, subflows, and configuration without launching Chromium. A path may be one `.yaml`/`.yml` file or a directory, searched recursively in stable path order. Files ending in `.subflow.yaml` or `.subflow.yml` are not directory entrypoints; they run only when included.

```text
playrust check <path> [--var NAME=VALUE]
playrust run <path> [--headed] [--jobs N] [--browser PATH]
                    [--var NAME=VALUE] [--video MODE]
                    [--ffmpeg-path PATH] [--artifacts DIR] [--junit] [--html]
```

`run` defaults to headless mode, retained video recording, up to four concurrent flows, and `./playrust-artifacts`. Use `--video off` to disable recording or `--jobs 1` for sequential execution. Exit codes are `0` for success, `2` for invalid input/specification, `3` for an automation or assertion failure, `4` for infrastructure/recording failure, and `130` when interrupted.

## YAML V1

Every file requires `version: 1`, a non-empty `name`, and non-empty `steps`. Unknown fields, duplicate keys, aliases, merge keys, unresolved inputs, and multiple operations in one step are rejected. A step may also have a unique `id` and a `timeout` using an integer followed by `ms`, `s`, or `m`.

Defaults are a `10s` timeout, `1280x720` viewport, and video `on`.

```yaml
settings:
  timeout: 10s
  viewport: { width: 1280, height: 720 }
  video: retain-on-failure
  geolocation: { latitude: 51.5074, longitude: -0.1278, accuracy: 10 }
```

`geolocation` is optional. Latitude must be between `-90` and `90`, longitude between `-180` and `180`, and accuracy must be a non-negative number in meters; accuracy defaults to `0`. When set, Playrust grants geolocation permission only in that flow's isolated browser context and applies the coordinates to its page before any steps run.

### Subflows

Include reusable steps at compile time with a relative path:

```yaml
steps:
  - open: /login
  - run: ./shared/sign-in.subflow.yaml
  - assert: { url: { path: /dashboard } }
```

A subflow is a normal V1 document with `version`, `name`, and `steps`. Includes are expanded in place, may be nested up to 32 levels, and the expanded flow may contain at most 10,000 steps. Include paths are literal, must end in `.subflow.yaml` or `.subflow.yml`, and resolve relative to the file containing the `run` step. The `run` field must be the only field in its step. Canonical active include paths are checked for cycles; the same subflow may be included more than once when it is not already active.

Each file resolves its own `base_url`, default `settings.timeout`, `vars`, and `secrets`; these values are not inherited across file boundaries. CLI variables apply to every file that declares the name and are rejected unless at least one file declares them. Child secrets remain redacted in root-flow diagnostics. Subflows cannot set `settings.viewport` or `settings.video`; those runtime-wide settings belong to the entrypoint. Child names and resolved inputs do not replace the entrypoint's report identity or inputs. Runtime failures from expanded steps report both the expanded step number and child source path/local step number.

### Actions

| Action | Form |
| --- | --- |
| Navigate | `open: /path` (requires `base_url`) or an absolute HTTP(S) URL |
| Click | `click: { target: { test_id: submit } }` |
| Double click | `double_click: { target: { test_id: item } }` |
| Fill | `fill: { target: { label: Email }, value: "${username}" }` |
| Erase | `erase: { target: { label: Search } }` |
| Select option | `select: { target: { label: Region }, value: us-east }` |
| Scroll viewport | `scroll: { x: 0, y: 600 }` |
| Navigate back | `back: {}` |
| Press | `press: { target: { label: Search }, key: Enter, modifiers: [Control] }` |
| Screenshot | `screenshot: { name: dashboard }` |
| Clear cookies | `clear: cookies` |
| Clear storage | `clear: storage` |

Click, double click, fill, erase, select, and press wait for one unique, visible, stable, enabled, uncovered target; fill and erase also require an editable target. Erase supports the same text inputs, textareas, and content-editable elements as fill. Select accepts an option value (including an empty value) on a native, non-`multiple` `<select>` and dispatches one bubbling `input` event followed by one `change` event. Input actions are not retried after pointer, keyboard, wheel, or form event dispatch begins.

Scroll sends one wheel input at the viewport center; positive values move right/down and negative values move left/up, and at least one axis must be non-zero. Back navigates one browser-history entry and fails when there is no previous entry. Both use the step timeout and normal cancellation/error reporting. Supported named keys are `Enter`, `Tab`, `Escape`, `Space`, `Backspace`, `Delete`, arrow keys, `Home`, `End`, `PageUp`, and `PageDown`. A key may also be one printable non-whitespace character. Modifiers are `Alt`, `Control`, `Meta`, and `Shift`.

Screenshots are PNG files written atomically to the flow artifact directory as `<name>.png`. Names may contain 1-64 ASCII letters, numbers, `-`, or `_`, must start and end with a letter or number, cannot be `failure` or a Windows-reserved filename, cannot contain secrets, and must be case-insensitively unique within a flow. An optional crop uses viewport-relative CSS pixels and must fit within the configured viewport:

```yaml
- screenshot:
    name: dashboard-panel
    crop: { x: 40, y: 80, width: 640, height: 360 }
```

`clear: cookies` clears every cookie in the active flow's isolated browser context only. `clear: storage` clears `localStorage` and `sessionStorage` for the active page origin only; it does not clear storage for other origins or other browser storage types. Both commands use the step timeout and produce the same cancellation and failure diagnostics as other actions.

### Selectors

```yaml
target: { css: "button.primary" }
target: { test_id: submit }
target: { text: "Sign in" }
target: { text: { value: "Welcome", match: contains } }
target: { label: Email }
target: { role: { value: button, name: "Sign in" } }
target: { css: ".row", checked: true, index: 0 }
```

Each target has exactly one strategy. `test_id` matches `data-testid`. Label and role use Chromium accessibility names. Text matching is case-sensitive, whitespace-collapsed, and exact unless `match: contains` is set.

The optional boolean filters `checked`, `selected`, and `focused` match the corresponding live DOM state. `checked` applies only to checkable elements and `selected` only to selectable elements. An optional zero-based `index` then selects from the filtered matches in DOM order. Filters and `index` run before uniqueness and actionability checks; without `index`, actions and positive assertions still require one final match. Relational selectors are not supported.

### Assertions

```yaml
- assert: { visible: { text: Welcome } }
- assert: { hidden: { css: .spinner } }
- assert:
    text: { target: { test_id: status }, equals: "Saved" }
- assert:
    text: { target: { css: .message }, contains: "complete" }
- assert: { url: { equals: "https://example.test/dashboard" } }
- assert: { url: { path: "/dashboard?tab=home" } }
```

Positive assertions require one unique visible target. `hidden` passes with no matches or when all matches are hidden. URL `equals` compares the full URL; `path` compares the encoded path and, when supplied, query while ignoring origin and fragment. Actions and assertions retry until their condition passes or the step deadline expires.

## Variables and secrets

```yaml
vars:
  region: local
  username: { env: TEST_USERNAME, default: guest }
secrets:
  password: { env: TEST_PASSWORD }
```

Use `${name}` in YAML strings. Variables are immutable literals or environment mappings with an optional default; `--var NAME=VALUE` overrides only names declared under `vars`. Secrets must be environment mappings and cannot have defaults or CLI overrides. Secret-derived values are redacted from terminal diagnostics and `report.json`, but screenshots and videos can contain secrets rendered by the tested page.

Treat flow files as executable test configuration. They can navigate to private services and transmit environment values whose names are explicitly declared in the flow.

## Video and artifacts

Video modes are `off`, `on`, and `retain-on-failure`. Recording defaults to `on`, which keeps every recording and prints its path after the flow finishes. `retain-on-failure` removes recordings for passing flows. Set a mode in YAML or override all flows with `--video MODE`.

Recording requires an `ffmpeg` executable on `PATH`, or `--ffmpeg-path PATH`, with the `libvpx-vp9` encoder. Playrust records the fixed page viewport as silent 15 FPS WebM/VP9; enabled video requires even viewport dimensions. Browser chrome, audio, OS dialogs, and a guaranteed pointer image are not recorded.

Each `run` writes `<artifacts>/report.json`. Pass `--junit` to also atomically write `<artifacts>/junit.xml`; automation failures are JUnit failures, while invalid specifications, infrastructure failures, and interruptions are JUnit errors. Pass `--html` to atomically write `<artifacts>/report.html`, a self-contained static summary with inline CSS, no scripts or external resources, and plain-text artifact paths. Optional reports are removed when their flags are omitted, so stale output cannot be mistaken for the current run. Successful named screenshot paths are listed under each flow's `artifacts.screenshots`. Per-flow directories may also contain `failure.png`, `recording.webm`, or `recording.partial.webm` when finalization fails. Change the root with `--artifacts DIR`.

## Boundaries

Playrust supports only its pinned Chromium build and rejects a different version supplied by path. V1 operates in the main frame with one page per isolated flow. Iframes, shadow-root traversal, popups, multiple tabs, uploads/downloads, browser extensions, and mobile-native automation are not supported. Flows are sequential and do not provide sleeps, scripts, loops, branches, mutable variables, parameterized imports, or plugins.

See [`examples/example.yaml`](examples/example.yaml) for a complete runnable V1 flow.
