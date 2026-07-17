use std::collections::BTreeMap;
use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};

const HTML: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <style>
      #panel { position: relative; height: 300px; }
      #target { position: absolute; left: 100px; top: 100px; width: 120px; height: 50px; }
      #cover { position: absolute; left: 145px; top: 100px; width: 30px; height: 50px; z-index: 2; }
      #above-anchor { position: absolute; left: 100px; top: 200px; width: 40px; height: 20px; }
      #below-anchor { position: absolute; left: 100px; top: 20px; width: 40px; height: 20px; }
      #left-anchor { position: absolute; left: 10px; top: 100px; width: 30px; height: 20px; }
      #right-anchor { position: absolute; left: 300px; top: 100px; width: 30px; height: 20px; }
    </style>
  </head>
  <body>
    <button class="enabled-choice" disabled>Disabled</button>
    <button class="enabled-choice" onclick="document.querySelector('#enabled-result').textContent = 'enabled'">Enabled</button>
    <p id="enabled-result">pending</p>
    <section id="panel">
      <button id="target" class="candidate"><span>Save</span></button>
      <div id="cover"></div>
      <div id="above-anchor"></div>
      <div id="below-anchor"></div>
      <div id="left-anchor"></div>
      <div id="right-anchor"></div>
    </section>
    <p id="click-result">pending</p>
    <script>
      document.querySelector('#target').addEventListener('click', event => {
        const rect = event.currentTarget.getBoundingClientRect();
        document.querySelector('#click-result').textContent =
          `${Math.round(event.clientX - rect.left)},${Math.round(event.clientY - rect.top)}`;
      });
    </script>
  </body>
</html>"#;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn advanced_relations_states_and_relative_click_point_work_live() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let address = listener.local_addr().expect("read fixture address");
    let server = thread::spawn(move || {
        for _ in 0..1 {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).expect("read fixture request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                HTML.len()
            )
            .expect("write fixture response");
        }
    });
    let flow = compile_yaml(
        &format!(
            r##"version: 1
name: advanced-selectors
base_url: http://{address}
settings: {{ video: off }}
steps:
  - open: /
  - click:
      target: {{ css: .enabled-choice, enabled: true, index: 0 }}
  - assert: {{ text: {{ target: {{ css: "#enabled-result" }}, equals: enabled }} }}
  - click:
      position: {{ x: 10, y: 10 }}
      target:
        css: .candidate
        within: {{ css: "#panel" }}
        has: {{ text: Save, within: {{ css: .candidate }} }}
        above: {{ css: "#above-anchor" }}
        below: {{ css: "#below-anchor" }}
        left: {{ css: "#right-anchor" }}
        right: {{ css: "#left-anchor" }}
  - assert: {{ text: {{ target: {{ css: "#click-result" }}, equals: "10,10" }} }}
"##
        ),
        "advanced-selectors.yaml",
        &BTreeMap::new(),
    )
    .expect("compile advanced selector flow");
    let chrome = PathBuf::from(
        env::var_os("PLAYRUST_CHROME")
            .expect("set PLAYRUST_CHROME to the pinned Chrome executable"),
    );
    let artifacts = tempfile::tempdir().expect("create artifact directory");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    host.shutdown().await.expect("shut down Chrome cleanly");
    server.join().expect("fixture server thread");

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}
