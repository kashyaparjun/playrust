mod support;

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::install::{self, PINNED_FFMPEG_VERSION, Platform};
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use playrust::video::{VideoConfig, preflight_ffmpeg};
use support::{FixtureServer, assert_h264_video, assert_success, playrust};

const VIDEO_HTML: &str = r#"<!doctype html>
<html lang="en">
  <body>
    <p id="status">frame 0</p>
    <script>
      let frame = 0;
      setInterval(() => {
        frame += 1;
        document.querySelector('#status').textContent = `frame ${frame}`;
      }, 50);
    </script>
  </body>
</html>"#;

fn provisioning_e2e_enabled() -> bool {
    env::var_os("PLAYRUST_FFMPEG_E2E").is_some_and(|value| {
        !matches!(
            value.to_str(),
            Some("0") | Some("false") | Some("no") | Some("")
        )
    })
}

fn require_chrome(test_name: &str) -> Option<PathBuf> {
    env::var_os(install::CHROME_ENV)
        .map(PathBuf::from)
        .filter(|path| {
            if path.is_file() {
                return true;
            }
            eprintln!("SKIP {test_name}: set PLAYRUST_CHROME to a pinned Chrome executable");
            false
        })
}

#[test]
fn ffmpeg_install_cli_is_available() {
    if !provisioning_e2e_enabled() {
        eprintln!(
            "SKIP ffmpeg_install_cli_is_available: set PLAYRUST_FFMPEG_E2E=1 to run FFmpeg provisioning e2e"
        );
        return;
    }
    let output = playrust(&["ffmpeg", "install"], &[]);
    assert_success("ffmpeg install", &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("FFmpeg") || stdout.contains("ffmpeg"),
        "{stdout}"
    );

    let root = install::ffmpeg_cache_root().expect("ffmpeg cache root");
    let platform = Platform::current().expect("supported platform");
    let cached =
        install::cached_ffmpeg_path(&root, PINNED_FFMPEG_VERSION, platform).expect("cache path");
    assert!(
        cached.is_file(),
        "missing provisioned ffmpeg at {}",
        cached.display()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_flow_records_h264_with_provisioned_ffmpeg_only() {
    if !provisioning_e2e_enabled() {
        eprintln!(
            "SKIP run_flow_records_h264_with_provisioned_ffmpeg_only: set PLAYRUST_FFMPEG_E2E=1"
        );
        return;
    }
    let Some(chrome) = require_chrome("run_flow_records_h264_with_provisioned_ffmpeg_only") else {
        return;
    };

    let install_output = playrust(&["ffmpeg", "install"], &[]);
    assert_success("ffmpeg install", &install_output);

    let root = install::ffmpeg_cache_root().expect("ffmpeg cache root");
    let platform = Platform::current().expect("supported platform");
    let ffmpeg =
        install::cached_ffmpeg_path(&root, PINNED_FFMPEG_VERSION, platform).expect("cache path");
    assert!(ffmpeg.is_file());

    let server = FixtureServer::start(&[("/", "text/html", VIDEO_HTML)]);
    let flow = compile_yaml(
        &format!(
            r##"version: 1
name: ffmpeg-provision-smoke
base_url: http://{}
settings:
  timeout: 10s
  viewport: {{ width: 800, height: 600 }}
  video: on
steps:
  - open: /
  - pause: 500ms
"##,
            server.address
        ),
        "ffmpeg-provision-smoke.yaml",
        &BTreeMap::new(),
    )
    .expect("compile ffmpeg provision smoke flow");

    let artifacts = tempfile::tempdir().expect("artifact directory");
    preflight_ffmpeg(&VideoConfig {
        mode: flow.settings.video,
        ffmpeg_path: ffmpeg.clone(),
        output_path: artifacts.path().join("recording.mp4"),
        viewport_width: flow.settings.viewport.width,
        viewport_height: flow.settings.viewport.height,
    })
    .await
    .expect("preflight provisioned ffmpeg");

    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");
    let report = run_flow(
        &host,
        &flow,
        &RunOptions::new(artifacts.path()).with_ffmpeg(&ffmpeg),
    )
    .await;
    host.shutdown().await.expect("shutdown Chrome");

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    let recording = PathBuf::from(report.artifacts.recording.expect("recording path"));
    assert_h264_video(&recording);
}

#[test]
fn playrust_run_auto_resolves_provisioned_ffmpeg_when_video_enabled() {
    if !provisioning_e2e_enabled() {
        eprintln!(
            "SKIP playrust_run_auto_resolves_provisioned_ffmpeg_when_video_enabled: set PLAYRUST_FFMPEG_E2E=1"
        );
        return;
    }
    let Some(chrome) =
        require_chrome("playrust_run_auto_resolves_provisioned_ffmpeg_when_video_enabled")
    else {
        return;
    };

    assert_success("ffmpeg install", &playrust(&["ffmpeg", "install"], &[]));

    let server = FixtureServer::start(&[("/", "text/html", VIDEO_HTML)]);
    let directory = tempfile::tempdir().unwrap();
    let flow = directory.path().join("video.yaml");
    std::fs::write(
        &flow,
        format!(
            "version: 1\nname: auto-ffmpeg\nbase_url: http://{}\nsettings:\n  video: on\n  viewport: {{ width: 800, height: 600 }}\nsteps:\n  - open: /\n  - pause: 500ms\n",
            server.address
        ),
    )
    .unwrap();
    let artifacts = directory.path().join("artifacts");

    let stripped_path = if cfg!(windows) {
        "C:\\Windows\\System32;C:\\Windows"
    } else {
        "/usr/bin:/bin"
    };

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--browser",
            chrome.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[
            (install::CHROME_ENV, chrome.to_string_lossy().as_ref()),
            ("PATH", stripped_path),
            ("PLAYRUST_NO_SANDBOX", "1"),
        ],
    );
    assert_success("run with auto-resolved ffmpeg", &output);

    let report = support::read_report(&artifacts);
    assert_eq!(report.flows[0].status, FlowStatus::Passed);
    let recording = report.flows[0]
        .artifacts
        .recording
        .as_ref()
        .expect("recording artifact");
    assert_h264_video(PathBuf::from(recording).as_path());
}
