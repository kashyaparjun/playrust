use std::fs;
use std::process::Command;

#[test]
fn html_is_written_for_invalid_yaml_when_requested() {
    let directory = tempfile::tempdir().unwrap();
    let flow = directory.path().join("invalid.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, "not: [valid").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args(["run", flow.to_str().unwrap(), "--html", "--artifacts"])
        .arg(&artifacts)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(artifacts.join("report.json").is_file());
    let html = fs::read_to_string(artifacts.join("report.html")).unwrap();
    assert!(html.contains("<span class=\"badge failed\">Failed</span>"));
    assert!(html.contains("<h3>specification</h3>"));
}

#[test]
fn html_is_opt_in() {
    let directory = tempfile::tempdir().unwrap();
    let flow = directory.path().join("invalid.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, "not: [valid").unwrap();
    fs::create_dir(&artifacts).unwrap();
    fs::write(artifacts.join("report.html"), "stale").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args(["run", flow.to_str().unwrap(), "--artifacts"])
        .arg(&artifacts)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(artifacts.join("report.json").is_file());
    assert!(!artifacts.join("report.html").exists());
}
