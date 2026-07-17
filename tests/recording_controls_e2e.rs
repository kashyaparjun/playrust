mod support;

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{CancellationToken, RunOptions, run_flow};
use playrust::video::{VideoConfig, preflight_ffmpeg};
use support::{FixtureServer, ffmpeg_path};

const HTML: &str = r#"<!doctype html><html><body><button id="change" onclick="document.body.style.background='blue'">change</button></body></html>"#;
const ROUTES: &[(&str, &str, &str)] = &[("/", "text/html", HTML)];

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
async fn manual_recording_finalizes_on_stop_failure_and_cancellation() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
    let ffmpeg = PathBuf::from(ffmpeg_path());
    let server = FixtureServer::start(ROUTES);
    let host = BrowserHost::launch(chrome, false).await.unwrap();

    for (name, video, middle, expected, has_recording) in [
        (
            "stopped",
            "on",
            "  - click: { target: { css: '#change' } }\n  - recording: stop\n",
            FlowStatus::Passed,
            true,
        ),
        (
            "passing-retain-on-failure",
            "retain-on-failure",
            "  - click: { target: { css: '#change' } }\n  - recording: stop\n",
            FlowStatus::Passed,
            false,
        ),
        (
            "failed-before-stop",
            "retain-on-failure",
            "  - assert: { visible: { css: '#missing' } }\n  - recording: stop\n",
            FlowStatus::Failed,
            true,
        ),
        (
            "failed-after-stop",
            "retain-on-failure",
            "  - click: { target: { css: '#change' } }\n  - recording: stop\n  - assert: { visible: { css: '#missing' } }\n",
            FlowStatus::Failed,
            true,
        ),
    ] {
        let source = format!(
            "version: 1\nname: {name}\nbase_url: {}\nsettings: {{ timeout: 300ms, video: {video} }}\nsteps:\n  - open: /\n  - recording: start\n{middle}",
            server.url
        );
        let flow = compile_yaml(&source, format!("{name}.yaml"), &BTreeMap::new()).unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        preflight(&flow, &ffmpeg, artifacts.path()).await;
        let report = run_flow(
            &host,
            &flow,
            &RunOptions::new(artifacts.path()).with_ffmpeg(&ffmpeg),
        )
        .await;
        assert_eq!(report.status, expected, "{:#?}", report.failures);
        if has_recording {
            assert_recording(&report.artifacts.recording);
        } else {
            assert!(report.artifacts.recording.is_none());
        }
    }

    let source = format!(
        "version: 1\nname: skipped\nbase_url: {}\nsettings: {{ video: on }}\nvars: {{ mode: disabled }}\nsteps:\n  - open: /\n  - when: {{ variable: {{ name: mode, equals: enabled }} }}\n    recording: start\n  - when: {{ variable: {{ name: mode, equals: enabled }} }}\n    recording: stop\n",
        server.url
    );
    let flow = compile_yaml(&source, "skipped.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let report = run_flow(
        &host,
        &flow,
        &RunOptions::new(artifacts.path()).with_ffmpeg(&ffmpeg),
    )
    .await;
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert!(report.artifacts.recording.is_none());

    let source = format!(
        "version: 1\nname: cancelled\nbase_url: {}\nsettings: {{ timeout: 10s, video: retain-on-failure }}\nsteps:\n  - open: /\n  - recording: start\n  - assert: {{ visible: {{ css: '#missing' }} }}\n  - recording: stop\n",
        server.url
    );
    let flow = compile_yaml(&source, "cancelled.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    preflight(&flow, &ffmpeg, artifacts.path()).await;
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
    });
    let report = run_flow(
        &host,
        &flow,
        &RunOptions::new(artifacts.path())
            .with_ffmpeg(&ffmpeg)
            .with_cancellation(cancellation),
    )
    .await;
    assert_eq!(
        report.status,
        FlowStatus::Interrupted,
        "{:#?}",
        report.failures
    );
    assert_recording(&report.artifacts.recording);

    host.shutdown().await.unwrap();
}

async fn preflight(flow: &playrust::flow::CompiledFlow, ffmpeg: &Path, directory: &Path) {
    preflight_ffmpeg(&VideoConfig {
        mode: flow.settings.video,
        ffmpeg_path: ffmpeg.to_owned(),
        output_path: directory.join("recording.webm"),
        viewport_width: flow.settings.viewport.width,
        viewport_height: flow.settings.viewport.height,
    })
    .await
    .unwrap();
}

fn assert_recording(recording: &Option<String>) {
    let path = Path::new(recording.as_deref().expect("reported recording"));
    assert!(path.exists());
    assert!(path.metadata().unwrap().len() > 0);
}
