mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><body><div id="card" style="margin:200px;width:200px;height:100px;background:#ccc">Swipe</div><p id="status">pending</p><script>
let start;
const card = document.querySelector('#card');
const result = document.querySelector('#status');
card.addEventListener('mousedown', event => start = event.clientX);
addEventListener('mouseup', event => result.textContent = event.clientX - start <= -100 ? 'swiped' : 'short');
</script></body>"#;

#[test]
fn swipes_once_from_an_actionable_target() {
    let Some(chrome) = support::require_browser("swipes_once_from_an_actionable_target") else {
        return;
    };
    let chrome_env = support::chrome_env(&chrome);
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("swipe.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: swipe\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - swipe: {{ target: {{ css: '#card' }}, x: -120, duration: 100ms }}\n  - assert: {{ text: {{ target: {{ css: '#status' }}, equals: swiped }} }}\n",
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
