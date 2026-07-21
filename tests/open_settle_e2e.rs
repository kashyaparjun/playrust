mod support;

use playrust::report::FlowStatus;
use std::fs;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><body><p id="ready" hidden>ready</p><script>setTimeout(() => document.querySelector('#ready').hidden = false, 300)</script></body>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn open_waits_for_dynamic_visibility_before_the_next_step() {
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("open-settle.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, format!("version: 1\nname: open-settle\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - timeout: 2s\n    open:\n      url: /\n      wait_until:\n        visible: {{ css: '#ready' }}\n  - assert: {{ text: {{ target: {{ css: '#ready' }}, equals: ready }} }}\n", server.url())).expect("write flow");
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
