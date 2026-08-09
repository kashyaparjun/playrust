use std::future::Future;
use std::path::PathBuf;

use libtest_mimic::{Arguments, Completion, Failed, Trial};

use super::{chrome_path, command_exists, ffmpeg_path, ffprobe_name};

const BROWSER_MISSING: &str = "requires PLAYRUST_CHROME to point to the pinned Chrome executable";
const FFMPEG_MISSING: &str = "requires FFmpeg and ffprobe";
const LIVE_E2E_MISSING: &str = "requires PLAYRUST_LIVE_E2E=1";

pub fn run(trials: Vec<Trial>) -> ! {
    libtest_mimic::run(&Arguments::from_args(), trials).exit();
}

pub fn plain_trial(name: impl Into<String>, runner: fn() -> Result<(), Failed>) -> Trial {
    Trial::test(name, runner)
}

pub fn browser_cli_trial(name: impl Into<String>, runner: fn() -> Result<(), Failed>) -> Trial {
    Trial::ignorable_test(name, move || match chrome() {
        Ok(_) => runner().map(|()| Completion::Completed),
        Err(outcome) => outcome,
    })
}

pub fn browser_trial(name: impl Into<String>, runner: fn(PathBuf) -> Result<(), Failed>) -> Trial {
    Trial::ignorable_test(name, move || match chrome() {
        Ok(path) => runner(path).map(|()| Completion::Completed),
        Err(outcome) => outcome,
    })
}

pub fn async_browser_trial<F>(name: impl Into<String>, runner: fn(PathBuf) -> F) -> Trial
where
    F: Future<Output = Result<(), Failed>> + Send + 'static,
{
    Trial::ignorable_test(name, move || match chrome() {
        Ok(path) => current_thread_runtime()
            .block_on(runner(path))
            .map(|()| Completion::Completed),
        Err(outcome) => outcome,
    })
}

pub fn async_browser_video_trial<F>(
    name: impl Into<String>,
    runner: fn(PathBuf, PathBuf) -> F,
) -> Trial
where
    F: Future<Output = Result<(), Failed>> + Send + 'static,
{
    Trial::ignorable_test(name, move || match chrome_and_ffmpeg() {
        Ok((chrome, ffmpeg)) => current_thread_runtime()
            .block_on(runner(chrome, ffmpeg))
            .map(|()| Completion::Completed),
        Err(outcome) => outcome,
    })
}

pub fn browser_video_cli_trial(
    name: impl Into<String>,
    runner: fn(PathBuf, PathBuf) -> Result<(), Failed>,
) -> Trial {
    Trial::ignorable_test(name, move || match chrome_and_ffmpeg() {
        Ok((chrome, ffmpeg)) => runner(chrome, ffmpeg).map(|()| Completion::Completed),
        Err(outcome) => outcome,
    })
}

pub fn live_wikipedia_trial(name: impl Into<String>, runner: fn() -> Result<(), Failed>) -> Trial {
    Trial::ignorable_test(name, move || {
        if std::env::var("PLAYRUST_LIVE_E2E").as_deref() != Ok("1") {
            return missing(LIVE_E2E_MISSING);
        }
        match chrome_and_ffmpeg() {
            Ok(_) => runner().map(|()| Completion::Completed),
            Err(outcome) => outcome,
        }
    })
}

fn chrome() -> Result<PathBuf, Result<Completion, Failed>> {
    match chrome_path() {
        Some(path) => Ok(path),
        None => Err(missing(BROWSER_MISSING)),
    }
}

fn chrome_and_ffmpeg() -> Result<(PathBuf, PathBuf), Result<Completion, Failed>> {
    let chrome = chrome()?;
    let path = ffmpeg_path();
    if command_exists(&path) && command_exists(&ffprobe_name()) {
        return Ok((chrome, PathBuf::from(path)));
    }
    Err(missing(FFMPEG_MISSING))
}

fn missing(reason: &'static str) -> Result<Completion, Failed> {
    playrust::install::escalate_missing_prerequisite(reason);
    Ok(Completion::ignored_with(reason))
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}
