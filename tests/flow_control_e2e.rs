mod support;

use std::collections::BTreeMap;
use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use playrust::browser::BrowserHost;
use playrust::flow::compile_file;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};

const HTML: &str = r#"<!doctype html><html><body>
<button id="counter" onclick="this.textContent = String(Number(this.textContent) + 1); if (this.textContent === '3') setTimeout(() => document.querySelector('#status').textContent = 'ready', 500)">0</button>
<button id="platform" onclick="this.textContent = String(Number(this.textContent) + 1)">0</button>
<button id="loop" onclick="this.textContent = String(Number(this.textContent) + 1)">0</button>
<p id="status">waiting</p>
</body></html>"#;

#[tokio::test(flavor = "current_thread")]
async fn predicates_repeats_retries_and_mapped_subflows_run_in_chrome() {
    let Some(chrome) =
        support::require_browser("predicates_repeats_retries_and_mapped_subflows_run_in_chrome")
    else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = thread::spawn(move || {
        while !server_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let _ = stream.read(&mut [0; 4096]);
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                        HTML.len()
                    )
                    .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture server failed: {error}"),
            }
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("control.yaml");
    let child = directory.path().join("verify.subflow.yaml");
    let worker = directory.path().join("worker.subflow.yaml");
    std::fs::write(
        &root,
        format!(
            r#"version: 1
name: control-flow
base_url: http://{address}
settings: {{ video: off }}
vars: {{ mode: enabled, expected: '3' }}
steps:
  - open: /
  - when: {{ visible: {{ css: '#counter' }} }}
    repeat: 2
    click: {{ target: {{ css: '#counter' }} }}
  - when: {{ hidden: {{ css: '.missing' }} }}
    click: {{ target: {{ css: '#counter' }} }}
  - when: {{ variable: {{ name: mode, equals: disabled }} }}
    click: {{ target: {{ css: '#counter' }} }}
  - when: {{ platform: web }}
    click: {{ target: {{ css: '#platform' }} }}
  - run: ./worker.subflow.yaml
  - when:
      expression:
        all:
          - boolean: '${{keep_going}}'
          - equals: {{ left: '${{mode}}', right: enabled }}
    assert: {{ text: {{ target: {{ css: '#platform' }}, equals: '1' }} }}
  - while:
      expression: {{ boolean: '${{keep_going}}' }}
      max_iterations: 3
    run: ./worker.subflow.yaml
  - assert: {{ text: {{ target: {{ css: '#loop' }}, equals: '2' }} }}
  - retry: 1
    run: {{ path: ./verify.subflow.yaml, vars: {{ expected: '${{expected}}' }} }}
"#
        ),
    )
    .unwrap();
    std::fs::write(
        &worker,
        "version: 1\nname: worker\nsteps:\n  - evaluate: { script: \"return Number(document.querySelector('#loop').textContent) < 1\", save_as: keep_going }\n  - click: { target: { css: '#loop' } }\n",
    )
    .unwrap();
    std::fs::write(
        &child,
        "version: 1\nname: verify\nsettings: { timeout: 300ms }\nvars: { expected: unset }\nsteps:\n  - assert: { text: { target: { css: '#counter' }, equals: '${expected}' } }\n  - assert: { text: { target: { css: '#status' }, equals: ready } }\n",
    )
    .unwrap();

    let flow = compile_file(&root, &BTreeMap::new()).unwrap();
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let report = run_flow(
        &host,
        &flow,
        &RunOptions::new(directory.path().join("artifacts")),
    )
    .await;
    host.shutdown().await.unwrap();
    stop.store(true, Ordering::Release);
    server.join().unwrap();

    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}
