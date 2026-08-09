mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use playrust::browser::{BrowserHost, Viewport};
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use support::FixtureServer;

const HTML: &str = r#"<!doctype html><html><body><p id="status">loading</p><button id="inspect">inspect</button><p id="state"></p><script>
async function initialize() {
  document.cookie = 'flow=present; path=/';
  localStorage.setItem('local', 'present');
  sessionStorage.setItem('session', 'present');
  await new Promise((resolve, reject) => {
    const request = indexedDB.open('flow-db');
    request.onsuccess = () => { request.result.close(); resolve(); };
    request.onerror = () => reject(request.error);
  });
  await caches.open('flow-cache');
  navigator.serviceWorker.register('/sw.js');
  while ((await navigator.serviceWorker.getRegistrations()).length === 0) {
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  document.querySelector('#status').textContent = 'ready';
}
document.querySelector('#inspect').onclick = async () => {
  const [databases, cacheNames, registrations] = await Promise.all([
    indexedDB.databases(), caches.keys(), navigator.serviceWorker.getRegistrations()
  ]);
  document.querySelector('#state').textContent = `cookie=${document.cookie};local=${localStorage.getItem('local')};session=${sessionStorage.getItem('session')};db=${databases.length};cache=${cacheNames.length};workers=${registrations.length}`;
};
initialize();
</script></body></html>"#;
const WORKER: &str = "self.addEventListener('fetch', () => {});";
const ROUTES: &[(&str, &str, &str)] = &[
    ("/", "text/html", HTML),
    ("/sw.js", "text/javascript", WORKER),
];

#[tokio::test(flavor = "current_thread")]
async fn extended_clear_targets_preserve_other_state_and_contexts() {
    let Some(chrome) =
        support::require_browser("extended_clear_targets_preserve_other_state_and_contexts")
    else {
        return;
    };
    let server = FixtureServer::start(ROUTES);
    let source = format!(
        "version: 1\nname: storage-clearing\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - assert: {{ text: {{ target: {{ css: '#status' }}, equals: ready }} }}\n  - clear: indexeddb\n  - clear: cache-storage\n  - clear: service-workers\n  - click: {{ target: {{ css: '#inspect' }} }}\n  - assert: {{ text: {{ target: {{ css: '#state' }}, equals: 'cookie=flow=present;local=present;session=present;db=0;cache=0;workers=0' }} }}\n",
        server.url
    );
    let flow = compile_yaml(&source, "storage-clearing.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let other = host
        .create_context(Viewport::new(800, 600).unwrap(), None)
        .await
        .unwrap();
    other.page().goto(&server.url).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        other.page().evaluate(
            "new Promise(resolve => { const check = () => document.querySelector('#status').textContent === 'ready' ? resolve() : setTimeout(check, 20); check(); })",
        ),
    )
    .await
    .expect("second context initialization timed out")
    .unwrap();
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        run_flow(&host, &flow, &RunOptions::new(artifacts.path())),
    )
    .await
    .expect("clear flow timed out");
    let other_state: Vec<usize> = tokio::time::timeout(
        Duration::from_secs(10),
        other.page().evaluate("Promise.all([indexedDB.databases(), caches.keys(), navigator.serviceWorker.getRegistrations()]).then(values => values.map(value => value.length))"),
    )
        .await
        .expect("second context state inspection timed out")
        .unwrap()
        .into_value()
        .unwrap();
    let other_dom: String = tokio::time::timeout(
        Duration::from_secs(10),
        other.page().evaluate("`${document.cookie};${localStorage.getItem('local')};${sessionStorage.getItem('session')}`"),
    )
        .await
        .expect("second context DOM storage inspection timed out")
        .unwrap()
        .into_value()
        .unwrap();

    host.dispose_context(other).await.unwrap();
    host.shutdown().await.unwrap();
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_eq!(other_state, vec![1, 1, 1]);
    assert_eq!(other_dom, "flow=present;present;present");
}
