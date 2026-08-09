use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream::FuturesUnordered};
use playrust::browser::BrowserHost;
use playrust::flow::{
    CompiledFlow, FlowError, VideoMode, compile_file, compile_file_with_video, discover_flow_files,
    parse_duration,
};
use playrust::install::{
    PINNED_CHROME_VERSION, install_browser, install_release, resolve_or_install_browser,
};
use playrust::report::{
    AggregateReport, ArtifactPaths, ChromiumInfo, ExitCode, Failure, FailureCategory, FlowReport,
    FlowStatus, RunnerInfo, SafeText, artifact_directory, write_aggregate_report,
    write_html_report, write_junit_report,
};
use playrust::runner::{CancellationToken, RunOptions, run_flow};
use playrust::session_protocol::{self, SessionOptions};
use playrust::video::{VideoConfig, preflight_ffmpeg};

const DEFAULT_ARTIFACTS: &str = "playrust-artifacts";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Parser)]
#[command(version, about = "Run Chromium browser flows described in YAML")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify a release archive and install its Playrust binary.
    Install(InstallArgs),
    /// Validate flows without launching Chromium.
    Check(CheckArgs),
    /// Run flows in Chromium.
    Run(RunArgs),
    /// Keep one isolated browser session open and accept foreground commands.
    Session(SessionArgs),
    /// Manage the pinned Chrome for Testing installation.
    Browser(BrowserArgs),
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// Local .tar.gz, .tgz, or .zip release archive.
    #[arg(long)]
    archive: PathBuf,
    /// Checksum file containing the archive's SHA-256 digest.
    #[arg(long)]
    checksum: PathBuf,
    /// Directory in which to install the platform binary.
    #[arg(long)]
    destination: PathBuf,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// YAML flow file or directory containing flows.
    path: PathBuf,
    /// Override a declared variable (name=value).
    #[arg(long = "var", value_name = "NAME=VALUE")]
    variables: Vec<CliVariable>,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// YAML flow file or directory containing flows.
    path: PathBuf,
    /// Show the Chromium window.
    #[arg(long)]
    headed: bool,
    /// Maximum number of flows to run concurrently.
    #[arg(long, default_value_t = default_jobs())]
    jobs: NonZeroUsize,
    /// Path to the pinned Chrome for Testing executable.
    #[arg(long)]
    browser: Option<PathBuf>,
    /// Override a declared variable (name=value).
    #[arg(long = "var", value_name = "NAME=VALUE")]
    variables: Vec<CliVariable>,
    /// Override video recording for every flow.
    #[arg(long, value_name = "MODE")]
    video: Option<VideoMode>,
    /// Path to FFmpeg; defaults to resolving ffmpeg on PATH.
    #[arg(long)]
    ffmpeg_path: Option<PathBuf>,
    /// Root directory for flow artifacts and report.json.
    #[arg(long, default_value = DEFAULT_ARTIFACTS)]
    artifacts: PathBuf,
    /// Write a JUnit XML report to <artifacts>/junit.xml.
    #[arg(long)]
    junit: bool,
    /// Write an HTML report to <artifacts>/report.html.
    #[arg(long)]
    html: bool,
}

