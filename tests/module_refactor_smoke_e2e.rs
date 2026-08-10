mod support;

use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use playrust::browser::BrowserHost;
use playrust::flow::{
    FrameLocation, PageLocation, RawLocator, RawSettings, RawStep, RawVariable, RawViewport,
    ViewportPoint, compile_file, compile_yaml,
};
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use serde_json::{Value, json};
use support::{FixtureServer, assert_success, playrust};

#[test]
fn check_compile_and_library_paths_remain_available_after_refactor() {
    let output = playrust(&["check", "examples/showcase/01-login.yaml"], &[]);
    assert_success("check showcase flow", &output);

    let flow = compile_file("examples/showcase/01-login.yaml", &BTreeMap::new())
        .expect("compile via flow module");
    assert_eq!(flow.name, "showcase-login");

    let inline = compile_yaml(
        "version: 1\nname: inline\nsteps: [{ pause: 1ms }]\n",
        "inline.yaml",
        &BTreeMap::new(),
    )
    .expect("compile inline yaml");
    assert_eq!(inline.steps.len(), 1);

    // Prior public raw/surface paths must remain reachable after the split.
    let _: Option<RawSettings> = None;
    let _: Option<RawStep> = None;
    let _: Option<RawLocator> = None;
    let _: Option<RawViewport> = None;
    let _: Option<RawVariable> = None;
    let _: Option<PageLocation> = None;
    let _: Option<FrameLocation> = None;
    let _: Option<ViewportPoint> = None;
    let settings = RawSettings {
        timeout: None,
        viewport: Some(RawViewport {
            width: 1280,
            height: 720,
        }),
        video: None,
        geolocation: None,
        overlays: None,
    };
    assert!(settings.viewport.is_some());
    let _ = (
        RawVariable::Literal("x".into()),
        PageLocation::Popup,
        FrameLocation::Main,
        ViewportPoint { x: 1, y: 2 },
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn decomposed_runner_executes_fixture_flow_end_to_end() {
    let chrome = env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME");
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Refactor smoke</title><p id='status'>ok</p>",
    )]);
    let flow = compile_yaml(
        &format!(
            "version: 1\nname: refactor-smoke\nbase_url: http://{}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - assert: {{ text: {{ target: {{ css: '#status' }}, equals: ok }} }}\n",
            server.address
        ),
        "refactor-smoke.yaml",
        &BTreeMap::new(),
    )
    .unwrap();
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn decomposed_session_protocol_handles_act_and_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Session smoke</title>",
    )]);
    let mut session = Session::start(&directory.path().join("artifacts"));
    assert_eq!(
        session.command(json!({
            "id": "open",
            "command": "act",
            "action": { "open": { "url": format!("{}/", server.url) } }
        }))["ok"],
        true
    );
    let snapshot = session.command(json!({ "id": "snapshot", "command": "snapshot" }));
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["result"]["title"], "Session smoke");
    assert_eq!(
        session.command(json!({ "id": "close", "command": "close" }))["ok"],
        true
    );
    assert!(session.finish().status.success());
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(artifacts: &Path) -> Self {
        let chrome = env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME");
        let mut child = Command::new(env!("CARGO_BIN_EXE_playrust"))
            .args([
                "session",
                "--protocol",
                "ndjson",
                "--browser",
                chrome.to_str().expect("UTF-8 Chrome path"),
                "--artifacts",
                artifacts.to_str().expect("UTF-8 artifact path"),
                "--video",
                "off",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    fn send(&mut self, value: Value) {
        serde_json::to_writer(self.stdin.as_mut().unwrap(), &value).unwrap();
        self.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("session JSON")
    }

    fn command(&mut self, value: Value) -> Value {
        self.send(value);
        self.read()
    }

    fn finish(mut self) -> std::process::Output {
        drop(self.stdin.take());
        self.child.wait_with_output().unwrap()
    }
}
