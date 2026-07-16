# Playrust

Playrust is a YAML-based web testing automation tool written in Rust. It runs sequential flows in isolated Chromium browser contexts, waits for actions and assertions, and produces machine-readable artifacts.

## Demo

[Watch Playrust run three browser tests](docs/playrust-demo.mp4) covering sign-in, task management, and interactive controls.

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

`check` validates YAML, inputs, URLs, durations, keys, and configuration without launching Chromium. A path may be one `.yaml`/`.yml` file or a directory, searched recursively in stable path order.

```text
playrust check <path> [--var NAME=VALUE]
playrust run <path> [--headed] [--jobs N] [--browser PATH]
                    [--var NAME=VALUE] [--video MODE]
                    [--ffmpeg-path PATH] [--artifacts DIR]
```

`run` defaults to headless mode, up to four concurrent flows, and `./playrust-artifacts`. Use `--jobs 1` for sequential execution. Exit codes are `0` for success, `2` for invalid input/specification, `3` for an automation or assertion failure, `4` for infrastructure/recording failure, and `130` when interrupted.

## YAML V1

Every file requires `version: 1`, a non-empty `name`, and non-empty `steps`. Unknown fields, duplicate keys, aliases, merge keys, unresolved inputs, and multiple operations in one step are rejected. A step may also have a unique `id` and a `timeout` using an integer followed by `ms`, `s`, or `m`.

Defaults are a `10s` timeout, `1280x720` viewport, and video `off`.

```yaml
settings:
  timeout: 10s
  viewport: { width: 1280, height: 720 }
  video: retain-on-failure
```

### Actions

| Action | Form |
| --- | --- |
| Navigate | `open: /path` (requires `base_url`) or an absolute HTTP(S) URL |
| Click | `click: { target: { test_id: submit } }` |
| Fill | `fill: { target: { label: Email }, value: "${username}" }` |
| Press | `press: { target: { label: Search }, key: Enter, modifiers: [Control] }` |

Click, fill, and press wait for one unique, visible, stable, enabled, uncovered target; fill also requires an editable target. Supported named keys are `Enter`, `Tab`, `Escape`, `Space`, `Backspace`, `Delete`, arrow keys, `Home`, `End`, `PageUp`, and `PageDown`. A key may also be one printable non-whitespace character. Modifiers are `Alt`, `Control`, `Meta`, and `Shift`.

### Selectors

```yaml
target: { css: "button.primary" }
target: { test_id: submit }
target: { text: "Sign in" }
target: { text: { value: "Welcome", match: contains } }
target: { label: Email }
target: { role: { value: button, name: "Sign in" } }
```

Each target has exactly one strategy. `test_id` matches `data-testid`. Label and role use Chromium accessibility names. Text matching is case-sensitive, whitespace-collapsed, and exact unless `match: contains` is set.

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

Video modes are `off`, `on`, and `retain-on-failure`. `on` keeps every recording; `retain-on-failure` removes recordings for passing flows. Set a mode in YAML or override all flows with `--video MODE`.

Recording requires an `ffmpeg` executable on `PATH`, or `--ffmpeg-path PATH`, with the `libvpx-vp9` encoder. Playrust records the fixed page viewport as silent 15 FPS WebM/VP9; enabled video requires even viewport dimensions. Browser chrome, audio, OS dialogs, and a guaranteed pointer image are not recorded.

Each `run` writes `<artifacts>/report.json`. Per-flow directories may contain `failure.png`, `recording.webm`, or `recording.partial.webm` when finalization fails. Change the root with `--artifacts DIR`.

## Boundaries

Playrust supports only its pinned Chromium build and rejects a different version supplied by path. V1 operates in the main frame with one page per isolated flow. Iframes, shadow-root traversal, popups, multiple tabs, uploads/downloads, browser extensions, and mobile-native automation are not supported. Flows are sequential and do not provide sleeps, scripts, loops, branches, mutable variables, imports, or plugins.

See [`examples/example.yaml`](examples/example.yaml) for a complete runnable V1 flow.
