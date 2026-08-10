#![allow(dead_code)]

//! Shared e2e helpers.
//!
//! Skip notices use `eprintln!`. Enable `--show-output` (or `--nocapture`) on
//! `cargo test` so skips remain visible when tests pass.

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use playrust::install;
use playrust::report::AggregateReport;
use serde_json::Value;

/// Resolve Chrome: `PLAYRUST_CHROME` env var, then the Playrust cache. Never downloads.
pub fn chrome_path() -> Option<PathBuf> {
    install::resolve_existing_browser()
}

/// True when `name` is set to a truthy value (not `0`/`false`/`no`/empty).
pub fn env_flag_enabled(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| {
        !matches!(
            value.to_str(),
            Some("0") | Some("false") | Some("no") | Some("")
        )
    })
}

fn skip_or_fail(test_name: &str, message: &str) {
    if env_flag_enabled("PLAYRUST_REQUIRE_BROWSER") {
        panic!("{test_name}: {message}");
    }
    eprintln!("SKIP {test_name}: {message}");
}

/// Returns Chrome when available; otherwise prints a skip notice or fails when
/// `PLAYRUST_REQUIRE_BROWSER=1`.
pub fn require_browser(test_name: &str) -> Option<PathBuf> {
    match chrome_path() {
        Some(path) => Some(path),
        None => {
            skip_or_fail(
                test_name,
                "no Chrome available (set PLAYRUST_CHROME or run `playrust browser install`)",
            );
            None
        }
    }
}

