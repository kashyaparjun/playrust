mod support;

use std::collections::BTreeMap;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use support::{FixtureServer, assert_png};

const ROOT: &str = r#"<!doctype html><html><body>
<p id="root">main frame</p><iframe id="child" src="/child" style="border:0;width:320px;height:240px"></iframe>
</body></html>"#;

const CHILD: &str = r#"<!doctype html><html><body>
<p id="child-marker">child frame</p><p id="position">pending</p>
<button id="locate" onclick="navigator.geolocation.getCurrentPosition(p => position.textContent = `${p.coords.latitude},${p.coords.longitude}`)">Locate</button>
<iframe id="grandchild" src="/grandchild" style="border:0;width:180px;height:120px"></iframe>
</body></html>"#;

const GRANDCHILD: &str = r#"<!doctype html><html><body>
<button id="change" onclick="marker.textContent='changed'">Change</button><p id="marker">grandchild frame</p>
</body></html>"#;

#[tokio::test(flavor = "current_thread")]
async fn nested_same_origin_frames_switch_to_parent_and_main() {
    let Some(chrome) =
        support::require_browser("nested_same_origin_frames_switch_to_parent_and_main")
    else {
        return;
    };
    let server = FixtureServer::start(&[
        ("/", "text/html", ROOT),
        ("/child", "text/html", CHILD),
        ("/grandchild", "text/html", GRANDCHILD),
    ]);
    let source = format!(
        r##"version: 1
name: frames
base_url: http://{}
settings:
  video: off
  viewport: {{ width: 800, height: 600 }}
  geolocation: {{ latitude: 35.6, longitude: 139.7 }}
steps:
  - open: /
  - switch_frame: {{ target: {{ css: "#child" }} }}
  - assert: {{ url: {{ path: /child }} }}
  - assert: {{ text: {{ target: {{ css: "#child-marker" }}, equals: "child frame" }} }}
  - click: {{ target: {{ css: "#locate" }} }}
  - assert: {{ text: {{ target: {{ css: "#position" }}, equals: "35.6,139.7" }} }}
  - switch_frame: {{ target: {{ css: "#grandchild" }} }}
  - assert: {{ url: {{ path: /grandchild }} }}
  - click: {{ target: {{ css: "#change" }} }}
  - assert: {{ text: {{ target: {{ css: "#marker" }}, equals: changed }} }}
  - screenshot: {{ name: grandchild }}
  - switch_frame: parent
  - assert: {{ text: {{ target: {{ css: "#child-marker" }}, equals: "child frame" }} }}
  - switch_frame: main
  - assert: {{ text: {{ target: {{ css: "#root" }}, equals: "main frame" }} }}
"##,
        server.address
    );
    let flow = compile_yaml(&source, "frames.yaml", &BTreeMap::new()).expect("compile flow");
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_png(&artifacts.path().join("grandchild.png"), (180, 120));
}

