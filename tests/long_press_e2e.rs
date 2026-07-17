mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><body><button id="target">Hold</button><p id="status">pending</p><script>
let started;
const target = document.querySelector('#target');
const result = document.querySelector('#status');
target.addEventListener('mousedown', () => started = performance.now());
target.addEventListener('mouseup', () => result.textContent = performance.now() - started >= 250 ? 'held' : 'short');
</script></body>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn holds_and_releases_an_actionable_target_once() {
    let server = FixtureServer::start(HTML);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("long-press.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: long-press\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - long_press: {{ target: {{ css: '#target' }}, duration: 300ms }}\n  - assert: {{ text: {{ target: {{ css: '#status' }}, equals: held }} }}\n",
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
}
