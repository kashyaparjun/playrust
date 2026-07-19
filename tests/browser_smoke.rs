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
use playrust::flow::{compile_file, compile_yaml};
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};
use playrust::video::{VideoConfig, preflight_ffmpeg};

const HTML: &str = r#"<!doctype html>
<html lang="en">
  <body style="min-height: 2000px">
    <label for="name">Name</label>
    <input id="name" value="old">
    <label for="choice">Choice</label>
    <select id="choice"><option value="">Choose</option><option value="second">Second</option></select>
    <button id="inspect" onclick="document.querySelector('#controls').textContent = `name=${document.querySelector('#name').value};choice=${document.querySelector('#choice').value};erase=${textEvents.join(',')};select=${selectEvents.join(',')}`">Inspect controls</button>
    <p id="controls"></p>
    <button onclick="document.querySelector('#message').textContent = 'Hello, ' + document.querySelector('#name').value; document.querySelector('#message').hidden = false; history.pushState({}, '', '/done')">Submit</button>
    <p id="message" hidden></p>
    <input class="choice" type="checkbox" data-name="first">
    <input class="choice" type="checkbox" data-name="second" checked>
    <input class="choice" type="checkbox" data-name="third">
    <p id="selected-choice"></p>
    <select size="2">
      <option>Alpha</option>
      <option selected>Beta</option>
    </select>
    <button data-testid="double">Double</button>
    <p id="mouse-events"></p>
    <p id="scroll-status">not scrolled</p>
    <script>
      const events = [];
      const textEvents = [];
      const selectEvents = [];
      for (const type of ['input', 'change']) {
        document.querySelector('#name').addEventListener(type, () => textEvents.push(type));
      }
      for (const type of ['input', 'change']) {
        document.querySelector('#choice').addEventListener(type, () => selectEvents.push(type));
      }
      addEventListener('scroll', () => document.querySelector('#scroll-status').textContent = 'scrolled', { once: true });
      for (const choice of document.querySelectorAll('.choice')) {
        choice.addEventListener('change', () => {
          document.querySelector('#selected-choice').textContent = choice.dataset.name;
        });
      }
      const target = document.querySelector('[data-testid="double"]');
      for (const type of ['mousedown', 'mouseup', 'click', 'dblclick']) {
        target.addEventListener(type, event => {
          events.push(`${type}:${event.detail}`);
          document.querySelector('#mouse-events').textContent = events.join(',');
        });
      }
    </script>
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

const STATE_HTML: &str = r#"<!doctype html>
<html lang="en">
  <body>
    <button id="inspect-cookies" onclick="document.querySelector('#cookies').textContent = document.cookie || 'none'">Inspect cookies</button>
    <button id="inspect-storage" onclick="document.querySelector('#storage').textContent = `${localStorage.getItem('local') ?? 'none'}/${sessionStorage.getItem('session') ?? 'none'}`">Inspect storage</button>
    <p id="cookies">pending</p>
    <p id="storage">pending</p>
    <script>
      document.cookie = 'flow=present; path=/';
      localStorage.setItem('local', 'present');
      sessionStorage.setItem('session', 'present');
    </script>
  </body>
</html>
"#;

const GEOLOCATION_HTML: &str = r#"<!doctype html>
<html lang="en">
  <body>
    <button id="locate" onclick="navigator.geolocation.getCurrentPosition(position => document.querySelector('#position').textContent = JSON.stringify([position.coords.latitude, position.coords.longitude, position.coords.accuracy]), error => document.querySelector('#position').textContent = error.message)">Locate</button>
    <p id="position">pending</p>
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
                        match stream.read(&mut request) {
                            Ok(0) => continue,
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                        if let Err(error) = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                            html.len()
                        ) && !matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                        ) {
                            return Err(error);
                        }
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

    let flow_files = tempfile::tempdir().expect("create subflow directory");
    let root = flow_files.path().join("browser-smoke.yaml");
    let child = flow_files.path().join("actions.subflow.yaml");
    std::fs::write(
        &root,
        format!(
            r##"version: 1
name: browser-smoke
base_url: http://{}
settings:
  video: off
steps:
  - open: /
  - run: ./actions.subflow.yaml
"##,
            server.address
        ),
    )
    .expect("write smoke entrypoint");
    let child_source = r##"version: 1
name: browser-smoke-actions
base_url: __BASE_URL__
steps:
  - erase: { target: { label: Name } }
  - select: { target: { label: Choice }, value: second }
  - click: { target: { css: "#inspect" } }
  - assert:
      text:
        target: { css: "#controls" }
        equals: "name=;choice=second;erase=input,change;select=input,change"
  - fill:
      target: { label: Name }
      value: Playrust
  - assert:
      visible: { css: "#name", focused: true }
  - click:
      target: { css: ".choice", checked: false, index: 1 }
  - assert:
      text: { target: { css: "#selected-choice" }, equals: third }
  - assert:
      visible: { css: ".choice", checked: false }
  - assert:
      text: { target: { css: "select[size] option", selected: false, index: 0 }, equals: Alpha }
  - assert:
      text: { target: { css: "select[size] option", selected: true }, equals: Beta }
  - click:
      target:
        role:
          value: button
          name: Submit
  - assert:
      visible: { css: "#message" }
  - assert:
      text:
        target: { css: "#message" }
        equals: "Hello, Playrust"
  - screenshot:
      name: result
      crop: { x: 5, y: 7, width: 100, height: 80 }
  - assert:
      url:
        path: /done
  - double_click:
      target: { test_id: double }
  - assert:
      text:
        target: { css: "#mouse-events" }
        equals: "mousedown:1,mouseup:1,click:1,mousedown:2,mouseup:2,click:2,dblclick:2"
  - scroll: { y: 600 }
  - assert: { text: { target: { css: "#scroll-status" }, equals: scrolled } }
  - back: {}
  - assert: { url: { path: / } }
  - open: /other
  - back: {}
  - assert: { url: { path: / } }