#[derive(Debug, Args)]
struct SessionArgs {
    /// Machine protocol spoken over stdin/stdout.
    #[arg(long, value_enum)]
    protocol: SessionProtocol,
    /// Show the Chromium window.
    #[arg(long)]
    headed: bool,
    /// Path to the pinned Chrome for Testing executable.
    #[arg(long)]
    browser: Option<PathBuf>,
    /// Fixed viewport for the interactive session.
    #[arg(long, default_value = "1280x720", value_name = "WIDTHxHEIGHT")]
    viewport: SessionViewport,
    /// Default timeout for interactive actions.
    #[arg(long, default_value = "10s", value_parser = parse_session_timeout)]
    timeout: Duration,
    /// Record one continuous session video.
    #[arg(long, value_enum, default_value_t = SessionVideoMode::On)]
    video: SessionVideoMode,
    /// Native JavaScript dialog handling policy.
    #[arg(long, value_enum, default_value_t = SessionDialogPolicy::Explicit)]
    dialog_policy: SessionDialogPolicy,
    /// Path to FFmpeg for continuous session recording.
    #[arg(long)]
    ffmpeg_path: Option<PathBuf>,
    /// Root directory for session artifacts.
    #[arg(long, default_value = DEFAULT_ARTIFACTS)]
    artifacts: PathBuf,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SessionVideoMode {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SessionDialogPolicy {
    Explicit,
    Accept,
    Dismiss,
}

#[derive(Clone, Copy, Debug)]
struct SessionViewport {
    width: u32,
    height: u32,
}

impl FromStr for SessionViewport {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once('x')
            .ok_or_else(|| "viewport must use WIDTHxHEIGHT".to_owned())?;
        let width = width
            .parse::<u32>()
            .map_err(|_| "viewport width must be a positive integer".to_owned())?;
        let height = height
            .parse::<u32>()
            .map_err(|_| "viewport height must be a positive integer".to_owned())?;
        playrust::browser::Viewport::new(width, height).map_err(|error| error.to_string())?;
        Ok(Self { width, height })
    }
}

fn parse_session_timeout(value: &str) -> Result<Duration, String> {
    parse_duration("session timeout", value).map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SessionProtocol {
    Ndjson,
}

#[derive(Debug, Args)]
struct BrowserArgs {
    #[command(subcommand)]
    command: BrowserCommand,
}

#[derive(Debug, Subcommand)]
enum BrowserCommand {
    /// Download and validate the pinned Chrome for Testing build.
    Install,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CliVariable {
    name: String,
    value: String,
}

impl FromStr for CliVariable {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (name, value) = input
            .split_once('=')
            .ok_or_else(|| "expected NAME=VALUE".to_owned())?;
        let mut bytes = name.bytes();
        if !matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err("variable name must match [A-Za-z_][A-Za-z0-9_]*".to_owned());
        }
        if value.trim().is_empty() {
            return Err("variable value must not be empty".to_owned());
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }
}

struct FlowRun {
    flow: CompiledFlow,
    options: RunOptions,
}

#[tokio::main]
async fn main() {
    let exit_code = match Cli::parse().command {
        Command::Install(args) => {
            match install_release(&args.archive, &args.checksum, &args.destination) {
                Ok(path) => {
                    println!("Installed verified Playrust binary to {}", path.display());
                    ExitCode::Success
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::Infrastructure
                }
            }
        }
        Command::Check(args) => check(args),
        Command::Run(args) => run(args).await,
        Command::Session(args) => session(args).await,
        Command::Browser(BrowserArgs {
            command: BrowserCommand::Install,
        }) => match install_browser().await {
            Ok(path) => {
                println!(
                    "Installed Chrome for Testing {PINNED_CHROME_VERSION}: {}",
                    path.display()
                );
                ExitCode::Success
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::Infrastructure
            }
        },
    };
    std::process::exit(exit_code.as_i32());
}

async fn session(args: SessionArgs) -> ExitCode {
    if matches!(args.video, SessionVideoMode::On)
        && (!args.viewport.width.is_multiple_of(2) || !args.viewport.height.is_multiple_of(2))
    {
        eprintln!("error: session video requires even viewport width and height");
        return ExitCode::Specification;
    }
    let browser = match resolve_or_install_browser(args.browser.as_deref()).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::Infrastructure;
        }
    };
    match args.protocol {
        SessionProtocol::Ndjson => {
            session_protocol::run(SessionOptions {
                browser,
                headed: args.headed,
                artifacts: args.artifacts,
                ffmpeg_path: args.ffmpeg_path,
                settings: playrust::runner::SessionSettings {
                    timeout: args.timeout,
                    viewport: playrust::flow::Viewport {
                        width: args.viewport.width,
                        height: args.viewport.height,
                    },
                    geolocation: None,
                },
                video: match args.video {
                    SessionVideoMode::On => VideoMode::On,
                    SessionVideoMode::Off => VideoMode::Off,
                },
                dialog_policy: match args.dialog_policy {
                    SessionDialogPolicy::Explicit => {
                        playrust::session_dialog::DialogPolicy::Explicit
                    }
                    SessionDialogPolicy::Accept => playrust::session_dialog::DialogPolicy::Accept,
                    SessionDialogPolicy::Dismiss => playrust::session_dialog::DialogPolicy::Dismiss,
                },
            })
            .await
        }
    }
}

