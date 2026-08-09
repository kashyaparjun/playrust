mod support;

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use image::{ImageEncoder, Rgba, RgbaImage, codecs::png::PngEncoder};
use playrust::browser::BrowserHost;
use playrust::flow::compile_file;
use playrust::report::FlowStatus;
use playrust::runner::{RunOptions, run_flow};

const HTML: &str = r#"<!doctype html><html><head><style>*{margin:0}html{background:#102030}</style></head><body></body></html>"#;

struct Fixture {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || -> io::Result<()> {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                        match stream.read(&mut [0; 4096]) {
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
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                            HTML.len()
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_png(path: &std::path::Path, width: u32, height: u32, color: Rgba<u8>) {
    let image = RgbaImage::from_pixel(width, height, color);
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn deterministic_visual_assertion_passes_and_retains_failure_artifacts() {
    let Some(chrome) = support::require_browser(
        "deterministic_visual_assertion_passes_and_retains_failure_artifacts",
    ) else {
        return;
    };
    let fixture = Fixture::start();
    let directory = tempfile::tempdir().unwrap();
    write_png(
        &directory.path().join("baseline.png"),
        40,
        30,
        Rgba([16, 32, 48, 255]),
    );
    write_png(
        &directory.path().join("mismatch.png"),
        40,
        30,
        Rgba([17, 32, 48, 255]),
    );
    write_png(
        &directory.path().join("retry.png"),
        800,
        600,
        Rgba([16, 32, 48, 255]),
    );
    let flow_source = |baseline: &str| {
        format!(
            "version: 1\nname: visual-{baseline}\nbase_url: http://{}\nsettings: {{ viewport: {{ width: 100, height: 80 }}, video: off }}\nsteps:\n  - open: /\n  - assert:\n      screenshot:\n        baseline: {baseline}.png\n        crop: {{ x: 10, y: 10, width: 40, height: 30 }}\n",
            fixture.address
        )
    };
    let passing_path = directory.path().join("passing.yaml");
    let failing_path = directory.path().join("failing.yaml");
    let retrying_path = directory.path().join("retrying.yaml");
    std::fs::write(&passing_path, flow_source("baseline")).unwrap();
    std::fs::write(&failing_path, flow_source("mismatch")).unwrap();
    std::fs::write(
        &retrying_path,
        format!(
            "version: 1\nname: visual-retry\nbase_url: http://{}\nsettings: {{ viewport: {{ width: 800, height: 600 }}, video: off }}\nsteps:\n  - open: /\n  - evaluate: {{ script: \"document.documentElement.style.background = '#112030'; setTimeout(() => document.documentElement.style.background = '#102030', 50)\" }}\n  - retry: 10\n    assert:\n      screenshot:\n        baseline: retry.png\n        crop: {{ x: 0, y: 0, width: 800, height: 600 }}\n",
            fixture.address
        ),
    )
    .unwrap();
    let passing = compile_file(&passing_path, &BTreeMap::new()).unwrap();
    let failing = compile_file(&failing_path, &BTreeMap::new()).unwrap();
    let retrying = compile_file(&retrying_path, &BTreeMap::new()).unwrap();
    let host = BrowserHost::launch(chrome, false).await.unwrap();

    let passed = run_flow(
        &host,
        &passing,
        &RunOptions::new(directory.path().join("pass-artifacts")),
    )
    .await;
    assert_eq!(passed.status, FlowStatus::Passed, "{:#?}", passed.failures);
    let failed = run_flow(
        &host,
        &failing,
        &RunOptions::new(directory.path().join("fail-artifacts")),
    )
    .await;
    let retry_artifacts = directory.path().join("retry-artifacts");
    let retried = run_flow(&host, &retrying, &RunOptions::new(&retry_artifacts)).await;
    host.shutdown().await.unwrap();

    assert_eq!(failed.status, FlowStatus::Failed);
    assert!(
        !format!("{:?}", failed.failures).contains("mismatch.png"),
        "baseline path leaked"
    );
    let actual = failed.artifacts.visual_actual.as_deref().unwrap();
    let diff = failed.artifacts.visual_diff.as_deref().unwrap();
    assert!(std::path::Path::new(actual).is_file());
    let diff = image::open(diff).unwrap().to_rgba8();
    assert!(diff.pixels().all(|pixel| *pixel == Rgba([255, 0, 0, 255])));
    assert_eq!(
        retried.status,
        FlowStatus::Passed,
        "{:#?}",
        retried.failures
    );
    assert_eq!(retried.artifacts.visual_actual, None);
    assert_eq!(retried.artifacts.visual_diff, None);
    assert!(!retry_artifacts.join("__visual-3-actual.png").exists());
    assert!(!retry_artifacts.join("__visual-3-diff.png").exists());
}
