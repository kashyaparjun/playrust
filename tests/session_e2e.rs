mod support;

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::{FixtureServer, read_report};

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn persistent_session_submits_inspects_reuses_output_and_closes() {
    let foreign = FixtureServer::start(&[(
        "/foreign",
        "text/html; charset=utf-8",
        "<!doctype html><title>OOPIF fixture</title><button onclick=\"document.querySelector('#status').textContent='continued'\">Continue</button><p id='status'>pending</p>",
    )]);
    let foreign_url = foreign.url().replace("127.0.0.1", "localhost");
    let root = format!(
        "<!doctype html><title>Session fixture</title><iframe id='foreign' src='{foreign_url}/foreign'></iframe>"
    );
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", &root)]);
    let directory = tempfile::tempdir().expect("create session E2E directory");
    let artifacts = directory.path().join("artifacts");
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
        .expect("start session");
    let mut stdin = child.stdin.take().expect("session stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("session stdout"));

    let first = command(
        &mut stdin,
        &mut stdout,
        json!({
            "id": "submit-1",
            "command": "submit",
            "flow": format!(
                "version: 1\nname: first\nsettings: {{ video: off }}\nsteps:\n  - open: {}/\n  - evaluate: {{ script: 'window.counter = 1; return window.counter', save_as: counter }}\n  - switch_frame: {{ target: {{ css: '#foreign' }} }}\n",
                server.url
            )
        }),
    );
    assert_eq!(first["ok"], true, "{first}");

    let inspection = command(
        &mut stdin,
        &mut stdout,
        json!({ "id": "inspect", "command": "inspect", "accessibility": true }),
    );
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

    let second = command(
        &mut stdin,
        &mut stdout,
        json!({
            "id": "submit-2",
            "command": "submit",
            "flow": "version: 1\nname: second\nsettings: { video: off }\nsteps:\n  - click: { target: { role: { value: button, name: Continue } } }\n  - assert: { text: { target: { css: '#status' }, equals: continued } }\n  - switch_frame: main\n  - evaluate: { script: 'window.counter += Number(args[0]); return window.counter', args: ['${counter}'], save_as: total }\n"
        }),
    );
    assert_eq!(second["ok"], true, "{second}");

    let output = command(
        &mut stdin,
        &mut stdout,
        json!({ "id": "output", "command": "output", "name": "total" }),
    );
    assert_eq!(output["result"]["value"], 2);
    let closed = command(
        &mut stdin,
        &mut stdout,
        json!({ "id": "close", "command": "close" }),
    );
    assert_eq!(closed["result"]["closed"], true);
    assert!(closed["revision"].as_u64().unwrap() > first["revision"].as_u64().unwrap());

    drop(stdin);
    let output = child.wait_with_output().expect("wait for session");
    assert!(
        output.status.success(),
        "session failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session_directory = std::fs::read_dir(&artifacts)
        .expect("read artifact root")
        .next()
        .expect("session artifact directory")
        .expect("read session artifact entry")
        .path();
    assert_eq!(read_report(&session_directory).flows.len(), 2);
}

fn command(stdin: &mut impl Write, stdout: &mut impl BufRead, value: Value) -> Value {
    serde_json::to_writer(&mut *stdin, &value).expect("write command");
    stdin.write_all(b"\n").expect("terminate command");
    stdin.flush().expect("flush command");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read response");
    assert!(!line.is_empty(), "session closed before responding");
    serde_json::from_str(&line).expect("decode response")
}
