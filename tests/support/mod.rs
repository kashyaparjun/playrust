#![allow(dead_code)]

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use playrust::report::AggregateReport;

pub fn playrust(arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_playrust"));
    command
        .args(arguments)
        .envs(environment.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Pin the harness-resolved browser so the child uses resolution-only and
    // never enters resolve_or_install_browser's download path.
    let chrome_overridden = environment
        .iter()
        .any(|(key, _)| *key == playrust::install::CHROME_ENV);
    if !chrome_overridden
        && let Some(path) = chrome_path()
    {
        command.env(playrust::install::CHROME_ENV, path);
    }
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

// ---------------------------------------------------------------------------
// Self-skipping helpers (issue #18)
// ---------------------------------------------------------------------------

pub mod harness;

/// Returns the path to a usable Chrome for Testing binary, if available.
///
/// Resolution: `PLAYRUST_CHROME` env var → pinned version in project cache.
pub fn chrome_path() -> Option<PathBuf> {
    playrust::install::resolve_cached_browser()
}

fn ffprobe_name() -> String {
    env::var_os("PLAYRUST_FFPROBE")
        .unwrap_or_else(|| "ffprobe".into())
        .to_string_lossy()
        .into_owned()
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
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
