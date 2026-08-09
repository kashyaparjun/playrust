mod support;

use std::fs;

use support::{FixtureServer, playrust, read_report};

const FIXTURE: &str = r#"<!doctype html>
<html><body>
<label for="secret">Secret</label>
<input id="secret">
</body></html>"#;

#[test]
fn secret_screenshot_warning_is_reported_without_secret_text() {
    let Some(chrome) =
        support::require_browser("secret_screenshot_warning_is_reported_without_secret_text")
    else {
        return;
    };
    let chrome_env = support::chrome_env(&chrome);
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", FIXTURE)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("secret.yaml");
    let artifacts = directory.path().join("artifacts");
    let secret = "e2e-canary-secret";
    fs::write(
        &flow,
        format!(
            "version: 1\nname: secret-recording\nbase_url: {}\nsecrets: {{ token: {{ env: TOKEN }} }}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - fill: {{ target: {{ css: '#secret' }}, value: '${{token}}' }}\n  - screenshot: {{ name: captured }}\n",
            server.url()
        ),
    )
    .expect("write flow fixture");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[("TOKEN", secret), (&chrome_env.0, &chrome_env.1)],
    );
    server.shutdown();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("warning: video or screenshots may capture secret-derived"),
        "{stdout}"
    );
    assert!(
        !stdout.contains(secret),
        "secret leaked to stdout: {stdout}"
    );

    let report_json = fs::read_to_string(artifacts.join("report.json")).expect("read report");
    assert!(report_json.contains("video or screenshots may capture secret-derived"));
    assert!(
        !report_json.contains(secret),
        "secret leaked to report: {report_json}"
    );
    let report = read_report(&artifacts);
    assert_eq!(report.flows[0].warnings.len(), 1);
    assert!(!format!("{report:?}").contains(secret));
}

#[test]
fn secret_fill_without_visual_capture_does_not_warn() {
    let Some(chrome) = support::require_browser("secret_fill_without_visual_capture_does_not_warn")
    else {
        return;
    };
    let chrome_env = support::chrome_env(&chrome);
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", FIXTURE)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("secret-off.yaml");
    let artifacts = directory.path().join("artifacts");
    let secret = "e2e-canary-secret";
    fs::write(
        &flow,
        format!(
            "version: 1\nname: secret-off\nbase_url: {}\nsecrets: {{ token: {{ env: TOKEN }} }}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - fill: {{ target: {{ css: '#secret' }}, value: '${{token}}' }}\n",
            server.url()
        ),
    )
    .expect("write flow fixture");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[("TOKEN", secret), (&chrome_env.0, &chrome_env.1)],
    );
    server.shutdown();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("warning: video or screenshots"),
        "unexpected warning: {stdout}"
    );
    assert!(read_report(&artifacts).flows[0].warnings.is_empty());
}
