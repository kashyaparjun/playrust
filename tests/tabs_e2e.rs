mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use support::{FixtureServer, assert_png};

const ROOT: &str = r#"<!doctype html><html><body>
<button id="open" onclick="window.open('/popup', 'popup')">Open popup</button>
<p id="root">opener page</p>
</body></html>"#;

const POPUP: &str = r#"<!doctype html><html><body>
<p id="popup">popup page</p><p id="position">pending</p>
<button id="locate" onclick="navigator.geolocation.getCurrentPosition(p => position.textContent = `${p.coords.latitude},${p.coords.longitude},${p.coords.accuracy}`)">Locate</button>
</body></html>"#;

#[test]
fn page_switching_rejects_enabled_video() {
    let error = compile_yaml(
        "version: 1\nname: unsafe-video\nsteps: [{ switch_page: popup }]\n",
        "unsafe-video.yaml",
        &BTreeMap::new(),
    )
    .expect_err("page switching must reject default-on video");
    assert!(error.to_string().contains("requires settings.video: off"));

    let error = compile_yaml(
        "version: 1\nname: unsafe-video\nsteps: [{ switch_page: { name: checkout } }]\n",
        "unsafe-video.yaml",
        &BTreeMap::new(),
    )
    .expect_err("named page switching must reject default-on video");
    assert!(error.to_string().contains("requires settings.video: off"));
}

#[tokio::test(flavor = "current_thread")]
async fn popup_and_opener_switch_active_page_state() {
    let Some(chrome) = support::require_browser("popup_and_opener_switch_active_page_state") else {
        return;
    };
    let server = FixtureServer::start(&[("/", "text/html", ROOT), ("/popup", "text/html", POPUP)]);
    let source = format!(
        r##"version: 1
name: tabs
base_url: http://{}
settings:
  video: off
  viewport: {{ width: 640, height: 480 }}
  geolocation: {{ latitude: 51.5, longitude: -0.12, accuracy: 4 }}
steps:
  - open: /
  - click: {{ target: {{ css: "#open" }} }}
  - switch_page: popup
  - assert: {{ url: {{ path: /popup }} }}
  - click: {{ target: {{ css: "#locate" }} }}
  - assert: {{ text: {{ target: {{ css: "#position" }}, equals: "51.5,-0.12,4" }} }}
  - screenshot: {{ name: popup }}
  - switch_page: opener
  - assert: {{ text: {{ target: {{ css: "#root" }}, equals: "opener page" }} }}
"##,
        server.address
    );
    let flow = compile_yaml(&source, "tabs.yaml", &BTreeMap::new()).expect("compile tabs flow");
    let artifacts = tempfile::tempdir().expect("create artifacts");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch Chrome");
    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_png(&artifacts.path().join("popup.png"), (640, 480));
    wait_for_context_pages_to_close(&host, &format!("http://{}", server.address)).await;

    let diagnostic_source = format!(
        "version: 1\nname: popup-diagnostic\nbase_url: http://{}\nsettings: {{ video: off, timeout: 500ms }}\nsteps:\n  - open: /\n  - click: {{ target: {{ css: '#open' }} }}\n  - switch_page: popup\n  - assert: {{ visible: {{ css: '#missing' }} }}\n",
        server.address
    );
    let diagnostic = compile_yaml(
        &diagnostic_source,
        "popup-diagnostic.yaml",
        &BTreeMap::new(),
    )
    .unwrap();
    let diagnostic_artifacts = tempfile::tempdir().unwrap();
    let report = run_flow(
        &host,
        &diagnostic,
        &RunOptions::new(diagnostic_artifacts.path()),
    )
    .await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Failed);
    assert!(
        report.failures[0]
            .current_url
            .as_ref()
            .is_some_and(|url| url.as_str().ends_with("/popup")),
        "{:#?}",
        report.failures
    );
}

async fn wait_for_context_pages_to_close(host: &BrowserHost, origin: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let pages = host.browser().pages().await.expect("list pages");
        let mut urls = Vec::new();
        for page in pages {
            urls.push(page.url().await.unwrap_or_default().unwrap_or_default());
        }
        if urls.iter().all(|url| !url.starts_with(origin)) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("flow context pages leaked: {urls:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
