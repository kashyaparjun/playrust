use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use playrust::browser::BrowserHost;
use playrust::flow::compile_yaml;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use playrust::video::{VideoConfig, preflight_ffmpeg};

const HTML: &str = r#"<!doctype html>
<html lang="en">
  <body>
    <label for="name">Name</label>
    <input id="name">
    <button onclick="document.querySelector('#message').textContent = 'Hello, ' + document.querySelector('#name').value; document.querySelector('#message').hidden = false; history.pushState({}, '', '/done')">Submit</button>
    <p id="message" hidden></p>
  </body>
</html>
"#;

const VIDEO_HTML: &str = r#"<!doctype html>
<html lang="en">
  <body>
    <p id="status">frame 0</p>
    <script>
      let frame = 0;
      setInterval(() => {
        frame += 1;
        document.querySelector('#status').textContent = `frame ${frame}`;
        document.body.style.backgroundColor = frame % 2 ? '#123456' : '#abcdef';
      }, 50);
    </script>
  </body>
</html>
"#;

struct FixtureServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

impl FixtureServer {
    fn start(html: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("read fixture address");
        listener
            .set_nonblocking(true)
            .expect("make fixture server stoppable");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || -> io::Result<()> {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false)?;
                        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                        let mut request = [0; 4096];
                        let _ = stream.read(&mut request)?;
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                            html.len()
                        )?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .expect("fixture server thread missing")
            .join()
            .expect("fixture server thread panicked")
            .expect("serve HTML fixture");
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.thread.take() {
            let _ = server.join();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn browser_flow_smoke() {
    let chrome = PathBuf::from(
        env::var_os("PLAYRUST_CHROME")
            .expect("set PLAYRUST_CHROME to the pinned Chrome executable"),
    );
    let server = FixtureServer::start(HTML);

    let flow = compile_yaml(
        &format!(
            r##"version: 1
name: browser-smoke
base_url: http://{}
settings:
  video: off
steps:
  - open: /
  - fill:
      target: {{ label: Name }}
      value: Playrust
  - click:
      target:
        role:
          value: button
          name: Submit
  - assert:
      visible: {{ css: "#message" }}
  - assert:
      text:
        target: {{ css: "#message" }}
        equals: "Hello, Playrust"
  - assert:
      url:
        path: /done
"##,
            server.address
        ),
        "browser-smoke.yaml",
        &BTreeMap::new(),
    )
    .expect("compile smoke flow");
    let artifacts = tempfile::tempdir().expect("create artifact directory");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    let shutdown = host.shutdown().await;
    server.shutdown();

    shutdown.expect("shut down Chrome cleanly");
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
async fn browser_video_flow_smoke() {
    let chrome = PathBuf::from(
        env::var_os("PLAYRUST_CHROME")
            .expect("set PLAYRUST_CHROME to the pinned Chrome executable"),
    );
    let ffmpeg = env::var_os("PLAYRUST_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let server = FixtureServer::start(VIDEO_HTML);
    let flow = compile_yaml(
        &format!(
            r##"version: 1
name: browser-video-smoke
base_url: http://{}
settings:
  timeout: 10s
  viewport: {{ width: 800, height: 600 }}
  video: on
steps:
  - open: /
  - assert:
      visible: {{ css: "#status" }}
"##,
            server.address
        ),
        "browser-video-smoke.yaml",
        &BTreeMap::new(),
    )
    .expect("compile video smoke flow");
    let artifacts = tempfile::tempdir().expect("create artifact directory");
    preflight_ffmpeg(&VideoConfig {
        mode: flow.settings.video,
        ffmpeg_path: ffmpeg.clone(),
        output_path: artifacts.path().join("recording.webm"),
        viewport_width: flow.settings.viewport.width,
        viewport_height: flow.settings.viewport.height,
    })
    .await
    .expect("preflight FFmpeg");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");

    let options = RunOptions::new(artifacts.path()).with_ffmpeg(ffmpeg);
    let report = run_flow(&host, &flow, &options).await;
    let shutdown = host.shutdown().await;
    server.shutdown();

    shutdown.expect("shut down Chrome cleanly");
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    let recording = report
        .artifacts
        .recording
        .as_deref()
        .expect("report recording path");
    assert!(std::path::Path::new(recording).exists());
    assert!(
        std::fs::metadata(recording)
            .expect("read recording metadata")
            .len()
            > 0
    );
    let ffprobe = env::var_os("PLAYRUST_FFPROBE").unwrap_or_else(|| "ffprobe".into());
    let probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height:format=duration",
            "-of",
            "default=noprint_wrappers=1",
            recording,
        ])
        .output()
        .expect("run ffprobe");
    assert!(
        probe.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let metadata = String::from_utf8_lossy(&probe.stdout);
    assert!(metadata.contains("codec_name=vp9"), "{metadata}");
    assert!(metadata.contains("width=800"), "{metadata}");
    assert!(metadata.contains("height=600"), "{metadata}");
}
