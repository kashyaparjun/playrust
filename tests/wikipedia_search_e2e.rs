mod support;

use std::fs;
use std::path::Path;

use libtest_mimic::Failed;
use playrust::report::{AggregateStatus, FlowStatus};
use support::{
    assert_h264_video, assert_png, assert_success, ffmpeg_path, harness, playrust, read_report,
};

const FLOW: &str = r##"version: 1
name: wikipedia-search
base_url: https://en.wikipedia.org
settings:
  timeout: 30s
  viewport: { width: 1280, height: 720 }
vars:
  query: overridden by the CLI
steps:
  - open: /wiki/Main_Page?useskin=vector-2022
  - assert:
      visible: { role: { value: heading, name: Welcome to Wikipedia } }
  - assert:
      hidden: { css: "#playrust-element-that-does-not-exist" }
  - fill:
      target: { label: Search Wikipedia }
      value: "${query}"
  - press:
      target: { label: Search Wikipedia }
      key: a
      modifiers: [Control]
  - fill:
      target: { label: Search Wikipedia }
      value: "${query}"
  - press:
      target: { label: Search Wikipedia }
      key: Escape
  - click:
      target: { role: { value: button, name: Search } }
  - assert:
      url: { path: /wiki/Rust_(programming_language) }
  - assert:
      text:
        target: { css: "#firstHeading" }
        equals: Rust (programming language)
  - assert:
      text:
        target: { css: "#firstHeading" }
        contains: programming language
  - double_click:
      target: { css: "#firstHeading" }
  - clear: cookies
  - clear: storage
  - screenshot:
      name: rust-article
"##;

fn main() {
    harness::run(vec![harness::live_wikipedia_trial(
        "search_flow_exercises_interactions_assertions_and_default_video",
        search_flow_exercises_interactions_assertions_and_default_video,
    )]);
}

fn search_flow_exercises_interactions_assertions_and_default_video() -> Result<(), Failed> {
    let ffmpeg = ffmpeg_path();
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("search.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, FLOW).expect("write search flow");

    let checked = playrust(
        &[
            "check",
            flow.to_str().unwrap(),
            "--var",
            "query=Rust (programming language)",
        ],
        &[],
    );
    assert_success("check", &checked);

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--var",
            "query=Rust (programming language)",
            "--ffmpeg-path",
            &ffmpeg,
            "--artifacts",
            artifacts.to_str().unwrap(),
            "--junit",
        ],
        &[],
    );
    assert_success("run", &run);

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("PASS wikipedia-search"), "{stdout}");
    assert!(stdout.contains("  Recording: "), "{stdout}");

    let report = read_report(&artifacts);
    assert_eq!(report.status, AggregateStatus::Passed);
    assert_eq!(report.flows.len(), 1);
    let flow = &report.flows[0];
    assert_eq!(flow.status, FlowStatus::Passed);
    assert_png(Path::new(&flow.artifacts.screenshots[0]), (1280, 720));
    assert_h264_video(Path::new(
        flow.artifacts.recording.as_deref().expect("recording"),
    ));

    let junit = fs::read_to_string(artifacts.join("junit.xml")).expect("read JUnit report");
    assert!(junit.contains("tests=\"1\""), "{junit}");
    assert!(junit.contains("failures=\"0\""), "{junit}");
    Ok(())
}