fn command_exists(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Returns ffmpeg when available; otherwise prints a skip notice or fails when
/// `PLAYRUST_REQUIRE_BROWSER=1`.
pub fn require_ffmpeg(test_name: &str) -> Option<String> {
    let path = ffmpeg_path();
    if command_exists(&path) {
        return Some(path);
    }
    skip_or_fail(
        test_name,
        "ffmpeg not available (set PLAYRUST_FFMPEG or install ffmpeg)",
    );
    None
}

/// Returns ffprobe when available; otherwise prints a skip notice or fails when
/// `PLAYRUST_REQUIRE_BROWSER=1`.
pub fn require_ffprobe(test_name: &str) -> Option<String> {
    let path = env::var_os("PLAYRUST_FFPROBE")
        .unwrap_or_else(|| "ffprobe".into())
        .to_string_lossy()
        .into_owned();
    if command_exists(&path) {
        return Some(path);
    }
    skip_or_fail(
        test_name,
        "ffprobe not available (set PLAYRUST_FFPROBE or install ffmpeg)",
    );
    None
}

/// Gate for live-network tests (Wikipedia). Set `PLAYRUST_LIVE_E2E=1` to run.
pub fn require_live_e2e(test_name: &str) -> Option<()> {
    if env_flag_enabled("PLAYRUST_LIVE_E2E") {
        return Some(());
    }
    skip_or_fail(
        test_name,
        "set PLAYRUST_LIVE_E2E=1 to run live network tests",
    );
    None
}

/// NDJSON session protocol harness shared by session / prerequisites e2e tests.
pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    pub fn start(artifacts: &Path, chrome: &Path) -> Self {
        Self::start_with_video(artifacts, "off", chrome)
    }

    pub fn start_recorded(artifacts: &Path, chrome: &Path) -> Self {
        Self::start_with_video(artifacts, "on", chrome)
    }

    pub fn start_with_video(artifacts: &Path, video: &str, chrome: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_playrust"))
            .args([
                "session",
                "--protocol",
                "ndjson",
                "--browser",
                chrome.to_str().expect("UTF-8 Chrome path"),
                "--artifacts",
                artifacts.to_str().expect("UTF-8 artifact path"),
                "--video",
                video,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn playrust session");
        let stdin = child.stdin.take().expect("session stdin");
        let stdout = BufReader::new(child.stdout.take().expect("session stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    pub fn send(&mut self, value: Value) {
        serde_json::to_writer(self.stdin.as_mut().expect("session stdin open"), &value)
            .expect("write session request");
        self.send_raw(b"\n");
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("session stdin open");
        stdin.write_all(bytes).expect("write session bytes");
        stdin.flush().expect("flush session stdin");
    }

    pub fn read(&mut self) -> Value {
        self.read_optional()
            .expect("session closed before responding")
    }

    pub fn read_optional(&mut self) -> Option<Value> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read session response");
        (!line.is_empty()).then(|| serde_json::from_str(&line).expect("session response JSON"))
    }

    pub fn command(&mut self, value: Value) -> Value {
        self.send(value);
        self.read()
    }

    pub fn close_input(&mut self) {
        drop(self.stdin.take());
    }

    pub fn finish(mut self) -> Output {
        drop(self.stdin.take());
        self.child.wait_with_output().expect("wait for session")
    }
}

pub fn assert_exit(output: Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn chrome_env(chrome: &Path) -> (String, String) {
    (
        install::CHROME_ENV.to_owned(),
        chrome.to_string_lossy().into_owned(),
    )
}

pub fn playrust(arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_playrust"));
    command
        .args(arguments)
        .envs(environment.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    output_with_timeout(command, Duration::from_secs(180))
}

pub fn assert_success(command: &str, output: &Output) {
    assert!(
        output.status.success(),
        "playrust {command} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub fn read_report(artifacts: &Path) -> AggregateReport {
    serde_json::from_slice(&fs::read(artifacts.join("report.json")).expect("read JSON report"))
        .expect("decode JSON report")
}

pub fn assert_png(path: &Path, expected: (u32, u32)) {
    let png = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        (
            u32::from_be_bytes(png[16..20].try_into().unwrap()),
            u32::from_be_bytes(png[20..24].try_into().unwrap()),
        ),
        expected,
    );
}

pub fn assert_h264_video(path: &Path) {
    let ffprobe = env::var_os("PLAYRUST_FFPROBE").unwrap_or_else(|| "ffprobe".into());
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert_success("ffprobe", &output);
    let metadata = String::from_utf8_lossy(&output.stdout);
    assert!(metadata.contains("codec_name=h264"), "{metadata}");
    assert!(metadata.contains("width=1280"), "{metadata}");
    assert!(metadata.contains("height=720"), "{metadata}");
}

pub fn video_duration(path: &Path) -> Duration {
    let ffprobe = env::var_os("PLAYRUST_FFPROBE").unwrap_or_else(|| "ffprobe".into());
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert_success("ffprobe", &output);
    let seconds: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("numeric video duration");
    Duration::from_secs_f64(seconds)
}

pub fn ffmpeg_path() -> String {
    env::var_os("PLAYRUST_FFMPEG")
        .unwrap_or_else(|| "ffmpeg".into())
        .to_string_lossy()
        .into_owned()
}

pub struct FixtureServer {
    pub url: String,
    pub address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

impl FixtureServer {
    pub fn start(routes: &[(&str, &str, &str)]) -> Self {
        let routes = routes
            .iter()
            .map(|(path, content_type, body)| {
                (
                    (*path).to_owned(),
                    (*content_type).to_owned(),
                    (*body).to_owned(),
                )
            })
            .collect::<Vec<_>>();
        Self::start_owned(move |request| {
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            routes
                .iter()
                .find(|(route, _, _)| route == path)
                .map(|(_, content_type, body)| (200, content_type.clone(), body.clone()))
                .unwrap_or_else(|| (404, "text/plain".to_owned(), "not found".to_owned()))
        })
    }

    pub fn start_with(
        responder: impl Fn(&str) -> (u16, &'static str, &'static str) + Send + Sync + 'static,
    ) -> Self {
        Self::start_owned(move |request| {
            let (status, content_type, body) = responder(request);
            (status, content_type.to_owned(), body.to_owned())
        })
    }

    fn start_owned(
        responder: impl Fn(&str) -> (u16, String, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("read fixture address");
        listener
            .set_nonblocking(true)
            .expect("make fixture server stoppable");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let responder = Arc::new(responder);
        let thread = thread::spawn(move || -> io::Result<()> {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let responder = Arc::clone(&responder);
                        thread::spawn(move || -> io::Result<()> {
                            stream.set_nonblocking(false)?;
                            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                            let mut request = [0; 8192];
                            let length = match stream.read(&mut request) {
                                Ok(0) => return Ok(()),
                                Ok(length) => length,
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                    ) =>
                                {
                                    return Ok(());
                                }
                                Err(error) => return Err(error),
                            };
                            let request = String::from_utf8_lossy(&request[..length]);
                            let (status, content_type, body) = responder(&request);
                            let _ = write!(
                                stream,
                                "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            Ok(())
                        });
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
            url: format!("http://{address}"),
            address,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .expect("fixture server thread missing")
            .join()
            .expect("fixture server thread panicked")
            .expect("serve fixture");
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> Output {
    let mut child = command.spawn().expect("run playrust");
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll playrust").is_some() {
            return child.wait_with_output().expect("collect playrust output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed out output");
            panic!(
                "playrust timed out\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}
