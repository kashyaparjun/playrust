mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><body><p id="late" hidden>ready</p><script>setTimeout(() => document.querySelector('#late').hidden = false, 300)</script></body>"#;

#[test]
fn uses_the_explicit_step_timeout_for_visibility() {
    let Some(chrome) = support::require_browser("uses_the_explicit_step_timeout_for_visibility")
    else {
        return;
    };
    let chrome_env = support::chrome_env(&chrome);
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("wait-until-visible.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: wait-until-visible\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - timeout: 2s\n    wait_until_visible: {{ target: {{ css: '#late' }} }}\n",
            server.url()
        ),
    )
    .expect("write flow");

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[(&chrome_env.0, &chrome_env.1)],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}
