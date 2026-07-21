mod support;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::runner::{RunOptions, run_flow};
use support::{FixtureServer, ffmpeg_path};

const HTML: &str = "<!doctype html><html><body><h1>Overlay fixture</h1></body></html>";
const ROUTES: &[(&str, &str, &str)] = &[("/", "text/html", HTML)];

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
async fn recording_changes_when_presentation_overlays_are_enabled() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
    let server = FixtureServer::start(ROUTES);
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let common = |name: &str, overlays: &str| {
        compile_yaml(
            &format!(
                "version: 1\nname: {name}\nbase_url: {}\nsettings:\n  video: on\n  overlays: {overlays}\nsteps:\n  - id: landing\n    open: /\n",
                server.url
            ),
            format!("{name}.yaml"),
            &BTreeMap::new(),
        )
        .unwrap()
    };
    let plain = run_flow(
        &host,
        &common("plain", "{}"),
        &RunOptions::new(directory.path().join("plain")).with_ffmpeg(ffmpeg_path()),
    )
    .await;
    let overlay = run_flow(
        &host,
        &common("overlay", "{ step: true, url: true, pointer: true }"),
        &RunOptions::new(directory.path().join("overlay")).with_ffmpeg(ffmpeg_path()),
    )
    .await;

    let plain_bytes = fs::read(plain.artifacts.recording.expect("plain recording")).unwrap();
    let overlay_bytes = fs::read(overlay.artifacts.recording.expect("overlay recording")).unwrap();
    assert_ne!(
        plain_bytes, overlay_bytes,
        "the overlay must be encoded into the recording"
    );
    host.shutdown().await.unwrap();
}
