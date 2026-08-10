mod support;

use serde_json::{Value, json};
use support::{FixtureServer, Session, assert_exit};

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

    // Never-issued refs are unknown_reference; stale applies only after invalidation.
    let unknown_ref = session.command(json!({
        "id": "unknown",
        "command": "act",
        "action": { "click": { "ref": "e999" } }
    }));
    assert_eq!(unknown_ref["ok"], false, "{unknown_ref}");
    assert_eq!(unknown_ref["error"]["code"], "unknown_reference");

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
