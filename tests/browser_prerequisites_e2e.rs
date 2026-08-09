//! Proves self-skipping e2e reporting via an isolated subprocess.
//!
//! Nested cargo invocations clear Chrome resolution (`HOME` / `XDG_CACHE_HOME` /
//! `PLAYRUST_CHROME`) so these assertions hold even when the parent machine has
//! a pinned browser installed.

mod support;

use std::path::PathBuf;
use std::process::Command;

use libtest_mimic::Failed;
use support::harness;
use tempfile::TempDir;

const PROBE: &str = "swipes_once_from_an_actionable_target";

fn main() {
    harness::run(vec![
        harness::plain_trial(
            "missing_chrome_is_reported_as_ignored",
            missing_chrome_is_reported_as_ignored,
        ),
        harness::plain_trial(
            "strict_mode_fails_without_chrome",
            strict_mode_fails_without_chrome,
        ),
    ]);
}

fn missing_chrome_is_reported_as_ignored() -> Result<(), Failed> {
    let output = run_swipe_probe(false)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("expected exit 0 when Chrome is missing\n{stdout}\n{stderr}").into());
    }
    if !(stdout.contains("1 ignored") || stdout.contains("ignored (")) {
        return Err(format!("expected ignored summary\n{stdout}\n{stderr}").into());
    }
    if stdout.contains("1 passed") && !stdout.contains("0 passed") {
        return Err(format!("ignored run must not report a pass\n{stdout}").into());
    }
    Ok(())
}

fn strict_mode_fails_without_chrome() -> Result<(), Failed> {
    let output = run_swipe_probe(true)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        return Err(format!(
            "PLAYRUST_REQUIRE_E2E=1 must fail when Chrome is missing\n{stdout}\n{stderr}"
        )
        .into());
    }
    let combined = format!("{stdout}{stderr}");
    if !combined.contains("PLAYRUST_REQUIRE_E2E=1") {
        return Err(format!("expected strict-mode panic text\n{combined}").into());
    }
    Ok(())
}

fn run_swipe_probe(require_e2e: bool) -> Result<std::process::Output, Failed> {
    let isolation = isolated_home()?;
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "test",
            "--locked",
            "--test",
            "swipe_e2e",
            "--",
            "--exact",
            PROBE,
        ])
        .env("HOME", isolation.path())
        .env("USERPROFILE", isolation.path())
        .env("XDG_CACHE_HOME", isolation.path().join("cache"))
        .env_remove("PLAYRUST_CHROME")
        .env_remove("PLAYRUST_REQUIRE_E2E")
        .env_remove("PLAYRUST_LIVE_E2E");
    if require_e2e {
        command.env(playrust::install::REQUIRE_E2E_ENV, "1");
    }
    // Keep the TempDir alive until the child exits.
    let output = command
        .output()
        .map_err(|error| Failed::from(error.to_string()))?;
    drop(isolation);
    Ok(output)
}

fn isolated_home() -> Result<TempDir, Failed> {
    let directory = tempfile::tempdir().map_err(|error| Failed::from(error.to_string()))?;
    // Ensure ProjectDirs cannot fall back to a parent cache by accident.
    let marker = PathBuf::from(directory.path());
    if !marker.exists() {
        return Err("temp home missing".into());
    }
    Ok(directory)
}