"##
    .replace("__BASE_URL__", &format!("http://{}", server.address));
    std::fs::write(&child, child_source).expect("write smoke subflow");
    let flow = compile_file(&root, &BTreeMap::new()).expect("compile smoke flow");
    let artifacts = tempfile::tempdir().expect("create artifact directory");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    let shutdown = host.shutdown().await;
    server.shutdown();

    shutdown.expect("shut down Chrome cleanly");
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    let screenshot = report
        .artifacts
        .screenshots
        .first()
        .expect("report screenshot path");
    assert!(screenshot.ends_with("result.png"));
    let png = std::fs::read(screenshot).expect("read screenshot");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 100);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 80);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn clear_state_is_scoped_to_the_active_flow() {
    let chrome = PathBuf::from(
        env::var_os("PLAYRUST_CHROME")
            .expect("set PLAYRUST_CHROME to the pinned Chrome executable"),
    );
    let server = FixtureServer::start(STATE_HTML);
    let url = format!("http://{}", server.address);
    let flow = compile_yaml(
        &format!(
            r##"version: 1
name: clear-state
base_url: {url}
settings:
  video: off
steps:
  - open: /
  - clear: cookies
  - click: {{ target: {{ css: "#inspect-cookies" }} }}
  - assert: {{ text: {{ target: {{ css: "#cookies" }}, equals: none }} }}
  - clear: storage
  - click: {{ target: {{ css: "#inspect-storage" }} }}
  - assert: {{ text: {{ target: {{ css: "#storage" }}, equals: none/none }} }}
"##
        ),
        "clear-state.yaml",
        &BTreeMap::new(),
    )
    .expect("compile clear-state flow");
    let artifacts = tempfile::tempdir().expect("create artifact directory");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");
    let other_context = host
        .create_context(playrust::browser::Viewport::new(800, 600).unwrap(), None)
        .await
        .expect("create second isolated context");
    other_context
        .page()
        .goto(url.as_str())
        .await
        .expect("initialize second context");

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    let other_cookie: String = other_context
        .page()
        .evaluate("document.cookie")
        .await
        .expect("read second context cookie")
        .into_value()
        .expect("decode second context cookie");
    host.dispose_context(other_context).await.unwrap();
    let shutdown = host.shutdown().await;
    server.shutdown();

    shutdown.expect("shut down Chrome cleanly");
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_eq!(other_cookie, "flow=present");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
async fn geolocation_is_applied_only_to_the_flow_context() {
    let chrome = PathBuf::from(
        env::var_os("PLAYRUST_CHROME")
            .expect("set PLAYRUST_CHROME to the pinned Chrome executable"),
    );
    let server = FixtureServer::start(GEOLOCATION_HTML);
    let url = format!("http://{}", server.address);
    let flow = compile_yaml(
        &format!(
            r##"version: 1
name: geolocation
base_url: {url}
settings:
  video: off
  geolocation: {{ latitude: 37.7749, longitude: -122.4194, accuracy: 7.25 }}
steps:
  - open: /
  - click: {{ target: {{ css: "#locate" }} }}
  - assert: {{ text: {{ target: {{ css: "#position" }}, equals: "[37.7749,-122.4194,7.25]" }} }}
"##
        ),
        "geolocation.yaml",
        &BTreeMap::new(),
    )
    .expect("compile geolocation flow");
    let artifacts = tempfile::tempdir().expect("create artifact directory");
    let host = BrowserHost::launch(&chrome, false)
        .await
        .expect("launch pinned Chrome");

    let report = run_flow(&host, &flow, &RunOptions::new(artifacts.path())).await;
    let other_context = host
        .create_context(playrust::browser::Viewport::new(800, 600).unwrap(), None)
        .await
        .expect("create second isolated context");
    other_context
        .page()
        .goto(url.as_str())
        .await
        .expect("open fixture in second context");
    let permission: String = other_context
        .page()
        .evaluate("navigator.permissions.query({ name: 'geolocation' }).then(value => value.state)")
        .await
        .expect("read second context geolocation permission")
        .into_value()
        .expect("decode second context geolocation permission");
    host.dispose_context(other_context).await.unwrap();
    let shutdown = host.shutdown().await;
    server.shutdown();

    shutdown.expect("shut down Chrome cleanly");
    assert_eq!(report.status, FlowStatus::Passed, "{:#?}", report.failures);
    assert_ne!(permission, "granted");
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
        output_path: artifacts.path().join("recording.mp4"),
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
    assert!(metadata.contains("codec_name=h264"), "{metadata}");
    assert!(metadata.contains("width=800"), "{metadata}");
    assert!(metadata.contains("height=600"), "{metadata}");
}
