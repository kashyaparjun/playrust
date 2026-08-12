mod support;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::install::{self, PINNED_FFMPEG_VERSION, Platform};
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use playrust::video::{VideoConfig, preflight_ffmpeg};
use support::{FixtureServer, assert_success, playrust};

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
    support::require_browser(test_name)
}

struct ProvisionedFfmpeg {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

/// Opt-in guard + install + cached path resolution shared by provisioning e2e tests.
fn require_provisioned_ffmpeg(test_name: &str) -> Option<ProvisionedFfmpeg> {
    if !provisioning_e2e_enabled() {
        eprintln!("SKIP {test_name}: set PLAYRUST_FFMPEG_E2E=1 to run FFmpeg provisioning e2e");
        return None;
    }
    assert_success("ffmpeg install", &playrust(&["ffmpeg", "install"], &[]));
    let root = install::ffmpeg_cache_root().expect("ffmpeg cache root");
    let platform = Platform::current().expect("supported platform");
    let ffmpeg =
        install::cached_ffmpeg_path(&root, PINNED_FFMPEG_VERSION, platform).expect("cache path");
    let ffprobe =
        install::cached_ffprobe_path(&root, PINNED_FFMPEG_VERSION, platform).expect("ffprobe path");
    assert!(
        ffmpeg.is_file(),
        "missing provisioned ffmpeg at {}",
        ffmpeg.display()
    );
    assert!(
        ffprobe.is_file(),
        "missing provisioned ffprobe at {}",
        ffprobe.display()
    );
    Some(ProvisionedFfmpeg { ffmpeg, ffprobe })
}

fn assert_h264_with_ffprobe(ffprobe: &Path, video: &Path) {
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
        .arg(video)
        .output()
        .expect("run ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let metadata = String::from_utf8_lossy(&output.stdout);
    assert!(metadata.contains("codec_name=h264"), "{metadata}");
    assert!(metadata.contains("width=800"), "{metadata}");
    assert!(metadata.contains("height=600"), "{metadata}");
}

fn path_hiding_system_ffmpeg(shadow_dir: &Path) -> String {
    fs::create_dir_all(shadow_dir).expect("create shadow bin dir");
    let fake_name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let fake = shadow_dir.join(fake_name);
    fs::write(
        &fake,
        b"#!/bin/sh\necho 'shadowed system ffmpeg' >&2\nexit 127\n",
    )
    .expect("write shadowed ffmpeg");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake, permissions).unwrap();
    }
    let cargo_bin = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .map(|root| root.join("bin"))
        .filter(|path| path.is_dir());
    let mut entries = vec![shadow_dir.to_path_buf()];
    if let Some(cargo_bin) = cargo_bin {
        entries.push(cargo_bin);
    }
    // Keep a minimal PATH without /usr/bin so system ffmpeg is not discovered.
    entries.push(PathBuf::from("/bin"));
    env::join_paths(entries)
        .expect("join PATH")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn ffmpeg_install_cli_is_available() {
    let Some(provisioned) = require_provisioned_ffmpeg("ffmpeg_install_cli_is_available") else {
        return;
    };
    let output = Command::new(&provisioned.ffmpeg)
        .args(["-hide_banner", "-version"])
        .output()
        .expect("run provisioned ffmpeg");
    assert!(output.status.success());
}

#[tokio::test(flavor = "current_thread")]
async fn run_flow_records_h264_with_provisioned_ffmpeg_only() {
    let Some(provisioned) =
        require_provisioned_ffmpeg("run_flow_records_h264_with_provisioned_ffmpeg_only")
    else {
        return;
    };
    let Some(chrome) = require_chrome("run_flow_records_h264_with_provisioned_ffmpeg_only") else {
        return;
    };

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
        ffmpeg_path: provisioned.ffmpeg.clone(),
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
        &RunOptions::new(artifacts.path()).with_ffmpeg(&provisioned.ffmpeg),
    )
    .await;
    host.shutdown().await.expect("shutdown Chrome");

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    let recording = PathBuf::from(report.artifacts.recording.expect("recording path"));
    assert_h264_with_ffprobe(&provisioned.ffprobe, &recording);
}

#[test]
fn playrust_run_auto_resolves_provisioned_ffmpeg_when_video_enabled() {
    let Some(provisioned) = require_provisioned_ffmpeg(
        "playrust_run_auto_resolves_provisioned_ffmpeg_when_video_enabled",
    ) else {
        return;
    };
    let Some(chrome) =
        require_chrome("playrust_run_auto_resolves_provisioned_ffmpeg_when_video_enabled")
    else {
        return;
    };

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
    let shadow = directory.path().join("shadow-bin");
    let path = path_hiding_system_ffmpeg(&shadow);

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
            ("PATH", &path),
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
    assert_h264_with_ffprobe(&provisioned.ffprobe, PathBuf::from(recording).as_path());
}

#[test]
fn playrust_run_auto_installs_ffmpeg_into_empty_cache() {
    if !provisioning_e2e_enabled() {
        eprintln!(
            "SKIP playrust_run_auto_installs_ffmpeg_into_empty_cache: set PLAYRUST_FFMPEG_E2E=1"
        );
        return;
    }
    let Some(chrome) = require_chrome("playrust_run_auto_installs_ffmpeg_into_empty_cache") else {
        return;
    };

    let server = FixtureServer::start(&[("/", "text/html", VIDEO_HTML)]);
    let directory = tempfile::tempdir().unwrap();
    let cache_home = directory.path().join("xdg-cache");
    fs::create_dir_all(&cache_home).unwrap();
    let flow = directory.path().join("video.yaml");
    std::fs::write(
        &flow,
        format!(
            "version: 1\nname: first-run-ffmpeg\nbase_url: http://{}\nsettings:\n  video: on\n  viewport: {{ width: 800, height: 600 }}\nsteps:\n  - open: /\n  - pause: 500ms\n",
            server.address
        ),
    )
    .unwrap();
    let artifacts = directory.path().join("artifacts");
    let shadow = directory.path().join("shadow-bin");
    let path = path_hiding_system_ffmpeg(&shadow);
    let cache_home_str = cache_home.to_string_lossy().into_owned();

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
            ("PATH", &path),
            ("PLAYRUST_NO_SANDBOX", "1"),
            ("XDG_CACHE_HOME", &cache_home_str),
        ],
    );
    assert_success("run with first-run ffmpeg auto-install", &output);

    let platform = Platform::current().expect("supported platform");
    let root = cache_home.join("playrust").join("ffmpeg");
    let ffmpeg =
        install::cached_ffmpeg_path(&root, PINNED_FFMPEG_VERSION, platform).expect("cache path");
    let ffprobe =
        install::cached_ffprobe_path(&root, PINNED_FFMPEG_VERSION, platform).expect("ffprobe path");
    assert!(
        ffmpeg.is_file(),
        "expected auto-installed ffmpeg at {}",
        ffmpeg.display()
    );
    assert!(
        ffprobe.is_file(),
        "expected auto-installed ffprobe at {}",
        ffprobe.display()
    );

    let report = support::read_report(&artifacts);
    assert_eq!(report.flows[0].status, FlowStatus::Passed);
    let recording = report.flows[0]
        .artifacts
        .recording
        .as_ref()
        .expect("recording artifact");
    assert_h264_with_ffprobe(&ffprobe, PathBuf::from(recording).as_path());
}
