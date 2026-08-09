mod support;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use playrust::report::{AggregateStatus, FlowStatus};
use support::{
    FixtureServer, assert_h264_video, assert_png, assert_success, ffmpeg_path, playrust,
    read_report,
};

const HTML: &str =
    r#"<!doctype html><html><body><h1>Fixture</h1><button id="ready">Ready</button></body></html>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
fn html_report_embeds_fixture_screenshot_and_recording_with_relative_media_links() {
    let chrome = env::var("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME");
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("viewer.yaml");
    let artifacts = PathBuf::from(format!(
        "playrust-artifacts-html-e2e-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(artifacts.clone());
    assert!(!artifacts.is_absolute());
    let source = format!(
        "version: 1\nname: html-viewer\nbase_url: {}\nsettings: {{ timeout: 30s, video: on, viewport: {{ width: 1280, height: 720 }} }}\nsteps:\n  - open: /\n  - assert: {{ visible: {{ css: '#ready' }} }}\n  - screenshot: {{ name: fixture }}\n",
        server.url
    );
    fs::write(&flow, source).expect("write fixture flow");

    let ffmpeg = ffmpeg_path();
    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--ffmpeg-path",
            &ffmpeg,
            "--artifacts",
            artifacts.to_str().unwrap(),
            "--html",
        ],
        &[("PLAYRUST_CHROME", chrome.as_str())],
    );
    assert_success("run", &run);

    let report = read_report(&artifacts);
    assert_eq!(report.status, AggregateStatus::Passed);
    assert_eq!(report.flows[0].status, FlowStatus::Passed);
    let flow_report = &report.flows[0];
    let screenshot = Path::new(&flow_report.artifacts.screenshots[0]);
    let recording = Path::new(
        flow_report
            .artifacts
            .recording
            .as_deref()
            .expect("recording"),
    );
    assert_png(screenshot, (1280, 720));
    assert_h264_video(recording);

    let html_report = fs::read_to_string(artifacts.join("report.html")).expect("read HTML report");
    let screenshot_relative = relative_to_artifacts(screenshot, &artifacts);
    let recording_relative = relative_to_artifacts(recording, &artifacts);
    assert!(
        html_report.contains(&format!(
            "<img class=\"artifact-preview\" src=\"{screenshot_relative}\""
        )),
        "{html_report}"
    );
    assert!(
        html_report.contains(&format!(
            "<source src=\"{recording_relative}\" type=\"video/mp4\">"
        )),
        "{html_report}"
    );
    assert!(
        html_report.contains(&format!("href=\"{recording_relative}\">Open</a>")),
        "{html_report}"
    );
    assert!(
        !html_report.contains(&format!("<a href=\"{recording_relative}\"><video")),
        "{html_report}"
    );
    assert!(!html_report.contains(screenshot.to_str().unwrap()));
    assert!(!html_report.contains(recording.to_str().unwrap()));

    server.shutdown();
}

fn relative_to_artifacts(path: &Path, artifacts: &Path) -> String {
    let cwd = env::current_dir().expect("cwd");
    let abs_artifacts = if artifacts.is_absolute() {
        artifacts.to_path_buf()
    } else {
        cwd.join(artifacts)
    };
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    abs_path
        .strip_prefix(&abs_artifacts)
        .unwrap_or_else(|_| panic!("{} under {}", abs_path.display(), abs_artifacts.display()))
        .to_string_lossy()
        .replace('\\', "/")
}
