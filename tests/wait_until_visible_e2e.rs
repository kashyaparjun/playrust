mod support;

use std::fs;

use libtest_mimic::Failed;
use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, harness, playrust, read_report};

const HTML: &str = r#"<!doctype html><body><p id="late" hidden>ready</p><script>setTimeout(() => document.querySelector('#late').hidden = false, 300)</script></body>"#;

fn main() {
    harness::run(vec![harness::browser_cli_trial(
        "uses_the_explicit_step_timeout_for_visibility",
        uses_the_explicit_step_timeout_for_visibility,
    )]);
}

fn uses_the_explicit_step_timeout_for_visibility() -> Result<(), Failed> {
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
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
    Ok(())
}