fn check(args: CheckArgs) -> ExitCode {
    let variables = match variable_map(args.variables) {
        Ok(variables) => variables,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::Specification;
        }
    };
    let files = match discover_flow_files(&args.path) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::Specification;
        }
    };

    let mut valid = Vec::new();
    let mut failed = false;
    for file in files {
        match compile_file(&file, &variables) {
            Ok(flow) => valid.push((file, flow.name)),
            Err(error) => {
                eprintln!("FAIL {}: {error}", file.display());
                failed = true;
            }
        }
    }
    if failed {
        return ExitCode::Specification;
    }
    for (file, name) in &valid {
        println!("OK {} ({name})", file.display());
    }
    println!("{} flow(s) valid", valid.len());
    ExitCode::Success
}

async fn run(args: RunArgs) -> ExitCode {
    let started = Instant::now();
    let variables = match variable_map(args.variables) {
        Ok(variables) => variables,
        Err(error) => {
            return finish_report(
                started,
                &args.artifacts,
                args.junit,
                args.html,
                None,
                vec![specification_report(&args.path, &args.artifacts, error)],
            );
        }
    };
    let files = match discover_flow_files(&args.path) {
        Ok(files) => files,
        Err(error) => {
            return finish_report(
                started,
                &args.artifacts,
                args.junit,
                args.html,
                None,
                vec![specification_report(
                    &args.path,
                    &args.artifacts,
                    error.to_string(),
                )],
            );
        }
    };
    let input_is_directory = args.path.is_dir();
    let mut runs = Vec::with_capacity(files.len());
    let mut compilation_failures = Vec::new();
    for file in files {
        let relative = relative_flow_path(&args.path, &file, input_is_directory);
        let artifact_directory = match artifact_directory(&args.artifacts, &relative) {
            Ok(directory) => directory,
            Err(error) => {
                compilation_failures.push(specification_report(
                    &file,
                    &args.artifacts,
                    error.to_string(),
                ));
                continue;
            }
        };
        match compile_file_for_run(&file, &variables, args.video) {
            Ok(flow) => runs.push(FlowRun {
                flow,
                options: RunOptions::new(artifact_directory),
            }),
            Err(error) => compilation_failures.push(specification_report(
                &file,
                &artifact_directory,
                error.to_string(),
            )),
        }
    }
    if !compilation_failures.is_empty() {
        compilation_failures.extend(setup_failure_reports(
            &runs,
            FailureCategory::Specification,
            "not run because another flow failed validation".to_owned(),
        ));
        return finish_report(
            started,
            &args.artifacts,
            args.junit,
            args.html,
            None,
            compilation_failures,
        );
    }

    let browser_path = match resolve_or_install_browser(args.browser.as_deref()).await {
        Ok(path) => path,
        Err(error) => {
            return finish_report(
                started,
                &args.artifacts,
                args.junit,
                args.html,
                None,
                setup_failure_reports(&runs, FailureCategory::BrowserLaunch, error.to_string()),
            );
        }
    };

    if let Some(first_video) = runs
        .iter()
        .find(|run| run.flow.settings.video != VideoMode::Off)
    {
        let ffmpeg_path = args
            .ffmpeg_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let preflight = VideoConfig {
            mode: VideoMode::On,
            ffmpeg_path: ffmpeg_path.clone(),
            output_path: args.artifacts.join("preflight.mp4"),
            viewport_width: first_video.flow.settings.viewport.width,
            viewport_height: first_video.flow.settings.viewport.height,
        };
        if let Err(error) = preflight_ffmpeg(&preflight).await {
            return finish_report(
                started,
                &args.artifacts,
                args.junit,
                args.html,
                None,
                setup_failure_reports(&runs, FailureCategory::Recording, error.to_string()),
            );
        }
        for run in &mut runs {
            if run.flow.settings.video != VideoMode::Off {
                run.options = run.options.clone().with_ffmpeg(&ffmpeg_path);
            }
        }
    }

    let host = match BrowserHost::launch(&browser_path, args.headed).await {
        Ok(host) => host,
        Err(error) => {
            return finish_report(
                started,
                &args.artifacts,
                args.junit,
                args.html,
                None,
                setup_failure_reports(&runs, FailureCategory::BrowserLaunch, error.to_string()),
            );
        }
    };
    let chromium = ChromiumInfo {
        version: host.version().product.clone(),
        executable: browser_path.to_string_lossy().into_owned(),
    };
    let (mut reports, interrupted) = execute_runs(&host, &runs, args.jobs.get()).await;
    if let Err(error) = host.shutdown().await {
        add_infrastructure_failure(&mut reports, error.to_string());
    }
    if interrupted {
        eprintln!("Interrupted");
    }
    finish_report(
        started,
        &args.artifacts,
        args.junit,
        args.html,
        Some(chromium),
        reports,
    )
}

