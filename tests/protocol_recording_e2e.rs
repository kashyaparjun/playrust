mod support;

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use playrust::video::{VideoConfig, preflight_ffmpeg};
use support::FixtureServer;

const VIDEO_HTML: &str = r#"<!doctype html><html><body><p id="status">recording</p></body></html>"#;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
async fn yaml_run_and_session_share_recording_lifecycle() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
    let ffmpeg = env::var_os("PLAYRUST_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let server = FixtureServer::start(&[("/", "text/html", VIDEO_HTML)]);

    let flow = compile_yaml(
        &format!(
            "version: 1\nname: flow-recording\nbase_url: http://{}\nsettings:\n  video: on\n  viewport: {{ width: 640, height: 480 }}\nsteps:\n  - open: /\n  - pause: 200ms\n",
            server.address
        ),
        "flow-recording.yaml",
        &BTreeMap::new(),
    )
    .unwrap();
    let run_artifacts = tempfile::tempdir().unwrap();
    preflight_ffmpeg(&VideoConfig {
        mode: flow.settings.video,
        ffmpeg_path: ffmpeg.clone(),
        output_path: run_artifacts.path().join("preflight.mp4"),
        viewport_width: flow.settings.viewport.width,
        viewport_height: flow.settings.viewport.height,
    })
    .await
    .unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();
    let run_report = run_flow(
        &host,
        &flow,
        &RunOptions::new(run_artifacts.path()).with_ffmpeg(&ffmpeg),
    )
    .await;
    host.shutdown().await.unwrap();
    assert_eq!(
        run_report.status,
        FlowStatus::Passed,
        "{:#?}",
        run_report.failures
    );
    assert!(run_report.artifacts.recording.is_some());

    let directory = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args([
            "session",
            "--protocol",
            "ndjson",
            "--browser",
            chrome.to_str().unwrap(),
            "--artifacts",
            directory.path().join("artifacts").to_str().unwrap(),
            "--video",
            "on",
            "--ffmpeg-path",
            ffmpeg.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let mut stdin = child.stdin.take().unwrap();
    for line in [
        r#"{"id":1,"command":"act","action":{"open":{"url":"about:blank"}}}"#,
        r#"{"id":2,"command":"act","action":{"pause":"100ms"}}"#,
        r#"{"id":3,"command":"close"}"#,
    ] {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    stdin.flush().unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"ok\":true"), "{stdout}");
    assert!(stdout.contains("recording"), "{stdout}");
}
