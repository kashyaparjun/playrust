mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use playrust::browser::{BrowserHost, Viewport};
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use support::FixtureServer;

const ROOT: &str = r#"<!doctype html><body>
<button id="named" onclick="window.open('/named', 'checkout')">Named</button>
<button id="url" onclick="window.open('/by-url?slot=2', 'second')">URL</button>
<p id="root">root</p>
</body>"#;
const NAMED: &str = r#"<!doctype html><body><p id="page">named</p></body>"#;
const BY_URL: &str = r#"<!doctype html><body><p id="page">url</p></body>"#;

#[tokio::test(flavor = "current_thread")]
async fn selects_named_and_exact_url_pages_inside_the_flow_context() {
    let Some(chrome) =
        support::require_browser("selects_named_and_exact_url_pages_inside_the_flow_context")
    else {
        return;
    };
    let server = FixtureServer::start(&[
        ("/", "text/html", ROOT),
        ("/named", "text/html", NAMED),
        ("/by-url?slot=2", "text/html", BY_URL),
    ]);
    let source = format!(
        "version: 1\nname: tab-selection\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - click: {{ target: {{ css: '#named' }} }}\n  - click: {{ target: {{ css: '#url' }} }}\n  - switch_page: {{ name: checkout }}\n  - assert: {{ text: {{ target: {{ css: '#page' }}, equals: named }} }}\n  - switch_page: opener\n  - switch_page: {{ url: '/by-url?slot=2' }}\n  - assert: {{ text: {{ target: {{ css: '#page' }}, equals: url }} }}\n",
        server.url
    );
    let flow = compile_yaml(&source, "tab-selection.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(chrome, false).await.unwrap();

    let other = host
        .create_context(Viewport::new(640, 480).unwrap(), None)
        .await
        .unwrap();
    other
        .page()
        .goto(format!("{}/named", server.url))
        .await
        .unwrap();
    other
        .page()
        .evaluate("window.name = 'checkout'")
        .await
        .unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    wait_for_flow_pages_to_close(&host, &server.url, other.page().target_id()).await;

    let diagnostic_source = format!(
        "version: 1\nname: tab-diagnostic\nbase_url: {}\nsettings: {{ video: off, timeout: 500ms }}\nsteps:\n  - open: /\n  - click: {{ target: {{ css: '#url' }} }}\n  - switch_page: {{ url: '/by-url?slot=2' }}\n  - assert: {{ visible: {{ css: '#missing' }} }}\n",
        server.url
    );
    let diagnostic =
        compile_yaml(&diagnostic_source, "tab-diagnostic.yaml", &BTreeMap::new()).unwrap();
    let diagnostic_artifacts = tempfile::tempdir().unwrap();
    let report = run_flow(
        &host,
        &diagnostic,
        &RunOptions::new(diagnostic_artifacts.path()),
    )
    .await;
    assert_eq!(report.status, FlowStatus::Failed);
    assert!(
        report.failures[0]
            .current_url
            .as_ref()
            .is_some_and(|url| url.as_str().ends_with("/by-url?slot=2")),
        "{:#?}",
        report.failures
    );
    wait_for_flow_pages_to_close(&host, &server.url, other.page().target_id()).await;

    host.dispose_context(other).await.unwrap();
    host.shutdown().await.unwrap();
}

async fn wait_for_flow_pages_to_close(
    host: &BrowserHost,
    origin: &str,
    retained: &chromiumoxide::cdp::browser_protocol::target::TargetId,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let pages = host.browser().pages().await.unwrap();
        let mut remaining = Vec::new();
        for page in pages {
            if page.target_id() != retained
                && let Some(url) = page.url().await.unwrap()
                && url.starts_with(origin)
            {
                remaining.push(url);
            }
        }
        if remaining.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("flow context pages leaked: {remaining:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
