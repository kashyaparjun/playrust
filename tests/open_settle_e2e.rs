mod support;

use std::fs;

use playrust::report::{FailureCategory, FlowStatus};
use support::{FixtureServer, assert_success, playrust, read_report};

/// Page that reveals `#late` 300ms after load.
const LATE_HTML: &str = r#"<!doctype html><body><p id="late" hidden>ready</p><script>setTimeout(() => document.querySelector('#late').hidden = false, 300)</script></body>"#;

/// Page that never shows `#missing`.
const STATIC_HTML: &str = r#"<!doctype html><body><p id="present">stable</p></body>"#;

/// Page that inserts `#dynamic` after `load` so a non-settled `open` could race it.
const DYNAMIC_HTML: &str = r#"<!doctype html><body><div id="shell"></div><script>window.addEventListener('load', () => { setTimeout(() => { const el = document.createElement('p'); el.id = 'dynamic'; el.textContent = 'injected'; document.querySelector('#shell').appendChild(el); }, 200); });</script></body>"#;

/// Intermediate page that immediately navigates to `/done`.
const REDIRECT_HTML: &str =
    r#"<!doctype html><body><script>location.replace('/done')</script></body>"#;

/// Final page after the client-side redirect.
const DONE_HTML: &str = r#"<!doctype html><body><h1 id="done">arrived</h1></body>"#;

/// Page whose target moves, then stops — same fixture shape as wait_until_stable_e2e.
const MOVING_HTML: &str = r#"<!doctype html><style>#moving { width:100px; height:50px; background:#ccc; animation:move 600ms linear } @keyframes move { from { transform:translateX(0) } to { transform:translateX(300px) } }</style><body><div id="moving"></div><p id="status">moving</p><script>document.querySelector('#moving').addEventListener('animationend', () => document.querySelector('#status').textContent = 'stable')</script></body>"#;

fn write_flow(
    directory: &std::path::Path,
    name: &str,
    base_url: &str,
    body: &str,
) -> std::path::PathBuf {
    let flow = directory.join(format!("{name}.yaml"));
    fs::write(
        &flow,
        format!(
            "version: 1\nname: {name}\nbase_url: {base_url}\nsettings: {{ video: off }}\nsteps:\n{body}",
        ),
    )
    .expect("write flow");
    flow
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_settles_until_a_late_element_becomes_visible() {
    let server = FixtureServer::start(&[("/", "text/html", LATE_HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let artifacts = directory.path().join("artifacts");
    let flow = write_flow(
        directory.path(),
        "open-settle-visible",
        &server.url(),
        "  - timeout: 2s\n    open: { url: /, wait_until: { visible: { css: '#late' } } }\n",
    );

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_settle_timeout_is_reported_when_the_element_never_appears() {
    let server = FixtureServer::start(&[("/", "text/html", STATIC_HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let artifacts = directory.path().join("artifacts");
    let flow = write_flow(
        directory.path(),
        "open-settle-timeout",
        &server.url(),
        "  - timeout: 1s\n    open: { url: /, wait_until: { visible: { css: '#missing' } } }\n",
    );

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert!(
        !run.status.success(),
        "settle should time out\nstderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = read_report(&artifacts);
    assert_eq!(report.flows[0].status, FlowStatus::Failed);
    let failure = &report.flows[0].failures[0];
    assert_eq!(failure.category, FailureCategory::Locator);
    assert!(
        failure
            .message
            .as_str()
            .contains("open settle condition was not satisfied before the step deadline"),
        "expected settle timeout message, got {}",
        failure.message
    );
    let step = failure.step.as_ref().expect("step context");
    assert_eq!(step.operation, "open");
    assert_eq!(
        step.locator.as_ref().map(|locator| locator.as_str()),
        Some("css=\"#missing\"")
    );
    assert_eq!(failure.timeout_ms, Some(1000));
    assert!(
        failure.last_observed.is_some(),
        "settle timeout should include last_observed"
    );
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_settle_waits_for_content_injected_after_load() {
    let server = FixtureServer::start(&[("/", "text/html", DYNAMIC_HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let artifacts = directory.path().join("artifacts");
    let flow = write_flow(
        directory.path(),
        "open-settle-dynamic",
        &server.url(),
        "  - timeout: 2s\n    open: { url: /, wait_until: { visible: { css: '#dynamic' } } }\n  - assert: { visible: { css: '#dynamic' } }\n",
    );

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_settle_follows_a_client_side_redirect() {
    let server = FixtureServer::start(&[
        ("/", "text/html", REDIRECT_HTML),
        ("/done", "text/html", DONE_HTML),
    ]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let artifacts = directory.path().join("artifacts");
    let flow = write_flow(
        directory.path(),
        "open-settle-redirect",
        &server.url(),
        "  - timeout: 2s\n    open: { url: /, wait_until: { visible: { css: '#done' } } }\n",
    );

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_settles_until_a_moving_element_becomes_stable() {
    let server = FixtureServer::start(&[("/", "text/html", MOVING_HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let artifacts = directory.path().join("artifacts");
    let flow = write_flow(
        directory.path(),
        "open-settle-stable",
        &server.url(),
        "  - timeout: 2s\n    open: { url: /, wait_until: { stable: { css: '#moving' } } }\n  - assert: { text: { target: { css: '#status' }, equals: stable } }\n",
    );

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_without_wait_until_is_backward_compatible() {
    let server = FixtureServer::start(&[("/", "text/html", STATIC_HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let artifacts = directory.path().join("artifacts");
    let flow = write_flow(
        directory.path(),
        "open-plain",
        &server.url(),
        "  - open: /\n  - assert: { visible: { css: '#present' } }\n",
    );

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}
