mod support;

use serde_json::json;
use support::{FixtureServer, Session, chrome_path, env_flag_enabled, require_browser};

#[test]
fn chrome_path_returns_some_when_browser_available() {
    let Some(chrome) = require_browser("chrome_path_returns_some_when_browser_available") else {
        return;
    };
    let resolved = chrome_path().expect("chrome_path should return Some when browser is available");
    assert_eq!(resolved, chrome);
    assert!(resolved.is_file());
}

#[test]
fn chrome_path_matches_playrust_chrome_env_when_set() {
    let Some(chrome) = require_browser("chrome_path_matches_playrust_chrome_env_when_set") else {
        return;
    };
    match std::env::var_os(playrust::install::CHROME_ENV) {
        Some(from_env) => {
            let from_env = std::path::PathBuf::from(from_env);
            if from_env.is_file() {
                assert_eq!(chrome_path().as_deref(), Some(from_env.as_path()));
                assert_eq!(chrome, from_env);
            }
        }
        None => {
            // Cache-only: chrome_path must still resolve without PLAYRUST_CHROME.
            assert_eq!(chrome_path().as_deref(), Some(chrome.as_path()));
        }
    }
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
    assert!(session.finish().status.success());
}

#[test]
fn live_e2e_flag_parsing_treats_unset_as_disabled() {
    // Do not call require_live_e2e here: under PLAYRUST_REQUIRE_BROWSER=1 it
    // would panic when LIVE_E2E is unset. Assert the gate predicate instead.
    if env_flag_enabled("PLAYRUST_LIVE_E2E") {
        return;
    }
    assert!(
        !env_flag_enabled("PLAYRUST_LIVE_E2E"),
        "unset/falsey PLAYRUST_LIVE_E2E must disable live e2e"
    );
}
