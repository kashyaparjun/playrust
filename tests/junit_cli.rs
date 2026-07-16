use std::fs;
use std::process::Command;

#[test]
fn junit_is_written_for_invalid_yaml_when_requested() {
    let directory = tempfile::tempdir().unwrap();
    let flow = directory.path().join("invalid.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, "not: [valid").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args(["run", flow.to_str().unwrap(), "--junit", "--artifacts"])
        .arg(&artifacts)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(artifacts.join("report.json").is_file());
    let xml = fs::read_to_string(artifacts.join("junit.xml")).unwrap();
    assert!(xml.contains("errors=\"1\""));
    assert!(xml.contains("<error type=\"specification\""));
}

#[test]
fn junit_is_opt_in() {
    let directory = tempfile::tempdir().unwrap();
    let flow = directory.path().join("invalid.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(&flow, "not: [valid").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args(["run", flow.to_str().unwrap(), "--artifacts"])
        .arg(&artifacts)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(artifacts.join("report.json").is_file());
    assert!(!artifacts.join("junit.xml").exists());
}
