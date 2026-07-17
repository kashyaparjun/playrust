mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><html><body>
<label for="token">Token</label><input id="token">
</body></html>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn http_setup_saves_bounded_json_for_later_page_values() {
    let server = FixtureServer::start_with(|request| {
        if request.starts_with("POST /setup ")
            && request.to_ascii_lowercase().contains("x-setup: yes")
            && request.ends_with("ready")
        {
            (201, "application/json", r#"{"token":"local-token"}"#)
        } else {
            (200, "text/html; charset=utf-8", HTML)
        }
    });
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("http.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            r#"version: 1
name: http-e2e
base_url: http://{}
settings: {{ video: off }}
steps:
  - open: /
  - request:
      method: POST
      url: http://{}/setup
      headers: {{ x-setup: yes }}
      body: ready
      expected_status: 201
      save_as: setup
  - evaluate:
      script: return JSON.parse(args[0]).token;
      args: ["${{setup}}"]
      save_as: token
  - fill: {{ target: {{ label: Token }}, value: "${{token}}" }}
  - evaluate:
      script: if (document.querySelector('#token').value !== 'local-token') throw new Error('wrong token');
"#,
            server.address, server.address
        ),
    )
    .expect("write HTTP flow");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    assert_success("run HTTP fixture", &output);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}