async fn execute_runs(
    host: &BrowserHost,
    runs: &[FlowRun],
    jobs: usize,
) -> (Vec<FlowReport>, bool) {
    let cancellation = CancellationToken::new();
    let interrupt_cancellation = cancellation.clone();
    let interrupt = async move {
        let result = tokio::signal::ctrl_c().await;
        if result.is_ok() {
            interrupt_cancellation.cancel();
        }
        result
    };
    let (reports, interrupted) = execute_bounded(runs.len(), jobs, interrupt, |index| {
        let options = runs[index]
            .options
            .clone()
            .with_cancellation(cancellation.clone());
        async move { run_flow(host, &runs[index].flow, &options).await }
    })
    .await;

    let reports = complete_run_reports(runs, reports);
    (reports, interrupted)
}

fn complete_run_reports(runs: &[FlowRun], reports: Vec<Option<FlowReport>>) -> Vec<FlowReport> {
    reports
        .into_iter()
        .enumerate()
        .map(|(index, report)| report.unwrap_or_else(|| interrupted_report(&runs[index])))
        .collect()
}

async fn execute_bounded<R, I, F, Fut>(
    count: usize,
    jobs: usize,
    interrupt: I,
    mut execute: F,
) -> (Vec<Option<R>>, bool)
where
    I: Future<Output = std::io::Result<()>>,
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = R>,
{
    let mut pending = FuturesUnordered::new();
    let mut reports = std::iter::repeat_with(|| None)
        .take(count)
        .collect::<Vec<_>>();
    let mut next = 0;
    while next < count && pending.len() < jobs {
        let index = next;
        let future = execute(index);
        pending.push(indexed(index, future));
        next += 1;
    }

    let mut interrupted = false;
    let mut listen_for_interrupt = true;
    tokio::pin!(interrupt);
    while !pending.is_empty() {
        tokio::select! {
            biased;
            signal = &mut interrupt, if listen_for_interrupt => {
                listen_for_interrupt = false;
                interrupted = signal.is_ok();
            }
            Some((index, report)) = pending.next() => {
                reports[index] = Some(report);
                if !interrupted && next < count {
                    let index = next;
                    let future = execute(index);
                    pending.push(indexed(index, future));
                    next += 1;
                }
            }
        }
    }

    (reports, interrupted)
}

async fn indexed<F: Future>(index: usize, future: F) -> (usize, F::Output) {
    (index, future.await)
}

fn variable_map(variables: Vec<CliVariable>) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for variable in variables {
        if values
            .insert(variable.name.clone(), variable.value)
            .is_some()
        {
            return Err(format!(
                "variable {:?} was provided more than once",
                variable.name
            ));
        }
    }
    Ok(values)
}

fn compile_file_for_run(
    path: &Path,
    variables: &BTreeMap<String, String>,
    video: Option<VideoMode>,
) -> Result<CompiledFlow, FlowError> {
    compile_file_with_video(path, variables, video)
}

fn relative_flow_path(input: &Path, file: &Path, input_is_directory: bool) -> PathBuf {
    if input_is_directory {
        file.strip_prefix(input).unwrap_or(file).to_owned()
    } else {
        file.file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| file.to_owned())
    }
}

fn setup_failure_reports(
    runs: &[FlowRun],
    category: FailureCategory,
    message: String,
) -> Vec<FlowReport> {
    runs.iter()
        .map(|run| FlowReport {
            name: run.flow.name.clone(),
            path: run.flow.source.to_string_lossy().into_owned(),
            duration_ms: 0,
            status: FlowStatus::Failed,
            failures: vec![Failure::new(
                category,
                SafeText::public(run.flow.redactor.redact(&message)),
            )],
            warnings: recording_secret_warnings(&run.flow),
            artifacts: ArtifactPaths {
                directory: run
                    .options
                    .artifact_directory
                    .to_string_lossy()
                    .into_owned(),
                ..ArtifactPaths::default()
            },
        })
        .collect()
}

