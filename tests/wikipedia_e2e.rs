use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use playrust::report::{AggregateReport, AggregateStatus, FlowStatus};

const SEARCH_FLOW: &str = r##"version: 1
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
  - assert:
      visible: { role: { value: heading, name: Rust (programming language) } }
  - double_click:
      target: { css: "#firstHeading" }
  - clear: cookies
  - clear: storage
  - screenshot:
      name: rust-article
"##;

const NAVIGATION_FLOW: &str = r##"version: 1
name: wikipedia-navigation
base_url: https://en.wikipedia.org
settings:
  timeout: 30s
  viewport: { width: 1280, height: 720 }
vars:
  query: overridden by the CLI
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

const FAILURE_FLOW: &str = r##"version: 1
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
#[ignore = "requires Wikipedia network access, pinned Chromium, FFmpeg, and ffprobe"]
fn wikipedia_exercises_the_full_cli_and_artifact_contract() {
    let ffmpeg = env::var_os("PLAYRUST_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let ffprobe = env::var_os("PLAYRUST_FFPROBE").unwrap_or_else(|| "ffprobe".into());
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flows = directory.path().join("flows");
    let artifacts = directory.path().join("artifacts");
    fs::create_dir(&flows).expect("create flow directory");
    fs::write(flows.join("01-search.yaml"), SEARCH_FLOW).expect("write search flow");
    fs::write(flows.join("02-navigation.yaml"), NAVIGATION_FLOW).expect("write navigation flow");

    let flow_environment = [("PLAYRUST_WIKI_ARTICLE", "/wiki/Rust_(programming_language)")];
    let checked = playrust(
        &[
            "check",
            flows.to_str().unwrap(),
            "--var",
            "query=Rust (programming language)",
        ],
        &flow_environment,
    );
    assert_success("check", &checked);

    let run = playrust(
        &[
            "run",
            flows.to_str().unwrap(),
            "--var",
            "query=Rust (programming language)",
            "--ffmpeg-path",
            ffmpeg.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
            "--jobs",
            "2",
            "--junit",
        ],
        &flow_environment,
    );
    assert_success("run", &run);

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("PASS wikipedia-search"), "{stdout}");
    assert!(stdout.contains("PASS wikipedia-navigation"), "{stdout}");
    assert_eq!(stdout.matches("  Recording: ").count(), 2, "{stdout}");

    let report: AggregateReport =
        serde_json::from_slice(&fs::read(artifacts.join("report.json")).expect("read JSON report"))
            .expect("decode JSON report");
    assert_eq!(report.status, AggregateStatus::Passed);
    assert_eq!(report.exit_code, 0);
    assert_eq!(report.flows.len(), 2);
    assert_eq!(report.flows[0].name, "wikipedia-search");
    assert_eq!(report.flows[1].name, "wikipedia-navigation");

    for flow in &report.flows {
        assert_eq!(flow.status, FlowStatus::Passed, "{:#?}", flow.failures);
        assert!(flow.failures.is_empty());
        assert_eq!(flow.artifacts.screenshots.len(), 1);
        let recording = Path::new(
            flow.artifacts
                .recording
                .as_deref()
                .expect("default recording path"),
        );
        assert!(recording.is_file(), "missing {}", recording.display());
        assert!(fs::metadata(recording).unwrap().len() > 0);
        assert_vp9_video(&ffprobe, recording);
    }

    assert_png(
        Path::new(&report.flows[0].artifacts.screenshots[0]),
        (1280, 720),
    );
    assert_png(
        Path::new(&report.flows[1].artifacts.screenshots[0]),
        (640, 360),
    );

    let junit = fs::read_to_string(artifacts.join("junit.xml")).expect("read JUnit report");
    assert!(junit.contains("tests=\"2\""), "{junit}");
    assert!(junit.contains("failures=\"0\""), "{junit}");
    assert!(junit.contains("errors=\"0\""), "{junit}");

    assert_failure_artifacts(directory.path(), &ffmpeg, &ffprobe);
}

fn assert_failure_artifacts(root: &Path, ffmpeg: &std::ffi::OsStr, ffprobe: &std::ffi::OsStr) {
    let secret = "#playrust-wikipedia-secret-canary";
    let flow = root.join("failure.yaml");
    let artifacts = root.join("failure-artifacts");
    fs::write(&flow, FAILURE_FLOW).expect("write failing flow");
    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--ffmpeg-path",
            ffmpeg.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
            "--junit",
        ],
        &[("PLAYRUST_WIKI_SECRET", secret)],
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

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

    let report: AggregateReport =
        serde_json::from_slice(&report_bytes).expect("decode failure report");
    assert_eq!(report.flows[0].status, FlowStatus::Failed);
    let screenshot = Path::new(
        report.flows[0]
            .artifacts
            .failure_screenshot
            .as_deref()
            .expect("failure screenshot"),
    );
    assert_png(screenshot, (1280, 720));
    let recording = Path::new(
        report.flows[0]
            .artifacts
            .recording
            .as_deref()
            .expect("retained failure recording"),
    );
    assert_vp9_video(ffprobe, recording);
}

fn playrust(arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_playrust"));
    command
        .args(arguments)
        .envs(environment.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    output_with_timeout(command, Duration::from_secs(180))
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> Output {
    let mut child = command.spawn().expect("run playrust");
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll playrust").is_some() {
            return child.wait_with_output().expect("collect playrust output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed out output");
            panic!(
                "playrust timed out\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_success(command: &str, output: &Output) {
    assert!(
        output.status.success(),
        "playrust {command} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_png(path: &Path, expected: (u32, u32)) {
    let png = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        (
            u32::from_be_bytes(png[16..20].try_into().unwrap()),
            u32::from_be_bytes(png[20..24].try_into().unwrap()),
        ),
        expected,
    );
}

fn assert_vp9_video(ffprobe: &std::ffi::OsStr, path: &Path) {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert_success("ffprobe", &output);
    let metadata = String::from_utf8_lossy(&output.stdout);
    assert!(metadata.contains("codec_name=vp9"), "{metadata}");
    assert!(metadata.contains("width=1280"), "{metadata}");
    assert!(metadata.contains("height=720"), "{metadata}");
}
