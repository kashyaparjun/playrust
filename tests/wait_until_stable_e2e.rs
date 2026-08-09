mod support;

use std::fs;

use libtest_mimic::Failed;
use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, harness, playrust, read_report};

const HTML: &str = r#"<!doctype html><style>#moving { width:100px; height:50px; background:#ccc; animation:move 600ms linear } @keyframes move { from { transform:translateX(0) } to { transform:translateX(300px) } }</style><body><div id="moving"></div><p id="status">moving</p><script>document.querySelector('#moving').addEventListener('animationend', () => document.querySelector('#status').textContent = 'stable')</script></body>"#;

fn main() {
    harness::run(vec![harness::browser_cli_trial(
        "waits_for_two_stable_actionability_samples",
        waits_for_two_stable_actionability_samples,
    )]);
}

fn waits_for_two_stable_actionability_samples() -> Result<(), Failed> {
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
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
    Ok(())
}