#[tokio::test(flavor = "current_thread")]
async fn cross_origin_oopif_locates_fills_clicks_and_asserts() {
    let Some(chrome) =
        support::require_browser("cross_origin_oopif_locates_fills_clicks_and_asserts")
    else {
        return;
    };
    let foreign = FixtureServer::start(&[(
        "/foreign",
        "text/html",
        r#"<!doctype html><label>Message <input id="message"></label>
        <button onclick="result.textContent = message.value">Send</button>
        <p id="result">pending</p>"#,
    )]);
    let foreign_url = foreign.url().replace("127.0.0.1", "localhost");
    let root = format!(
        "<p id='root'>main frame</p><iframe id='foreign-frame' src='{foreign_url}/foreign' style='margin:80px 0 0 120px;border:0;width:360px;height:240px'></iframe>"
    );
    let server = FixtureServer::start(&[("/", "text/html", &root)]);
    let source = format!(
        "version: 1\nname: oopif\nbase_url: http://{}\nsettings: {{ video: off, viewport: {{ width: 800, height: 600 }} }}\nsteps:\n  - open: /\n  - switch_frame: {{ target: {{ css: '#foreign-frame' }} }}\n  - assert: {{ url: {{ path: /foreign }} }}\n  - fill: {{ target: {{ label: Message }}, value: delivered }}\n  - click: {{ target: {{ role: {{ value: button, name: Send }} }} }}\n  - assert: {{ text: {{ target: {{ css: '#result' }}, equals: delivered }} }}\n  - screenshot: {{ name: foreign }}\n  - switch_frame: parent\n  - assert: {{ text: {{ target: {{ css: '#root' }}, equals: 'main frame' }} }}\n",
        server.address
    );
    let flow = compile_yaml(&source, "oopif.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_png(&artifacts.path().join("foreign.png"), (360, 240));
}

#[tokio::test(flavor = "current_thread")]
async fn active_frame_navigates_across_origins_and_process_replacements() {
    let Some(chrome) =
        support::require_browser("active_frame_navigates_across_origins_and_process_replacements")
    else {
        return;
    };
    let foreign = FixtureServer::start(&[(
        "/foreign",
        "text/html",
        "<!doctype html><p id='marker'>foreign process</p>",
    )]);
    let foreign_url = foreign.url().replace("127.0.0.1", "localhost");
    let server = FixtureServer::start(&[
        (
            "/",
            "text/html",
            "<!doctype html><iframe id='frame' src='/same'></iframe>",
        ),
        (
            "/same",
            "text/html",
            "<!doctype html><p id='marker'>same process</p>",
        ),
        (
            "/returned",
            "text/html",
            "<!doctype html><p id='marker'>returned process</p>",
        ),
    ]);
    let source = format!(
        "version: 1\nname: frame-navigation\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - switch_frame: {{ target: {{ css: '#frame' }} }}\n  - assert: {{ text: {{ target: {{ css: '#marker' }}, equals: 'same process' }} }}\n  - open: {foreign_url}/foreign\n  - assert: {{ url: {{ path: /foreign }} }}\n  - assert: {{ text: {{ target: {{ css: '#marker' }}, equals: 'foreign process' }} }}\n  - open: {}/returned\n  - assert: {{ url: {{ path: /returned }} }}\n  - assert: {{ text: {{ target: {{ css: '#marker' }}, equals: 'returned process' }} }}\n",
        server.url, server.url
    );
    let flow = compile_yaml(&source, "frame-navigation.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}

#[tokio::test(flavor = "current_thread")]
async fn active_oopif_clears_dom_storage_scrolls_and_swipes() {
    let Some(chrome) =
        support::require_browser("active_oopif_clears_dom_storage_scrolls_and_swipes")
    else {
        return;
    };
    let foreign = FixtureServer::start(&[(
        "/foreign",
        "text/html",
        r#"<!doctype html><style>body{height:1800px}#swipe{margin-top:20px;width:160px;height:80px}</style>
        <button id="storage">storage</button><button id="swipe">swipe</button>
        <p id="state">pending</p><script>
        localStorage.setItem('local', 'present'); sessionStorage.setItem('session', 'present');
        storage.onclick = () => state.textContent = `${localStorage.getItem('local')};${sessionStorage.getItem('session')}`;
        addEventListener('scroll', () => state.textContent = 'scrolled');
        swipe.onmouseup = () => state.textContent = 'swiped';
        </script>"#,
    )]);
    let foreign_url = foreign.url().replace("127.0.0.1", "localhost");
    let root = format!(
        "<!doctype html><iframe id='frame' src='{foreign_url}/foreign' style='width:500px;height:400px'></iframe>"
    );
    let server = FixtureServer::start(&[("/", "text/html", &root)]);
    let source = format!(
        "version: 1\nname: oopif-actions\nbase_url: {}\nsettings: {{ video: off, viewport: {{ width: 800, height: 600 }} }}\nsteps:\n  - open: /\n  - switch_frame: {{ target: {{ css: '#frame' }} }}\n  - clear: storage\n  - click: {{ target: {{ css: '#storage' }} }}\n  - assert: {{ text: {{ target: {{ css: '#state' }}, equals: 'null;null' }} }}\n  - scroll: {{ y: 300 }}\n  - assert: {{ text: {{ target: {{ css: '#state' }}, equals: scrolled }} }}\n  - swipe: {{ target: {{ css: '#swipe' }}, x: 40, duration: 50ms }}\n  - assert: {{ text: {{ target: {{ css: '#state' }}, equals: swiped }} }}\n",
        server.url
    );
    let flow = compile_yaml(&source, "oopif-actions.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}

#[tokio::test(flavor = "current_thread")]
async fn nested_oopif_preserves_parent_and_main_switching() {
    let Some(chrome) = support::require_browser("nested_oopif_preserves_parent_and_main_switching")
    else {
        return;
    };
    let foreign_page = r#"<p id='foreign'>foreign</p>
        <iframe id='nested' style='margin:20px;width:220px;height:120px'></iframe>
        <script>
          const root = decodeURIComponent(location.hash.slice(1));
          nested.src = root + '/nested';
        </script>"#;
    let foreign = FixtureServer::start(&[("/foreign", "text/html", foreign_page)]);
    let foreign_url = foreign.url().replace("127.0.0.1", "localhost");
    let root = format!(
        "<p id='root'>root</p><iframe id='foreign-frame' src='{foreign_url}/foreign#' style='margin:60px;width:420px;height:320px'></iframe><script>foreignFrame = document.querySelector('#foreign-frame'); foreignFrame.src += encodeURIComponent(location.origin)</script>"
    );
    let server = FixtureServer::start(&[
        ("/", "text/html", &root),
        (
            "/nested",
            "text/html",
            "<button id='nested-button' onclick=\"nestedStatus.textContent='nested clicked'\">Nested</button><p id='nestedStatus'>pending</p><iframe id='same-child' src='/same-child'></iframe>",
        ),
        (
            "/same-child",
            "text/html",
            "<p id='runtime-marker'>pending</p>",
        ),
    ]);
    let source = format!(
        "version: 1\nname: nested-oopif\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - switch_frame: {{ target: {{ css: '#foreign-frame' }} }}\n  - switch_frame: {{ target: {{ css: '#nested' }} }}\n  - click: {{ target: {{ css: '#nested-button' }} }}\n  - assert: {{ text: {{ target: {{ css: '#nestedStatus' }}, equals: 'nested clicked' }} }}\n  - switch_frame: {{ target: {{ css: '#same-child' }} }}\n  - evaluate: {{ script: \"document.querySelector('#runtime-marker').textContent = 'runtime routed';\" }}\n  - assert: {{ text: {{ target: {{ css: '#runtime-marker' }}, equals: 'runtime routed' }} }}\n  - switch_frame: parent\n  - switch_frame: parent\n  - assert: {{ text: {{ target: {{ css: '#foreign' }}, equals: foreign }} }}\n  - switch_frame: main\n  - assert: {{ text: {{ target: {{ css: '#root' }}, equals: root }} }}\n",
        server.url()
    );
    let flow = compile_yaml(&source, "nested-oopif.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}

#[tokio::test(flavor = "current_thread")]
async fn back_inside_frame_is_explicitly_rejected() {
    let Some(chrome) = support::require_browser("back_inside_frame_is_explicitly_rejected") else {
        return;
    };
    let server = FixtureServer::start(&[("/", "text/html", ROOT), ("/child", "text/html", CHILD)]);
    let source = format!(
        "version: 1\nname: frame-back\nbase_url: http://{}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - switch_frame: {{ target: {{ css: '#child' }} }}\n  - back: {{}}\n",
        server.address
    );
    let flow = compile_yaml(&source, "frame-back.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Failed);
    assert!(
        report.failures[0]
            .message
            .as_str()
            .contains("back navigation is unsupported inside a frame"),
        "{:#?}",
        report.failures
    );
}
