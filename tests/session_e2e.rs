mod support;

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{FixtureServer, read_report};

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

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn interactive_snapshot_refs_actions_scroll_and_dialog_recover() {
    const SECRET: &str = "session-secret-canary";
    unsafe { std::env::set_var("PLAYRUST_SESSION_TEST_SECRET", SECRET) };
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Interactive</title><button data-testid='confirm' onclick=\"confirm('Continue?')\">Continue</button><button id='replaceable' data-testid='replaceable'>Replaceable</button><input aria-label='Name' oninput='document.title=this.value'><div style='height:2000px'></div><script>setTimeout(() => { const old = document.querySelector('#replaceable'); const replacement = old.cloneNode(true); old.replaceWith(replacement); }, 2000)</script>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));

    assert_eq!(
        session.command(json!({
            "id": "open",
            "command": "act",
            "action": { "open": { "url": format!("{}/", server.url) } }
        }))["ok"],
        true
    );
    let snapshot = session.command(json!({
        "id": "snapshot",
        "command": "snapshot",
        "screenshot": "viewport"
    }));
    assert_eq!(snapshot["ok"], true, "{snapshot}");
    let button_ref = snapshot["result"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == "Continue")
        .unwrap()["ref"]
        .clone();
    let replaceable_ref = snapshot["result"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == "Replaceable")
        .unwrap()["ref"]
        .clone();
    std::thread::sleep(Duration::from_millis(2200));
    let detached = session.command(json!({
        "id": "detached",
        "command": "act",
        "action": { "click": { "ref": replaceable_ref } }
    }));
    assert_eq!(detached["error"]["code"], "stale_reference", "{detached}");
    let click = session.command(json!({
        "id": "click",
        "command": "act",
        "action": { "click": { "ref": button_ref } }
    }));
    assert_eq!(click["ok"], true, "{click}");
    assert_eq!(
        session.command(json!({ "id": "blocked", "command": "scroll", "y": 100 }))["error"]["code"],
        "dialog_pending"
    );
    assert_eq!(
        session.command(json!({ "id": "dialog", "command": "dialog", "action": "dismiss" }))["ok"],
        true
    );
    let after_dialog = session.command(json!({ "id": "after-dialog", "command": "snapshot" }));
    let input_ref = after_dialog["result"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == "Name")
        .unwrap()["ref"]
        .clone();
    let fill = session.command(json!({
        "id": "fill-secret",
        "command": "act",
        "action": { "fill": { "ref": input_ref, "value": { "env": "PLAYRUST_SESSION_TEST_SECRET" } } }
    }));
    assert_eq!(fill["ok"], true, "{fill}");
    assert!(!fill.to_string().contains(SECRET), "{fill}");
    assert_eq!(
        session.command(json!({ "id": "scroll", "command": "scroll", "y": 500 }))["ok"],
        true
    );
    let pause = session.command(json!({
        "id": "pause",
        "command": "act",
        "action": { "pause": "25ms" }
    }));
    assert_eq!(pause["ok"], true, "{pause}");
    assert!(pause["result"]["elapsed_ms"].as_u64().unwrap() >= 20);
    assert_eq!(
        session.command(json!({
            "id": "stale",
            "command": "act",
            "action": { "click": { "ref": button_ref } }
        }))["error"]["code"],
        "stale_reference"
    );
    let export = session.command(json!({
        "id": "export",
        "command": "export",
        "name": "interactive-test"
    }));
    assert_eq!(export["ok"], true, "{export}");
    assert_eq!(
        session.command(json!({ "id": "close", "command": "close" }))["ok"],
        true
    );
    assert_exit(session.finish(), 0);

    let bundle = directory.path().join("artifacts/interactive-test");
    let replay = std::fs::read_to_string(bundle.join("replay.yaml")).unwrap();
    let journal = std::fs::read_to_string(bundle.join("session.ndjson")).unwrap();
    assert!(replay.contains("PLAYRUST_SESSION_TEST_SECRET"));
    assert!(replay.contains("dialog:"));
    assert!(replay.contains("dismiss"));
    assert!(replay.contains("pause: '25ms'"));
    assert!(!replay.contains(SECRET));
    assert!(!journal.contains(SECRET));

    let replay_run = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args([
            "run",
            bundle.join("replay.yaml").to_str().unwrap(),
            "--browser",
            env::var_os("PLAYRUST_CHROME").unwrap().to_str().unwrap(),
            "--video",
            "off",
            "--artifacts",
            directory.path().join("replay-artifacts").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_exit(replay_run, 0);
    unsafe { std::env::remove_var("PLAYRUST_SESSION_TEST_SECRET") };
}

#[test]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
fn interactive_session_records_one_continuous_video() {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Recorded session</title><style>body{margin:0;height:2400px}.q{position:fixed;width:50vw;height:50vh}.tl{inset:0 auto auto 0;background:#f00}.tr{inset:0 0 auto auto;background:#0f0}.bl{inset:auto auto 0 0;background:#00f}.br{inset:auto 0 0 auto;background:#ff0}</style><div class='q tl'></div><div class='q tr'></div><div class='q bl'></div><div class='q br'></div>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let artifacts = directory.path().join("artifacts");
    let mut session = Session::start_recorded(&artifacts);

    assert_eq!(
        session.command(json!({
            "id": "open",
            "command": "act",
            "action": { "open": { "url": format!("{}/", server.url) } }
        }))["ok"],
        true
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        session.command(json!({
            "id": "snapshot",
            "command": "snapshot",
            "screenshot": "viewport"
        }))["ok"],
        true
    );
    assert_eq!(
        session.command(json!({ "id": "scroll", "command": "scroll", "y": 700 }))["ok"],
        true
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        session.command(json!({
            "id": "export",
            "command": "export",
            "name": "recorded-session"
        }))["ok"],
        true
    );
    let close = session.command(json!({ "id": "close", "command": "close" }));
    assert_eq!(close["ok"], true, "{close}");
    assert_eq!(close["result"]["recording"]["status"], "complete");
    assert_exit(session.finish(), 0);

    let recording = artifacts.join("recorded-session/recording.mp4");
    assert!(recording.is_file());
    assert!(recording.metadata().unwrap().len() > 1_000);
    let frames = directory.path().join("frames");
    std::fs::create_dir(&frames).unwrap();
    let extracted = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&recording)
        .arg(frames.join("%03d.png"))
        .output()
        .unwrap();
    assert_exit(extracted, 0);
    let full_viewport_frame = std::fs::read_dir(frames).unwrap().any(|entry| {
        let image = image::load_from_memory(&std::fs::read(entry.unwrap().path()).unwrap())
            .unwrap()
            .to_rgb8();
        if image.dimensions() != (1280, 720) {
            return false;
        }
        let near = |x, y, expected: [u8; 3]| {
            image
                .get_pixel(x, y)
                .0
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) < 30)
        };
        near(20, 20, [255, 0, 0])
            && near(1260, 20, [0, 255, 0])
            && near(20, 700, [0, 0, 255])
            && near(1260, 700, [255, 255, 0])
    });
    assert!(
        full_viewport_frame,
        "recording never showed all four viewport corners"
    );
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn large_page_snapshot_is_bounded() {
    let body = format!(
        "<!doctype html><title>Large snapshot</title><main>{}</main>",
        (0..400)
            .map(|index| format!("<a href='/{index}'>Link {index}</a>"))
            .collect::<String>()
    );
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", &body)]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&directory.path().join("artifacts"));

    assert_eq!(
        session.command(json!({
            "id": "open",
            "command": "act",
            "action": { "open": { "url": format!("{}/", server.url) } }
        }))["ok"],
        true
    );
    let started = Instant::now();
    let snapshot = session.command(json!({ "id": "snapshot", "command": "snapshot" }));

    assert_eq!(snapshot["ok"], true, "{snapshot}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "large-page snapshot took {:?}",
        started.elapsed()
    );
    assert!(snapshot["result"]["truncation"]["truncated"] == true);
    assert!(snapshot["result"]["elements"].as_array().unwrap().len() <= 250);
    let link_ref = snapshot["result"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == "Link 100")
        .unwrap()["ref"]
        .clone();
    assert_eq!(
        session.command(json!({
            "id": "click",
            "command": "act",
            "action": { "click": { "ref": link_ref } }
        }))["ok"],
        true
    );
    assert_eq!(
        session.command(json!({ "id": "close", "command": "close" }))["ok"],
        true
    );
    assert_exit(session.finish(), 0);
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(artifacts: &Path) -> Self {
        Self::start_with_video(artifacts, "off")
    }

    fn start_recorded(artifacts: &Path) -> Self {
        Self::start_with_video(artifacts, "on")
    }

    fn start_with_video(artifacts: &Path, video: &str) -> Self {
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
                video,
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

    fn close_input(&mut self) {
        drop(self.stdin.take());
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

fn only_entry(path: &Path) -> PathBuf {
    std::fs::read_dir(path)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}
