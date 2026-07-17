#![allow(dead_code)]

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use playrust::report::AggregateReport;

pub struct FixtureServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

impl FixtureServer {
    pub fn start(html: &'static str) -> Self {
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

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn shutdown(mut self) {
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

pub fn assert_vp9_video(path: &Path) {
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
    assert!(metadata.contains("codec_name=vp9"), "{metadata}");
    assert!(metadata.contains("width=1280"), "{metadata}");
    assert!(metadata.contains("height=720"), "{metadata}");
}

pub fn ffmpeg_path() -> String {
    env::var_os("PLAYRUST_FFMPEG")
        .unwrap_or_else(|| "ffmpeg".into())
        .to_string_lossy()
        .into_owned()
}

pub struct FixtureServer {
    pub url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

impl FixtureServer {
    pub fn start(routes: &'static [(&'static str, &'static str, &'static str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("read fixture address");
        listener
            .set_nonblocking(true)
            .expect("make server stoppable");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || -> io::Result<()> {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false)?;
                        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                        let mut request = [0; 4096];
                        let length = stream.read(&mut request)?;
                        let path = std::str::from_utf8(&request[..length])
                            .ok()
                            .and_then(|request| request.lines().next())
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/");
                        let (status, content_type, body) = routes
                            .iter()
                            .find(|(route, _, _)| *route == path)
                            .map(|(_, content_type, body)| ("200 OK", *content_type, *body))
                            .unwrap_or(("404 Not Found", "text/plain", "not found"));
                        write!(
                            stream,
                            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
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
            url: format!("http://{address}"),
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("fixture server thread panicked")
                .expect("serve fixture");
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
