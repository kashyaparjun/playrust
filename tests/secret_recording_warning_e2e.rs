mod support;

use std::fs;
use std::path::PathBuf;

use support::{FixtureServer, ffmpeg_path, playrust, read_report};

const FIXTURE: &str = r#"<!doctype html>
<html><body>
<label for="secret">Secret</label>
<input id="secret">
</body></html>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
fn secret_recording_warning_is_reported_without_secret_text() {
    let chrome = PathBuf::from(
        std::env::var_os("PLAYRUST_CHROME")
            .expect("set PLAYRUST_CHROME to the pinned Chrome executable"),
    );
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", FIXTURE)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("secret.yaml");
    let artifacts = directory.path().join("artifacts");
    let secret = "e2e-canary-secret";
    fs::write(
        &flow,
        format!(
            "version: 1\nname: secret-recording\nbase_url: {}\nsecrets: {{ token: {{ env: TOKEN }} }}\nsteps:\n  - open: /\n  - fill: {{ target: {{ css: '#secret' }}, value: '${{token}}' }}\n  - screenshot: {{ name: captured }}\n",
            server.url()
        ),
    )
    .expect("write flow fixture");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--browser",
            chrome.to_str().unwrap(),
            "--ffmpeg-path",
            &ffmpeg_path(),
            "--jobs",
            "1",
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[("TOKEN", secret)],
    );
    server.shutdown();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("WARNING: video or screenshots"), "{stdout}");
    assert!(
        !stdout.contains(secret),
        "secret leaked to stdout: {stdout}"
    );

    let report_json = fs::read_to_string(artifacts.join("report.json")).expect("read report");
    assert!(report_json.contains("WARNING: video or screenshots"));
    assert!(
        !report_json.contains(secret),
        "secret leaked to report: {report_json}"
    );
    let report = read_report(&artifacts);
    assert_eq!(report.flows[0].warnings.len(), 1);
    assert!(!format!("{report:?}").contains(secret));
}
