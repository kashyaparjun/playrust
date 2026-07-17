mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><html><head><title>Fixture</title></head><body>
<label for="result">Result</label><input id="result">
</body></html>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn page_script_receives_separate_arguments_and_saves_a_flow_local_value() {
    let server = FixtureServer::start_with(|_| (200, "text/html; charset=utf-8", HTML));
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("scripting.yaml");
    let child = directory.path().join("script.subflow.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &child,
        r#"version: 1
name: script-child
vars: { prefix: unset }
steps:
  - evaluate:
      script: window.__runs = (window.__runs || 0) + 1; return args[0] + '-' + document.title + '-' + window.__runs;
      args: ["${prefix}"]
      save_as: result
"#,
    )
    .expect("write scripting subflow");
    fs::write(
        &flow,
        format!(
            r#"version: 1
name: scripting-e2e
base_url: http://{}
settings: {{ video: off }}
steps:
  - open: /
  - run:
      path: ./script.subflow.yaml
      vars: {{ prefix: 'prefix; throw new Error("not source")' }}
    repeat: 2
  - fill: {{ target: {{ label: Result }}, value: "${{result}}" }}
  - evaluate:
      script: if (document.querySelector('#result').value !== args[0]) throw new Error('wrong value');
      args: ['prefix; throw new Error("not source")-Fixture-2']
"#,
            server.address
        ),
    )
    .expect("write scripting flow");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    assert_success("run scripting fixture", &output);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn runtime_script_failures_are_redacted_and_keep_subflow_provenance() {
    let server = FixtureServer::start_with(|_| (200, "text/html; charset=utf-8", HTML));
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("failure.yaml");
    let child = directory.path().join("failure.subflow.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: runtime-failure\nbase_url: http://{}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - run: ./failure.subflow.yaml\n",
            server.address
        ),
    )
    .expect("write failure flow");
    fs::write(
        &child,
        "version: 1\nname: failure-child\nsteps:\n  - evaluate: { script: 'return args[0]', args: [runtime-canary], save_as: secret }\n  - evaluate: { script: 'throw new Error(args[0])', args: ['${secret}'] }\n",
    )
    .expect("write failure subflow");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(4));
    let report = read_report(&artifacts);
    let failure = &report.flows[0].failures[0];
    let step = failure.step.as_ref().expect("step provenance");
    assert_eq!(step.source_step, Some(2));
    assert_eq!(step.operation, "evaluate");
    assert!(
        step.source
            .as_deref()
            .is_some_and(|source| source.ends_with("failure.subflow.yaml"))
    );
    let diagnostics = format!(
        "{}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(artifacts.join("report.json")).expect("read report")
    );
    assert!(!diagnostics.contains("runtime-canary"), "{diagnostics}");
}
