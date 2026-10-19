mod support;

use std::fs;

use playrust::report::StepStatus;
use support::{FixtureServer, playrust, read_report, video_duration};

const FIXTURE: &str = r#"<!doctype html>
<html><body>
<textarea aria-label="Message"></textarea>
<button onclick="document.querySelector('p').textContent = 'sent'">Send message</button>
<p></p>
</body></html>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME and FFmpeg"]
fn report_times_each_step_against_the_recording() {
    let server = FixtureServer::start(&[("/", "text/html; charset=utf-8", FIXTURE)]);
    let base_url = server.url();
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("steps.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: step-timings\nbase_url: {}\nsettings: {{ video: on }}\nsteps:\n  - open: /\n  - fill: {{ target: {{ css: textarea }}, value: typed-canary-value }}\n  - click: {{ target: {{ role: {{ value: button, name: Send message }} }} }}\n",
            base_url
        ),
    )
    .expect("write flow fixture");

    let output = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report = read_report(&artifacts);
    assert_eq!(report.report_version, playrust::report::REPORT_VERSION);
    let flow_report = &report.flows[0];
    let steps = &flow_report.steps;
    assert_eq!(
        steps
            .iter()
            .map(|step| (step.index, step.label.as_str(), step.status))
            .collect::<Vec<_>>(),
        [
            (1, format!("open {base_url}/").as_str(), StepStatus::Passed),
            (2, "fill css=\"textarea\"", StepStatus::Passed),
            (
                3,
                "click role=\"button\" name=\"Send message\"",
                StepStatus::Passed
            ),
        ]
    );
    for pair in steps.windows(2) {
        assert!(
            pair[0].started_ms + pair[0].duration_ms <= pair[1].started_ms + 1,
            "steps overlap: {steps:?}"
        );
        assert!(pair[0].started_ms < pair[1].started_ms, "{steps:?}");
    }
    let recording = flow_report
        .artifacts
        .recording
        .as_deref()
        .expect("recording path");
    let last = steps.last().unwrap();
    assert!(
        u128::from(last.started_ms + last.duration_ms)
            <= video_duration(recording.as_ref()).as_millis() + 100,
        "last step ends after the recording: {steps:?}"
    );
    let json = fs::read_to_string(artifacts.join("report.json")).expect("read report");
    assert!(!json.contains("typed-canary-value"), "{json}");
}
