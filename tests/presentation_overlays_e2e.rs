mod support;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use playrust::browser::BrowserHost;
use playrust::flow::{compile_yaml, compile_yaml_with_env};
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use support::{FixtureServer, ffmpeg_path};

const SECRET: &str = "presentation-overlay-secret";
const HTML: &str = r#"<!doctype html><html><body style="min-height:2000px">
<h1>Overlay fixture</h1>
<button id="action">Act</button>
</body></html>"#;
const ROUTES: &[(&str, &str, &str)] = &[
    ("/", "text/html", HTML),
    (
        "/secret?token=presentation-overlay-secret",
        "text/html",
        HTML,
    ),
];

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
async fn presentation_overlays_redact_text_and_mark_clicks_and_scrolls() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
    let server = FixtureServer::start(ROUTES);
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let flow = compile_yaml_with_env(
        &format!(
            r#"version: 1
name: presentation-overlay-markers
base_url: {}
settings:
  video: on
  overlays: {{ step: true, url: true, pointer: true }}
secrets:
  token: {{ env: TOKEN }}
steps:
  - id: {SECRET}
    open: /secret?token={SECRET}
  - evaluate:
      script: |
        const button = document.querySelector('#action');
        const rect = button.getBoundingClientRect();
        const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
        if (hit !== button) throw new Error('presentation overlay blocks the click target');
  - click: {{ target: {{ css: '#action' }} }}
  - scroll: {{ y: 200 }}
  - id: verify-{SECRET}
    evaluate:
      script: |
        const host = document.querySelector('playrust-presentation-overlay');
        const overlay = host?.shadowRoot;
        if (!overlay) throw new Error('presentation overlay is missing');
        if (overlay.textContent.includes(args[0])) throw new Error('presentation overlay leaked its secret');
        if (!overlay.textContent.includes('[REDACTED]')) throw new Error('presentation overlay did not render redaction');
        if (!overlay.querySelector('[data-marker="click"]')) throw new Error('click marker is missing');
        if (!overlay.querySelector('[data-marker="scroll"]')) throw new Error('scroll marker is missing');
      args: ['${{token}}']
"#,
            server.url
        ),
        "presentation-overlay-markers.yaml",
        &BTreeMap::new(),
        &BTreeMap::from([("TOKEN".to_owned(), SECRET.to_owned())]),
    )
    .unwrap();

    let report = run_flow(
        &host,
        &flow,
        &RunOptions::new(directory.path()).with_ffmpeg(ffmpeg_path()),
    )
    .await;
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_recording(&report.artifacts.recording);
    assert_video_contains_synchronized_markers(
        Path::new(report.artifacts.recording.as_deref().unwrap()),
        directory.path(),
    );
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
async fn presentation_overlays_are_recording_only_and_do_not_change_screenshots() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
    let server = FixtureServer::start(ROUTES);
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let compile_recording_flow = |name: &str, video: &str, overlays: &str, extra_steps: &str| {
        compile_yaml(
            &format!(
                "version: 1\nname: {name}\nbase_url: {}\nsettings:\n  video: {video}\n  overlays: {overlays}\nsteps:\n  - open: /\n{extra_steps}",
                server.url
            ),
            format!("{name}.yaml"),
            &BTreeMap::new(),
        )
        .unwrap()
    };

    let no_video = run_flow(
        &host,
        &compile_recording_flow(
            "no-video",
            "off",
            "{ step: true, url: true, pointer: true }",
            "  - evaluate: { script: \"if (document.querySelector('playrust-presentation-overlay, #playrust-presentation-overlay')) throw new Error('overlay exists without recording')\" }\n",
        ),
        &RunOptions::new(directory.path().join("no-video")),
    )
    .await;
    assert_eq!(
        no_video.status,
        FlowStatus::Passed,
        "{:#?}",
        no_video.failures
    );

    let plain = run_flow(
        &host,
        &compile_recording_flow("plain", "on", "{}", "  - screenshot: { name: page }\n"),
        &RunOptions::new(directory.path().join("plain")).with_ffmpeg(ffmpeg_path()),
    )
    .await;
    let overlay = run_flow(
        &host,
        &compile_recording_flow(
            "overlay",
            "on",
            "{ step: true, url: true, pointer: true }",
            "  - screenshot: { name: page }\n",
        ),
        &RunOptions::new(directory.path().join("overlay")).with_ffmpeg(ffmpeg_path()),
    )
    .await;
    assert_eq!(plain.status, FlowStatus::Passed, "{:#?}", plain.failures);
    assert_eq!(
        overlay.status,
        FlowStatus::Passed,
        "{:#?}",
        overlay.failures
    );
    assert_eq!(
        fs::read(&plain.artifacts.screenshots[0]).unwrap(),
        fs::read(&overlay.artifacts.screenshots[0]).unwrap(),
        "presentation overlays must not change screenshot artifacts"
    );
    host.shutdown().await.unwrap();
}

fn assert_recording(recording: &Option<String>) {
    let path = Path::new(recording.as_deref().expect("reported recording"));
    assert!(path.exists());
    assert!(path.metadata().unwrap().len() > 0);
}

fn assert_video_contains_synchronized_markers(recording: &Path, directory: &Path) {
    let frames = directory.join("decoded-overlay-frames");
    fs::create_dir(&frames).unwrap();
    let output = Command::new(ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(recording)
        .args(["-vf", "fps=15"])
        .arg(frames.join("frame-%03d.png"))
        .output()
        .expect("decode presentation overlay frames");
    assert!(
        output.status.success(),
        "decode presentation overlay frames: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut decoded = fs::read_dir(frames)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    decoded.sort();
    let marker_frames = decoded
        .into_iter()
        .map(|frame| {
            let image = image::open(frame).unwrap().to_rgb8();
            let click = image.pixels().any(|pixel| {
                pixel[1] > 140 && pixel[1] > pixel[0].saturating_add(30) && pixel[2] < 150
            });
            let scroll = image
                .pixels()
                .any(|pixel| pixel[0] > 180 && pixel[1] > 150 && pixel[2] < 100);
            (click, scroll)
        })
        .collect::<Vec<_>>();
    let click_frame = marker_frames
        .iter()
        .position(|(click, _)| *click)
        .expect("decoded recording did not contain the green click marker");
    let scroll_frame = marker_frames
        .iter()
        .position(|(_, scroll)| *scroll)
        .expect("decoded recording did not contain the yellow scroll marker");
    assert!(
        click_frame < scroll_frame,
        "click marker frame {click_frame} must precede scroll marker frame {scroll_frame}"
    );
}
