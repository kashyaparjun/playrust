mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use support::{FixtureServer, chrome_env, chrome_path, require_browser, require_live_e2e};

#[test]
fn chrome_path_resolves_playrust_chrome_env() {
    let Some(chrome) = require_browser("chrome_path_resolves_playrust_chrome_env") else {
        return;
    };
    let (key, _value) = chrome_env(&chrome);
    let resolved = std::env::var_os(&key).map(PathBuf::from);
    assert_eq!(resolved.as_deref(), Some(chrome.as_path()));
    assert_eq!(chrome_path(), Some(chrome));
}

#[test]
fn require_browser_runs_minimal_session_snapshot_and_close() {
    let Some(chrome) = require_browser("require_browser_runs_minimal_session_snapshot_and_close")
    else {
        return;
    };
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Prerequisites</title><p id='status'>ready</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"), &chrome);

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
    assert!(
        snapshot["result"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|element| element["name"] == "ready")
    );

    let close = session.command(json!({ "id": "close", "command": "close" }));
    assert_eq!(close["ok"], true, "{close}");
    assert_eq!(session.finish().status.success(), true);
}

#[test]
fn require_live_e2e_skips_without_flag() {
    if std::env::var_os("PLAYRUST_LIVE_E2E").is_some_and(|value| {
        !matches!(
            value.to_str(),
            Some("0") | Some("false") | Some("no") | Some("")
        )
    }) {
        return;
    }
    let Some(()) = require_live_e2e("require_live_e2e_skips_without_flag") else {
        return;
    };
    panic!("require_live_e2e should skip when PLAYRUST_LIVE_E2E is unset");
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(artifacts: &Path, chrome: &Path) -> Self {
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
        serde_json::from_str(&line).expect("session response JSON")
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
