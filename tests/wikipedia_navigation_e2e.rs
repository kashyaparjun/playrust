mod support;

use std::fs;
use std::path::Path;

use playrust::report::FlowStatus;
use support::{assert_h264_video, assert_png, assert_success, ffmpeg_path, playrust, read_report};

const FLOW: &str = r##"version: 1
name: wikipedia-navigation
base_url: https://en.wikipedia.org
settings:
  timeout: 30s
  viewport: { width: 1280, height: 720 }
vars:
  article_path: { env: PLAYRUST_WIKI_ARTICLE }
  main_heading: { env: PLAYRUST_WIKI_MAIN_HEADING, default: Welcome to Wikipedia }
steps:
  - open: "${article_path}"
  - assert:
      visible: { css: "#firstHeading" }
  - click:
      target: { role: { value: link, name: Wikipedia The Free Encyclopedia } }
  - assert:
      url: { equals: https://en.wikipedia.org/wiki/Main_Page }
  - assert:
      visible: { text: "${main_heading}" }
  - screenshot:
      name: wikipedia-home
      crop: { x: 0, y: 0, width: 640, height: 360 }
"##;

#[test]
#[ignore = "requires Wikipedia network access, pinned Chromium, FFmpeg, and ffprobe"]
fn navigation_flow_uses_environment_defaults_and_cropped_artifacts() {
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("navigation.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, FLOW).expect("write navigation flow");
    let environment = [("PLAYRUST_WIKI_ARTICLE", "/wiki/Rust_(programming_language)")];
    let ffmpeg = ffmpeg_path();
    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--ffmpeg-path",
            &ffmpeg,
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &environment,
    );
    assert_success("run", &run);

    let report = read_report(&artifacts);
    let flow = &report.flows[0];
    assert_eq!(flow.status, FlowStatus::Passed);
    assert_png(Path::new(&flow.artifacts.screenshots[0]), (640, 360));
    assert_h264_video(Path::new(
        flow.artifacts.recording.as_deref().expect("recording"),
    ));
}
