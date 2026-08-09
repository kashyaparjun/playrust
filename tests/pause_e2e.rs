mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use libtest_mimic::Failed;
use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use support::{FixtureServer, assert_h264_video, harness, video_duration};

const HTML: &str = "<!doctype html><html><body><h1>pause fixture</h1></body></html>";

fn main() {
    harness::run(vec![harness::async_browser_video_trial(
        "pause_adds_deliberate_dwell_time_to_a_recording",
        pause_adds_deliberate_dwell_time_to_a_recording,
    )]);
}

async fn pause_adds_deliberate_dwell_time_to_a_recording(
    chrome: PathBuf,
    ffmpeg: PathBuf,
) -> Result<(), Failed> {
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let host = BrowserHost::launch(chrome, false).await.unwrap();

    let baseline = compile_recording_flow(&server, "baseline", None);
    let paused = compile_recording_flow(&server, "paused", Some(Duration::from_millis(1500)));
    let baseline_artifacts = directory.path().join("baseline");
    let paused_artifacts = directory.path().join("paused");
    let baseline_report = run_flow(
        &host,
        &baseline,
        &RunOptions::new(&baseline_artifacts).with_ffmpeg(&ffmpeg),
    )
    .await;
    let paused_report = run_flow(
        &host,
        &paused,
        &RunOptions::new(&paused_artifacts).with_ffmpeg(&ffmpeg),
    )
    .await;
    assert_eq!(baseline_report.status, FlowStatus::Passed);
    assert_eq!(paused_report.status, FlowStatus::Passed);

    let baseline_recording = recording_path(&baseline_report.artifacts.recording);
    let paused_recording = recording_path(&paused_report.artifacts.recording);
    assert_h264_video(baseline_recording);
    assert_h264_video(paused_recording);
    let baseline_duration = video_duration(baseline_recording);
    let paused_duration = video_duration(paused_recording);
    assert!(
        paused_duration >= baseline_duration + Duration::from_secs(1),
        "paused recording {paused_duration:?} was not sufficiently longer than baseline {baseline_duration:?}"
    );

    host.shutdown().await.unwrap();
    Ok(())
}

fn compile_recording_flow(
    server: &FixtureServer,
    name: &str,
    pause: Option<Duration>,
) -> playrust::flow::CompiledFlow {
    let pause_step = pause.map_or_else(String::new, |duration| {
        format!("  - pause: {}ms\n", duration.as_millis())
    });
    compile_yaml(
        &format!(
            "version: 1\nname: {name}\nbase_url: {}\nsettings: {{ video: on }}\nsteps:\n  - open: /\n{pause_step}  - assert: {{ visible: {{ css: h1 }} }}\n",
            server.url()
        ),
        format!("{name}.yaml"),
        &BTreeMap::new(),
    )
    .unwrap()
}

fn recording_path(recording: &Option<String>) -> &Path {
    Path::new(recording.as_deref().expect("reported recording"))
}
