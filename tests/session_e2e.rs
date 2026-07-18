mod support;

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use serde_json::{Value, json};
use support::{FixtureServer, read_report};

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn state_storage_tab_frame_inspection_and_artifacts_persist() {
    let foreign = FixtureServer::start(&[(
        "/foreign",
        "text/html; charset=utf-8",
        "<!doctype html><title>OOPIF fixture</title><button onclick=\"document.querySelector('#status').textContent='continued'\">Continue</button><p id='status'>pending</p>",
    )]);
    let foreign_url = foreign.url().replace("127.0.0.1", "localhost");
    let root = format!(
        "<!doctype html><title>Session fixture</title><iframe id='foreign' src='{foreign_url}/foreign'></iframe>"
    );
    let server = FixtureServer::start(&[
        ("/", "text/html; charset=utf-8", &root),
        (
            "/popup",
            "text/html; charset=utf-8",
            "<!doctype html><title>Persistent popup</title>",
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let artifacts = directory.path().join("artifacts");
    let mut session = Session::start(&artifacts);

    let first = session.command(json!({
        "id": "submit-1",
        "command": "submit",
        "flow": format!(
            "version: 1\nname: first\nsettings: {{ video: off }}\nsteps:\n  - open: {}/\n  - evaluate: {{ script: \"localStorage.setItem('kept', 'yes'); window.counter = 1; window.open('{}', 'persisted'); return window.counter\", save_as: counter }}\n  - switch_page: {{ name: persisted }}\n  - evaluate: {{ script: 'window.popupState = 41' }}\n  - switch_page: opener\n  - switch_frame: {{ target: {{ css: '#foreign' }} }}\n  - screenshot: {{ name: first }}\n",
            server.url,
            format!("{}/popup", server.url)
        )
    }));
    assert_eq!(first["ok"], true, "{first}");

    let inspection = session.command(json!({
        "id": "inspect",
        "command": "inspect",
        "accessibility": true,
        "screenshot": true
    }));
    assert_eq!(inspection["result"]["title"], "OOPIF fixture");
    assert!(
        inspection["result"]["active_frame"]
            .as_str()
            .is_some_and(|url| url.ends_with("/foreign"))
    );
    assert!(
        inspection["result"]["accessibility"]
            .to_string()
            .contains("Continue")
    );
    let inspection_path = PathBuf::from(inspection["result"]["screenshot"].as_str().unwrap());
    assert!(inspection_path.is_file());
    assert!(
        inspection_path
            .to_string_lossy()
            .contains("inspection-000001")
    );

    let second = session.command(json!({
        "id": "submit-2",
        "command": "submit",
        "flow": "version: 1\nname: second\nsettings: { video: off }\nsteps:\n  - click: { target: { role: { value: button, name: Continue } } }\n  - switch_frame: main\n  - evaluate: { script: \"return localStorage.getItem('kept') + window.counter\", save_as: root_state }\n  - switch_page: { name: persisted }\n  - evaluate: { script: 'return window.popupState + Number(args[0])', args: ['${counter}'], save_as: total }\n  - screenshot: { name: second }\n"
    }));
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(
        session.command(json!({ "id": "root", "command": "output", "name": "root_state" }))["result"]
            ["value"],
        "yes1"
    );
    assert_eq!(
        session.command(json!({ "id": "total", "command": "output", "name": "total" }))["result"]["value"],
        42
    );
    assert_eq!(
        session.command(json!({ "id": "close", "command": "close" }))["result"]["closed"],
        true
    );
    assert_exit(session.finish(), 0);

    let session_directory = only_entry(&artifacts);
    assert_eq!(read_report(&session_directory).flows.len(), 2);
    assert!(session_directory.join("submission-000001").is_dir());
    assert!(session_directory.join("submission-000002").is_dir());
    assert_ne!(
        inspection_path.parent(),
        Some(session_directory.join("submission-000001").as_path())
    );
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn malformed_validation_automation_and_settings_errors_recover() {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Recovery</title><p>ready</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));

    session.send_raw(b"not json\n");
    assert_eq!(session.read()["error"]["code"], "invalid_command");
    let mut oversized = vec![b' '; playrust::session_protocol::MAX_ENVELOPE_BYTES + 1];
    oversized.push(b'\n');
    session.send_raw(&oversized);
    assert_eq!(session.read()["error"]["code"], "envelope_too_large");
    assert_eq!(
        session.command(json!({ "id": "invalid", "command": "submit", "flow": "version: 1" }))["error"]
            ["code"],
        "validation"
    );
    assert_eq!(session.command(json!({
        "id": "open",
        "command": "submit",
        "flow": format!("version: 1\nname: open\nsettings: {{ video: off }}\nsteps: [{{ open: {}/ }}]\n", server.url)
    }))["ok"], true);
    assert_eq!(session.command(json!({
        "id": "conflict",
        "command": "submit",
        "flow": "version: 1\nname: conflict\nsettings: { video: off, viewport: { width: 800, height: 600 } }\nsteps: [{ open: https://example.test }]\n"
    }))["error"]["code"], "settings_conflict");
    assert_eq!(session.command(json!({
        "id": "automation",
        "command": "submit",
        "flow": "version: 1\nname: fail\nsettings: { video: off, timeout: 1s }\nsteps: [{ assert: { text: { target: { css: body }, equals: wrong } } }]\n"
    }))["error"]["code"], "submission_failed");
    assert_eq!(session.command(json!({
        "id": "clean",
        "command": "submit",
        "flow": "version: 1\nname: clean\nsettings: { video: off }\nsteps: [{ assert: { text: { target: { css: body }, equals: ready } } }]\n"
    }))["ok"], true);
    assert_eq!(
        session.command(json!({ "id": "close", "command": "close" }))["result"]["closed"],
        true
    );
    assert_exit(session.finish(), 0);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn busy_then_cancel_is_terminal_and_exits_130() {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><p>wait</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));
    session.send(json!({
        "id": "submit",
        "command": "submit",
        "flow": format!("version: 1\nname: wait\nsettings: {{ video: off, timeout: 30s }}\nsteps: [{{ open: {}/ }}, {{ wait_until_visible: {{ target: {{ css: '#never' }} }} }}]\n", server.url)
    }));
    session.send(json!({ "id": "busy", "command": "inspect" }));
    session.send(json!({ "id": "cancel", "command": "cancel" }));
    assert_eq!(session.read()["error"]["code"], "busy");
    assert_eq!(session.read()["result"]["cancelling"], true);
    let cancelled = session.read();
    assert_eq!(cancelled["id"], "submit");
    assert_eq!(cancelled["error"]["code"], "cancelled");
    assert_exit(session.finish(), 130);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn close_during_submit_orders_submit_before_the_only_close_response() {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><p>wait</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));
    session.send(json!({
        "id": "submit",
        "command": "submit",
        "flow": format!("version: 1\nname: wait\nsettings: {{ video: off, timeout: 30s }}\nsteps: [{{ open: {}/ }}, {{ wait_until_visible: {{ target: {{ css: '#never' }} }} }}]\n", server.url)
    }));
    session.send(json!({ "id": "close", "command": "close" }));
    let submit = session.read();
    let close = session.read();
    assert_eq!(submit["id"], "submit", "{submit}");
    assert_eq!(submit["error"]["code"], "cancelled");
    assert_eq!(close["id"], "close", "{close}");
    assert_eq!(close["result"]["closed"], true);
    assert_eq!(
        close["revision"].as_u64().unwrap(),
        submit["revision"].as_u64().unwrap() + 1
    );
    assert!(
        session.read_optional().is_none(),
        "received a second close response"
    );
    assert_exit(session.finish(), 130);
}

#[test]
fn fatal_browser_startup_exits_4() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args([
            "session",
            "--protocol",
            "ndjson",
            "--browser",
            directory.path().join("missing-chrome").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_exit(output, 4);
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
        self.send_raw(b"\n");
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        self.read_optional()
            .expect("session closed before responding")
    }

    fn read_optional(&mut self) -> Option<Value> {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        (!line.is_empty()).then(|| serde_json::from_str(&line).unwrap())
    }

    fn command(&mut self, value: Value) -> Value {
        self.send(value);
        self.read()
    }

    fn finish(mut self) -> Output {
        drop(self.stdin.take());
        self.child.wait_with_output().unwrap()
    }
}

fn assert_exit(output: Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn only_entry(path: &Path) -> PathBuf {
    std::fs::read_dir(path)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}
