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
    assert_eq!(
        report.flows[0].failures[0].category,
        FailureCategory::Locator
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