fn specification_report(file: &Path, artifacts: &Path, message: String) -> FlowReport {
    FlowReport {
        name: file
            .file_stem()
            .unwrap_or(file.as_os_str())
            .to_string_lossy()
            .into_owned(),
        path: file.to_string_lossy().into_owned(),
        duration_ms: 0,
        status: FlowStatus::Failed,
        failures: vec![Failure::new(
            FailureCategory::Specification,
            SafeText::public(message),
        )],
        warnings: Vec::new(),
        artifacts: ArtifactPaths {
            directory: artifacts.to_string_lossy().into_owned(),
            ..ArtifactPaths::default()
        },
    }
}

fn interrupted_report(run: &FlowRun) -> FlowReport {
    FlowReport {
        name: run.flow.name.clone(),
        path: run.flow.source.to_string_lossy().into_owned(),
        duration_ms: 0,
        status: FlowStatus::Interrupted,
        failures: Vec::new(),
        warnings: recording_secret_warnings(&run.flow),
        artifacts: ArtifactPaths {
            directory: run
                .options
                .artifact_directory
                .to_string_lossy()
                .into_owned(),
            ..ArtifactPaths::default()
        },
    }
}

fn recording_secret_warnings(flow: &CompiledFlow) -> Vec<SafeText> {
    flow.recording_secret_warning()
        .into_iter()
        .map(SafeText::public)
        .collect()
}

fn add_infrastructure_failure(reports: &mut [FlowReport], message: String) {
    let index = reports
        .iter_mut()
        .position(|report| report.status == FlowStatus::Passed)
        .or_else(|| (!reports.is_empty()).then_some(0));
    if let Some(report) = index.map(|index| &mut reports[index]) {
        if report.status == FlowStatus::Passed {
            report.status = FlowStatus::Failed;
        }
        report.failures.push(Failure::new(
            FailureCategory::Protocol,
            SafeText::public(format!("browser shutdown failed: {message}")),
        ));
    }
}

fn finish_report(
    started: Instant,
    artifacts: &Path,
    junit: bool,
    html: bool,
    chromium: Option<ChromiumInfo>,
    flows: Vec<FlowReport>,
) -> ExitCode {
    let report = AggregateReport::new(
        RunnerInfo {
            name: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        SCHEMA_VERSION,
        chromium,
        duration_ms(started.elapsed()),
        flows,
    );
    let exit_code = report.exit_code();
    print_results(&report);
    for name in ["report.json", "junit.xml", "report.html"] {
        let path = artifacts.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!(
                    "error: could not remove stale report {}: {error}",
                    path.display()
                );
                return ExitCode::Infrastructure;
            }
        }
    }
    if junit {
        match write_junit_report(artifacts, &report) {
            Ok(path) => println!("JUnit: {}", path.display()),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::Infrastructure;
            }
        }
    }
    if html {
        match write_html_report(artifacts, &report) {
            Ok(path) => println!("HTML: {}", path.display()),
            Err(error) => {
                if junit {
                    let _ = fs::remove_file(artifacts.join("junit.xml"));
                }
                eprintln!("error: {error}");
                return ExitCode::Infrastructure;
            }
        }
    }
    match write_aggregate_report(artifacts, &report) {
        Ok(path) => println!("Report: {}", path.display()),
        Err(error) => {
            if junit {
                let _ = fs::remove_file(artifacts.join("junit.xml"));
            }
            if html {
                let _ = fs::remove_file(artifacts.join("report.html"));
            }
            eprintln!("error: {error}");
            return ExitCode::Infrastructure;
        }
    }
    exit_code
}

