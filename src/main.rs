use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use futures_util::{StreamExt, stream::FuturesUnordered};
use playrust::browser::BrowserHost;
use playrust::flow::{
    CompiledFlow, FlowError, RawFlow, VideoMode, compile_file, compile_raw, discover_flow_files,
    parse_yaml,
};
use playrust::install::{PINNED_CHROME_VERSION, install_browser, resolve_or_install_browser};
use playrust::report::{
    AggregateReport, ArtifactPaths, ChromiumInfo, ExitCode, Failure, FailureCategory, FlowReport,
    FlowStatus, RunnerInfo, SafeText, artifact_directory, write_aggregate_report,
};
use playrust::runner::{CancellationToken, RunOptions, run_flow};
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
    /// Validate flows without launching Chromium.
    Check(CheckArgs),
    /// Run flows in Chromium.
    Run(RunArgs),
    /// Manage the pinned Chrome for Testing installation.
    Browser(BrowserArgs),
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
        Command::Check(args) => check(args),
        Command::Run(args) => run(args).await,
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
        return finish_report(started, &args.artifacts, None, compilation_failures);
    }

    let browser_path = match resolve_or_install_browser(args.browser.as_deref()).await {
        Ok(path) => path,
        Err(error) => {
            return finish_report(
                started,
                &args.artifacts,
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
            output_path: args.artifacts.join("preflight.webm"),
            viewport_width: first_video.flow.settings.viewport.width,
            viewport_height: first_video.flow.settings.viewport.height,
        };
        if let Err(error) = preflight_ffmpeg(&preflight).await {
            return finish_report(
                started,
                &args.artifacts,
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
    finish_report(started, &args.artifacts, Some(chromium), reports)
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

    let reports = reports
        .into_iter()
        .enumerate()
        .map(|(index, report)| report.unwrap_or_else(|| interrupted_report(&runs[index])))
        .collect();
    (reports, interrupted)
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
                if !interrupted {
                    reports[index] = Some(report);
                    if next < count {
                        let index = next;
                        let future = execute(index);
                        pending.push(indexed(index, future));
                        next += 1;
                    }
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
    let source = fs::read_to_string(path).map_err(|source| FlowError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut raw = parse_yaml(&source)?;
    apply_video_override(&mut raw, video);
    compile_raw(raw, path, variables, &std::env::vars().collect())
}

fn apply_video_override(flow: &mut RawFlow, video: Option<VideoMode>) {
    if let Some(video) = video {
        flow.settings.video = Some(video);
    }
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
    match write_aggregate_report(artifacts, &report) {
        Ok(path) => println!("Report: {}", path.display()),
        Err(error) => {
            eprintln!("error: {error}");
            return if exit_code == ExitCode::Interrupted {
                ExitCode::Interrupted
            } else {
                ExitCode::Infrastructure
            };
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
                if let Some(path) = &report.artifacts.recording {
                    println!("  Recording: {}", terminal_text(path));
                }
                if let Some(path) = &report.artifacts.partial_recording {
                    println!("  Partial recording: {}", terminal_text(path));
                }
            }
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
    use playrust::flow::{compile_raw, parse_yaml};
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
    fn video_override_is_applied_before_viewport_validation() {
        let mut flow = parse_yaml(
            "version: 1\nname: odd\nsettings: { viewport: { width: 801, height: 600 } }\nsteps: [{ open: https://example.test }]\n",
        )
        .unwrap();
        apply_video_override(&mut flow, Some(VideoMode::On));
        assert!(compile_raw(flow.clone(), "odd.yaml", &BTreeMap::new(), &BTreeMap::new()).is_err());
        apply_video_override(&mut flow, Some(VideoMode::Off));
        assert!(compile_raw(flow, "odd.yaml", &BTreeMap::new(), &BTreeMap::new()).is_ok());
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
                    index
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
        assert_eq!(reports, vec![None, None, None, None]);
    }
}
