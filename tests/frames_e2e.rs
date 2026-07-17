mod support;

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

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
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn nested_same_origin_frames_switch_to_parent_and_main() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
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
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn cross_origin_oopif_is_explicitly_rejected() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
    let foreign =
        FixtureServer::start(&[("/foreign", "text/html", "<p id='foreign'>foreign frame</p>")]);
    let root = format!(
        "<p id='root'>main frame</p><iframe id='foreign-frame' src='http://{}/foreign'></iframe>",
        foreign.address
    );
    let server = FixtureServer::start(&[("/", "text/html", &root)]);
    let source = format!(
        "version: 1\nname: oopif\nbase_url: http://{}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - switch_frame: {{ target: {{ css: '#foreign-frame' }} }}\n",
        server.address
    );
    let flow = compile_yaml(&source, "oopif.yaml", &BTreeMap::new()).unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let host = BrowserHost::launch(&chrome, false).await.unwrap();

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.unwrap();

    assert_eq!(report.status, FlowStatus::Failed);
    assert!(
        report.failures[0]
            .message
            .as_str()
            .contains("cross-origin iframe switching is unsupported"),
        "{:#?}",
        report.failures
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn back_inside_frame_is_explicitly_rejected() {
    let chrome = PathBuf::from(env::var_os("PLAYRUST_CHROME").expect("set PLAYRUST_CHROME"));
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