fn print_results(report: &AggregateReport) {
    if let Some(chromium) = &report.chromium {
        println!("Chromium: {}", terminal_text(&chromium.version));
    }
    for report in &report.flows {
        match report.status {
            FlowStatus::Passed => println!(
                "PASS {} ({} ms)",
                terminal_text(&report.name),
                report.duration_ms
            ),
            FlowStatus::Interrupted => {
                println!("INTERRUPTED {}", terminal_text(&report.name));
            }
            FlowStatus::Failed => {
                println!("FAIL {}", terminal_text(&report.name));
                for failure in &report.failures {
                    println!(
                        "  {:?}: {}",
                        failure.category,
                        terminal_text(failure.message.as_str())
                    );
                    if let Some(step) = &failure.step {
                        println!(
                            "  Step: {} ({})",
                            step.number,
                            terminal_text(&step.operation)
                        );
                        if let Some(locator) = &step.locator {
                            println!("  Locator: {}", terminal_text(locator.as_str()));
                        }
                        if let (Some(source), Some(source_step)) = (&step.source, step.source_step)
                        {
                            println!("  Source: {} (step {source_step})", terminal_text(source));
                        }
                    }
                    if let Some(url) = &failure.current_url {
                        println!("  URL: {}", terminal_text(url.as_str()));
                    }
                    if let Some(timeout) = failure.timeout_ms {
                        println!("  Timeout: {timeout} ms");
                    }
                    if let Some(observed) = &failure.last_observed {
                        println!("  Last observed: {}", terminal_text(observed.as_str()));
                    }
                }
                if let Some(path) = &report.artifacts.failure_screenshot {
                    println!("  Screenshot: {}", terminal_text(path));
                }
                if let Some(path) = &report.artifacts.visual_actual {
                    println!("  Visual actual: {}", terminal_text(path));
                }
                if let Some(path) = &report.artifacts.visual_diff {
                    println!("  Visual diff: {}", terminal_text(path));
                }
            }
        }
        for path in &report.artifacts.screenshots {
            println!("  Screenshot: {}", terminal_text(path));
        }
        if let Some(path) = &report.artifacts.recording {
            println!("  Recording: {}", terminal_text(path));
        }
        if let Some(path) = &report.artifacts.partial_recording {
            println!("  Partial recording: {}", terminal_text(path));
        }
        for warning in &report.warnings {
            println!("  warning: {}", terminal_text(warning.as_str()));
        }
    }
}

