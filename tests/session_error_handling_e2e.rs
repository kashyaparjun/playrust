mod support;

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use serde_json::{Value, json};
use support::FixtureServer;

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn malformed_act_and_missing_env_return_structured_errors_without_panic() {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Errors</title><label for='name'>Name</label><input id='name'>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));

    let missing_action = session.command(json!({ "id": "bad-act", "command": "act" }));
    assert_eq!(missing_action["ok"], false, "{missing_action}");
    assert_eq!(missing_action["error"]["code"], "invalid_command");

    let open = session.command(json!({
        "id": "open",
        "command": "act",
        "action": { "open": { "url": format!("{}/", server.url) } }
    }));
    assert_eq!(open["ok"], true, "{open}");

    let snapshot = session.command(json!({
        "id": "snapshot",
        "command": "snapshot",
        "accessibility": true
    }));
    assert_eq!(snapshot["ok"], true, "{snapshot}");
    let name_ref = snapshot["result"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == "Name")
        .expect("Name element")["ref"]
        .clone();

    let missing_env = session.command(json!({
        "id": "missing-env",
        "command": "act",
        "action": {
            "fill": {
                "ref": name_ref,
                "value": { "env": "PLAYRUST_TEST_MISSING_ENV_FOR_E2E" }
            }
        }
    }));
    assert_eq!(missing_env["ok"], false, "{missing_env}");
    assert_eq!(missing_env["error"]["code"], "validation");
    assert!(
        missing_env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("PLAYRUST_TEST_MISSING_ENV_FOR_E2E")
    );

    let stale_ref = session.command(json!({
        "id": "stale",
        "command": "act",
        "action": { "click": { "ref": "e999" } }
    }));
    assert_eq!(stale_ref["ok"], false, "{stale_ref}");
    assert_eq!(stale_ref["error"]["code"], "stale_reference");

    let close = session.command(json!({ "id": "close", "command": "close" }));
    assert_eq!(close["ok"], true, "{close}");
    assert_exit(session.finish(), 0);
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn invalid_transport_and_oversized_envelope_recover_without_process_panic() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));

    session.send_raw(b"not json\n");
    assert_eq!(session.read()["error"]["code"], "invalid_command");

    session.send_raw(b"{\"id\":\"utf8\",\"command\":\"cancel\",\"x\":\xff}\n");
    let invalid_utf8 = session.read();
    assert_eq!(invalid_utf8["id"], Value::Null);
    assert_eq!(invalid_utf8["error"]["code"], "invalid_command");

    let mut oversized = vec![b' '; playrust::session_protocol::MAX_ENVELOPE_BYTES + 1];
    oversized.push(b'\n');
    session.send_raw(&oversized);
    assert_eq!(session.read()["error"]["code"], "envelope_too_large");

    let snapshot = session.command(json!({ "id": "snapshot", "command": "snapshot" }));
    assert_eq!(snapshot["ok"], true, "{snapshot}");
    assert_eq!(snapshot["result"]["url"], "about:blank");

    let close = session.command(json!({ "id": "close", "command": "close" }));
    assert_eq!(close["ok"], true, "{close}");
    assert_exit(session.finish(), 0);
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
        self.send_raw(b"\n");
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("session response JSON")
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
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
