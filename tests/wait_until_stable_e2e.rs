mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><style>#moving { width:100px; height:50px; background:#ccc; animation:move 600ms linear } @keyframes move { from { transform:translateX(0) } to { transform:translateX(300px) } }</style><body><div id="moving"></div><p id="status">moving</p><script>document.querySelector('#moving').addEventListener('animationend', () => document.querySelector('#status').textContent = 'stable')</script></body>"#;

#[test]
fn waits_for_two_stable_actionability_samples() {
    let Some(chrome) = support::require_browser("waits_for_two_stable_actionability_samples")
    else {
        return;
    };
    let chrome_env = support::chrome_env(&chrome);
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("wait-until-stable.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: wait-until-stable\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - timeout: 2s\n    wait_until_stable: {{ target: {{ css: '#moving' }} }}\n  - assert: {{ text: {{ target: {{ css: '#status' }}, equals: stable }} }}\n",
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
