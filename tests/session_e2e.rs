mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use libtest_mimic::Failed;
use serde_json::{Value, json};
use support::{FixtureServer, harness, read_report};

fn main() {
    harness::run(vec![
        harness::browser_trial(
            "state_storage_tab_frame_inspection_and_artifacts_persist",
            state_storage_tab_frame_inspection_and_artifacts_persist,
        ),
        harness::browser_trial(
            "malformed_validation_automation_and_settings_errors_recover",
            malformed_validation_automation_and_settings_errors_recover,
        ),
        harness::browser_trial(
            "busy_then_cancel_is_terminal_and_exits_130",
            busy_then_cancel_is_terminal_and_exits_130,
        ),
        harness::browser_trial(
            "close_during_submit_orders_submit_before_the_only_close_response",
            close_during_submit_orders_submit_before_the_only_close_response,
        ),
        harness::browser_trial(
            "cancellation_ack_wins_a_racing_success",
            cancellation_ack_wins_a_racing_success,
        ),
        harness::browser_trial(
            "eof_during_submission_cancels_and_exits_130",
            eof_during_submission_cancels_and_exits_130,
        ),
        harness::browser_trial(
            "artifact_failure_wins_cancellation",
            artifact_failure_wins_cancellation,
        ),
        harness::plain_trial(
            "fatal_browser_startup_exits_4",
            fatal_browser_startup_exits_4,
        ),
        harness::browser_trial(
            "interactive_snapshot_refs_actions_scroll_and_dialog_recover",
            interactive_snapshot_refs_actions_scroll_and_dialog_recover,
        ),
        harness::browser_video_cli_trial(
            "interactive_session_records_one_continuous_video",
            interactive_session_records_one_continuous_video,
        ),
        harness::browser_trial(
            "large_page_snapshot_is_bounded",
            large_page_snapshot_is_bounded,
        ),
    ]);
}

fn state_storage_tab_frame_inspection_and_artifacts_persist(chrome: PathBuf) -> Result<(), Failed> {
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
    let mut session = Session::start(&chrome, &artifacts);

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
    let snapshot = session.command(json!({ "id": "snapshot", "command": "snapshot" }));
    let button = snapshot["result"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == "Continue")
        .unwrap();
    assert!(button["bounds"]["x"].as_f64().unwrap() >= 15.0, "{button}");

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
    Ok(())
}

fn malformed_validation_automation_and_settings_errors_recover(
    chrome: PathBuf,
) -> Result<(), Failed> {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Recovery</title><p>ready</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));

    assert_eq!(
        session.command(json!({ "id": "inspect-early", "command": "inspect" }))["result"]["url"],
        "about:blank"
    );
    assert_eq!(
        session.command(json!({ "id": "cancel-idle", "command": "cancel" }))["error"]["code"],
        "not_active"
    );
    assert_eq!(
        session.command(json!({ "id": "missing", "command": "output", "name": "absent" }))["error"]
            ["code"],
        "output_not_found"
    );
    session.send_raw(b"not json\n");
    assert_eq!(session.read()["error"]["code"], "invalid_command");
    session.send_raw(b"{\"id\":\"utf8\",\"command\":\"cancel\",\"x\":\xff}\n");
    let invalid_utf8 = session.read();
    assert_eq!(invalid_utf8["id"], Value::Null);
    assert_eq!(invalid_utf8["error"]["code"], "invalid_command");
    session.send_raw(b"{\"id\":\"crlf\",\"command\":\"cancel\"}\r\n");
    assert_eq!(session.read()["error"]["code"], "not_active");
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
    let script = session.command(json!({
        "id": "script",
        "command": "submit",
        "flow": "version: 1\nname: script\nsettings: { video: off }\nsteps: [{ evaluate: { script: 'throw new Error(\"expected\")' } }]\n"
    }));
    assert_eq!(script["error"]["code"], "submission_failed");
    assert_eq!(
        script["error"]["details"]["failures"][0]["category"],
        "script"
    );
    let request = session.command(json!({
        "id": "request",
        "command": "submit",
        "flow": "version: 1\nname: request\nsettings: { video: off }\nsteps: [{ request: { method: GET, url: 'http://127.0.0.1:1/unavailable', expected_status: 200 } }]\n"
    }));
    assert_eq!(request["error"]["code"], "submission_failed");
    assert_eq!(
        request["error"]["details"]["failures"][0]["category"],
        "request"
    );
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
    Ok(())
}

fn busy_then_cancel_is_terminal_and_exits_130(chrome: PathBuf) -> Result<(), Failed> {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><p>wait</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));
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
    Ok(())
}

fn close_during_submit_orders_submit_before_the_only_close_response(
    chrome: PathBuf,
) -> Result<(), Failed> {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><p>wait</p>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));
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
    Ok(())
}