fn terminal_text(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn default_jobs() -> NonZeroUsize {
    let jobs = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1)
        .min(4);
    NonZeroUsize::new(jobs).expect("default job count is at least one")
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn cli_variables_split_once_and_validate_names_and_values() {
        assert_eq!(
            "token=a=b".parse::<CliVariable>().unwrap(),
            CliVariable {
                name: "token".to_owned(),
                value: "a=b".to_owned(),
            }
        );
        for invalid in ["token", "1token=value", "token=", "bad-name=value"] {
            assert!(
                invalid.parse::<CliVariable>().is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn duplicate_cli_variables_are_rejected() {
        let variable = "region=west".parse::<CliVariable>().unwrap();
        assert!(variable_map(vec![variable.clone(), variable]).is_err());
    }

    #[test]
    fn session_cli_defaults_and_overrides_are_bounded() {
        let cli = Cli::try_parse_from(["playrust", "session", "--protocol", "ndjson"])
            .expect("default session CLI");
        let Command::Session(defaults) = cli.command else {
            panic!("expected session command");
        };
        assert_eq!(
            (defaults.viewport.width, defaults.viewport.height),
            (1280, 720)
        );
        assert_eq!(defaults.timeout, Duration::from_secs(10));
        assert!(matches!(defaults.video, SessionVideoMode::On));
        assert!(matches!(
            defaults.dialog_policy,
            SessionDialogPolicy::Explicit
        ));

        assert!(
            Cli::try_parse_from([
                "playrust",
                "session",
                "--protocol",
                "ndjson",
                "--viewport",
                "bad",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "playrust",
                "session",
                "--protocol",
                "ndjson",
                "--video",
                "retain-on-failure",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "playrust",
                "session",
                "--protocol",
                "ndjson",
                "--dialog-policy",
                "ignore",
            ])
            .is_err()
        );
    }

    #[test]
    fn video_override_is_applied_before_viewport_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("odd.yaml");
        std::fs::write(
            &path,
            "version: 1\nname: odd\nsettings: { viewport: { width: 801, height: 600 } }\nsteps: [{ open: https://example.test }]\n",
        )
        .unwrap();
        assert!(compile_file_with_video(&path, &BTreeMap::new(), Some(VideoMode::On)).is_err());
        assert!(compile_file_with_video(&path, &BTreeMap::new(), Some(VideoMode::Off)).is_ok());
    }

    #[test]
    fn relative_paths_preserve_stable_directory_ordering_keys() {
        assert_eq!(
            relative_flow_path(
                Path::new("flows"),
                Path::new("flows/admin/login.yaml"),
                true,
            ),
            Path::new("admin/login.yaml")
        );
        assert_eq!(
            relative_flow_path(
                Path::new("flows/login.yaml"),
                Path::new("flows/login.yaml"),
                false
            ),
            Path::new("login.yaml")
        );
    }

    #[tokio::test]
    async fn bounded_execution_preserves_order() {
        let reports = execute_bounded(4, 2, std::future::pending(), |index| async move {
            tokio::time::sleep(Duration::from_millis((3 - index) as u64)).await;
            index
        })
        .await
        .0;

        assert_eq!(reports, vec![Some(0), Some(1), Some(2), Some(3)]);
    }

    #[tokio::test]
    async fn interruption_stops_scheduling_and_drains_started_runs() {
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();
        let task_started = Arc::clone(&started);
        let task_completed = Arc::clone(&completed);
        let task_release = Arc::clone(&release);
        let task = tokio::spawn(execute_bounded(
            4,
            2,
            async move { interrupt_rx.await.map_err(std::io::Error::other) },
            move |index| {
                let started = Arc::clone(&task_started);
                let completed = Arc::clone(&task_completed);
                let release = Arc::clone(&task_release);
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    release.notified().await;
                    completed.fetch_add(1, Ordering::SeqCst);
                    ArtifactPaths {
                        directory: format!("flow-{index}"),
                        recording: Some(format!("flow-{index}/recording.mp4")),
                        ..ArtifactPaths::default()
                    }
                }
            },
        ));

        while started.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        interrupt_tx.send(()).unwrap();
        tokio::task::yield_now().await;
        release.notify_waiters();

        let (reports, interrupted) = task.await.unwrap();
        assert!(interrupted);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        assert_eq!(
            reports
                .iter()
                .map(|report| report
                    .as_ref()
                    .and_then(|report| report.recording.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                Some("flow-0/recording.mp4"),
                Some("flow-1/recording.mp4"),
                None,
                None,
            ]
        );
    }

    #[test]
    fn interrupted_run_reports_keep_started_artifacts_and_mark_only_unstarted_flows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("flow.yaml");
        std::fs::write(
            &path,
            "version: 1\nname: flow\nsteps: [{ open: https://example.test }]\n",
        )
        .unwrap();
        let flow = compile_file(&path, &BTreeMap::new()).unwrap();
        let runs = vec![
            FlowRun {
                flow: flow.clone(),
                options: RunOptions::new(directory.path().join("started")),
            },
            FlowRun {
                flow,
                options: RunOptions::new(directory.path().join("unstarted")),
            },
        ];
        let reports = complete_run_reports(
            &runs,
            vec![
                Some(FlowReport {
                    name: "flow".to_owned(),
                    path: path.to_string_lossy().into_owned(),
                    duration_ms: 42,
                    status: FlowStatus::Passed,
                    failures: Vec::new(),
                    warnings: Vec::new(),
                    artifacts: ArtifactPaths {
                        directory: directory
                            .path()
                            .join("started")
                            .to_string_lossy()
                            .into_owned(),
                        recording: Some("started/recording.mp4".to_owned()),
                        ..ArtifactPaths::default()
                    },
                }),
                None,
            ],
        );

        assert_eq!(reports[0].status, FlowStatus::Passed);
        assert_eq!(reports[0].duration_ms, 42);
        assert_eq!(
            reports[0].artifacts.recording.as_deref(),
            Some("started/recording.mp4")
        );
        assert_eq!(reports[1].status, FlowStatus::Interrupted);
        assert_eq!(
            reports[1].artifacts.directory,
            directory.path().join("unstarted").to_string_lossy()
        );
        assert_eq!(
            AggregateReport::new(
                RunnerInfo {
                    name: "playrust".to_owned(),
                    version: "test".to_owned(),
                },
                SCHEMA_VERSION,
                None,
                42,
                reports,
            )
            .exit_code(),
            ExitCode::Interrupted
        );
    }
}
