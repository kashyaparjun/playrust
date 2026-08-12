mod support;

use std::fs;
use std::path::Path;

use playrust::report::FlowStatus;
use support::{assert_h264_video, assert_png, ffmpeg_path, playrust, read_report};

const FLOW: &str = r##"version: 1
name: wikipedia-redaction-failure
base_url: https://en.wikipedia.org
settings:
  timeout: 2s
  viewport: { width: 1280, height: 720 }
  video: retain-on-failure
secrets:
  missing_target: { env: PLAYRUST_WIKI_SECRET }
steps:
  - open: /wiki/Rust_(programming_language)
  - click:
      target: { css: "${missing_target}" }
"##;

#[test]
fn failed_flow_redacts_secrets_and_retains_debug_artifacts() {
    let Some(()) =
        support::require_live_e2e("failed_flow_redacts_secrets_and_retains_debug_artifacts")
    else {
        return;
    };
    let Some(chrome) =
        support::require_browser("failed_flow_redacts_secrets_and_retains_debug_artifacts")
    else {
        return;
    };
    let Some(_ffmpeg) =
        support::require_ffmpeg("failed_flow_redacts_secrets_and_retains_debug_artifacts")
    else {
        return;
    };
    let Some(_ffprobe) =
        support::require_ffprobe("failed_flow_redacts_secrets_and_retains_debug_artifacts")
    else {
        return;
    };
    let chrome_env = support::chrome_env(&chrome);
    let secret = "#playrust-wikipedia-secret-canary";
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("failure.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, FLOW).expect("write failing flow");
    let ffmpeg = ffmpeg_path();
    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--ffmpeg-path",
            &ffmpeg,
            "--artifacts",
            artifacts.to_str().unwrap(),
            "--junit",
        ],
        &[
            (&chrome_env.0, &chrome_env.1),
            ("PLAYRUST_WIKI_SECRET", secret),
        ],
    );
    assert_eq!(output.status.code(), Some(3));

    let report_bytes = fs::read(artifacts.join("report.json")).expect("read failure report");
    let junit = fs::read_to_string(artifacts.join("junit.xml")).expect("read failure JUnit");
    let diagnostics = format!(
        "{}\n{}\n{}\n{junit}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&report_bytes),
    );
    assert!(
        !diagnostics.contains(secret),
        "secret leaked: {diagnostics}"
    );
    assert!(diagnostics.contains("[REDACTED]"), "{diagnostics}");
    assert!(junit.contains("failures=\"1\""), "{junit}");

    let report = read_report(&artifacts);
    let flow = &report.flows[0];
    assert_eq!(flow.status, FlowStatus::Failed);
    assert_png(
        Path::new(
            flow.artifacts
                .failure_screenshot
                .as_deref()
                .expect("failure screenshot"),
        ),
        (1280, 720),
    );
    assert_h264_video(Path::new(
        flow.artifacts.recording.as_deref().expect("recording"),
    ));
}