fn cancellation_ack_wins_a_racing_success(chrome: PathBuf) -> Result<(), Failed> {
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", "<!doctype html>")]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));
    session.send(json!({
        "id": "submit",
        "command": "submit",
        "flow": format!("version: 1\nname: race\nsettings: {{ video: off }}\nsteps: [{{ open: {}/ }}, {{ evaluate: {{ script: 'return new Promise(resolve => setTimeout(() => resolve(1), 100))', save_as: result }} }}]\n", server.url)
    }));
    session.send(json!({ "id": "cancel", "command": "cancel" }));
    assert_eq!(session.read()["result"]["cancelling"], true);
    let submit = session.read();
    assert_eq!(submit["id"], "submit");
    assert_eq!(submit["error"]["code"], "cancelled");
    assert_eq!(submit["error"]["details"]["status"], "interrupted");
    assert_exit(session.finish(), 130);
    Ok(())
}

fn eof_during_submission_cancels_and_exits_130(chrome: PathBuf) -> Result<(), Failed> {
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", "<!doctype html>")]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));
    session.send(json!({
        "id": "submit",
        "command": "submit",
        "flow": format!("version: 1\nname: eof\nsettings: {{ video: off, timeout: 30s }}\nsteps: [{{ open: {}/ }}, {{ wait_until_visible: {{ target: {{ css: '#never' }} }} }}]\n", server.url)
    }));
    session.close_input();
    assert_eq!(session.read()["error"]["code"], "cancelled");
    assert_exit(session.finish(), 130);
    Ok(())
}

fn artifact_failure_wins_cancellation(chrome: PathBuf) -> Result<(), Failed> {
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", "<!doctype html>")]);
    let directory = tempfile::tempdir().unwrap();
    let artifacts = directory.path().join("not-a-directory");
    std::fs::write(&artifacts, "blocked").unwrap();
    let mut session = Session::start(&chrome, &artifacts);
    session.send(json!({
        "id": "submit",
        "command": "submit",
        "flow": format!("version: 1\nname: artifact\nsettings: {{ video: off, timeout: 30s }}\nsteps: [{{ open: {}/ }}, {{ wait_until_visible: {{ target: {{ css: '#never' }} }} }}]\n", server.url)
    }));
    session.send(json!({ "id": "cancel", "command": "cancel" }));
    assert_eq!(session.read()["result"]["cancelling"], true);
    assert_eq!(session.read()["error"]["code"], "artifacts");
    assert_exit(session.finish(), 4);
    Ok(())
}

fn fatal_browser_startup_exits_4() -> Result<(), Failed> {
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
    Ok(())
}

fn interactive_snapshot_refs_actions_scroll_and_dialog_recover(
    chrome: PathBuf,
) -> Result<(), Failed> {
    const SECRET: &str = "session-secret-canary";
    unsafe { std::env::set_var("PLAYRUST_SESSION_TEST_SECRET", SECRET) };
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Interactive</title><button data-testid='confirm' onclick=\"confirm('Continue?')\">Continue</button><button id='replaceable' data-testid='replaceable'>Replaceable</button><input aria-label='Name' oninput='document.title=this.value'><div style='height:2000px'></div><script>setTimeout(() => { const old = document.querySelector('#replaceable'); const replacement = old.cloneNode(true); old.replaceWith(replacement); }, 2000)</script>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));

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
            chrome.to_str().unwrap(),
            "--video",
            "off",
            "--artifacts",
            directory.path().join("replay-artifacts").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_exit(replay_run, 0);
    unsafe { std::env::remove_var("PLAYRUST_SESSION_TEST_SECRET") };
    Ok(())
}

fn interactive_session_records_one_continuous_video(
    chrome: PathBuf,
    ffmpeg: PathBuf,
) -> Result<(), Failed> {
    let server = FixtureServer::start(&[(
        "/",
        "text/html; charset=utf-8",
        "<!doctype html><title>Recorded session</title><style>body{margin:0;height:2400px}.q{position:fixed;width:50vw;height:50vh}.tl{inset:0 auto auto 0;background:#f00}.tr{inset:0 0 auto auto;background:#0f0}.bl{inset:auto auto 0 0;background:#00f}.br{inset:auto 0 0 auto;background:#ff0}</style><div class='q tl'></div><div class='q tr'></div><div class='q bl'></div><div class='q br'></div>",
    )]);
    let directory = tempfile::tempdir().unwrap();
    let artifacts = directory.path().join("artifacts");
    let mut session = Session::start_recorded(&chrome, &artifacts);

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
    let extracted = Command::new(&ffmpeg)
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
    Ok(())
}

fn large_page_snapshot_is_bounded(chrome: PathBuf) -> Result<(), Failed> {
    let body = format!(
        "<!doctype html><title>Large snapshot</title><main>{}</main>",
        (0..400)
            .map(|index| format!("<a href='/{index}'>Link {index}</a>"))
            .collect::<String>()
    );
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", &body)]);
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(&chrome, &directory.path().join("artifacts"));

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
    Ok(())
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(chrome: &Path, artifacts: &Path) -> Self {
        Self::start_with_video(chrome, artifacts, "off")
    }

    fn start_recorded(chrome: &Path, artifacts: &Path) -> Self {
        Self::start_with_video(chrome, artifacts, "on")
    }

    fn start_with_video(chrome: &Path, artifacts: &Path, video: &str) -> Self {
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
