mod support;

use std::fs;
use std::time::Duration;

use playrust::report::FlowStatus;
use support::{
    FixtureServer, assert_h264_video, assert_success, playrust, read_report, video_duration,
};

const HTML: &str = "<!doctype html><html><body><h1>pause fixture</h1></body></html>";

#[test]
#[ignore = "requires PLAYRUST_CHROME, FFmpeg, and ffprobe"]
fn pause_adds_deliberate_dwell_time_to_a_flow() {
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("pause.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: pause-e2e\nbase_url: {}\nsettings: {{ video: on }}\nsteps:\n  - open: /\n  - pause: 500ms\n  - assert: {{ visible: {{ css: h1 }} }}\n",
            server.url()
        ),
    )
    .expect("write flow");

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);

    let report = read_report(&artifacts);
    assert_eq!(report.flows[0].status, FlowStatus::Passed);
    assert!(report.flows[0].duration_ms >= Duration::from_millis(450).as_millis() as u64);
    let recording = report.flows[0]
        .artifacts
        .recording
        .as_deref()
        .expect("pause recording");
    let recording = std::path::Path::new(recording);
    assert_h264_video(recording);
    assert!(video_duration(recording) >= Duration::from_millis(450));
}
