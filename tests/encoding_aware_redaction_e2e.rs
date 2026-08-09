mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, playrust, read_report};

const FIXTURE: &str = r#"<!doctype html><html><body><h1>ready</h1></body></html>"#;

/// Space is percent-encoded in navigation URLs; other characters stay literal in query values.
const SECRET: &str = "ab cd";

fn percent_encoded_spaces(secret: &str) -> String {
    secret.replace(' ', "%20")
}

#[test]
fn check_rejects_short_declared_secrets_without_leaking_value() {
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("short-secret.yaml");
    fs::write(
        &flow,
        "version: 1\nname: short-secret\nsecrets: { token: { env: TOKEN } }\nsteps: [{ open: https://example.test }]\n",
    )
    .expect("write flow");

    let output = playrust(&["check", flow.to_str().unwrap()], &[("TOKEN", "ab")]);
    let diagnostics = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !output.status.success(),
        "short secret should fail check: {diagnostics}"
    );
    assert!(diagnostics.contains("token"), "{diagnostics}");
    assert!(
        diagnostics.contains("4") || diagnostics.to_lowercase().contains("four"),
        "{diagnostics}"
    );
    assert!(
        !diagnostics.contains("ab"),
        "short secret value leaked: {diagnostics}"
    );
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn encoding_aware_redaction_hides_raw_and_percent_encoded_secrets_in_diagnostics() {
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", FIXTURE)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("encoding-redaction.yaml");
    let artifacts = directory.path().join("artifacts");
    let encoded = percent_encoded_spaces(SECRET);
    fs::write(
        &flow,
        format!(
            "version: 1\nname: encoding-redaction\nbase_url: {}\nsettings: {{ video: off }}\nsecrets: {{ token: {{ env: TOKEN }} }}\nsteps:\n  - open: /?q=${{token}}\n  - evaluate: {{ script: 'throw new Error(args[0])', args: ['${{token}}'] }}\n",
            server.url()
        ),
    )
    .expect("write flow");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[("TOKEN", SECRET)],
    );
    server.shutdown();

    assert_eq!(output.status.code(), Some(3));
    let diagnostics = format!(
        "{}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(artifacts.join("report.json")).expect("read report"),
    );
    assert!(
        !diagnostics.contains(SECRET),
        "raw secret leaked: {diagnostics}"
    );
    assert!(
        !diagnostics.contains(&encoded),
        "percent-encoded secret leaked ({encoded}): {diagnostics}"
    );
    assert!(diagnostics.contains("[REDACTED]"), "{diagnostics}");

    let report = read_report(&artifacts);
    assert_eq!(report.flows[0].status, FlowStatus::Failed);
    assert!(!format!("{report:?}").contains(SECRET));
}

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn short_runtime_string_outputs_are_not_over_redacted_in_diagnostics() {
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", FIXTURE)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("short-runtime.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: short-runtime\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - evaluate: {{ script: 'return \"ab\"', save_as: short }}\n  - evaluate: {{ script: 'throw new Error(\"cab\")' }}\n",
            server.url()
        ),
    )
    .expect("write flow");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();

    assert_eq!(output.status.code(), Some(3));
    let diagnostics = format!(
        "{}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(artifacts.join("report.json")).expect("read report"),
    );
    assert!(
        !diagnostics.contains("c[REDACTED]"),
        "short runtime output over-redacted unrelated text: {diagnostics}"
    );
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Failed);
}
