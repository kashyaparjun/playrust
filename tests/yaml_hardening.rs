//! Adversarial YAML corpus and property tests for the serde-saphyr trust boundary.

use playrust::flow::{
    FlowError, MAX_FLOW_SOURCE_BYTES, MAX_SCALAR_BYTES, compile_yaml_with_env, parse_yaml,
};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE_DIR: &str = "tests/fixtures/yaml";
const PARSE_TIMEOUT: Duration = Duration::from_secs(5);
const FIXTURE_WALL_CLOCK_BOUND: Duration = Duration::from_secs(2);

struct FixtureCase {
    file: &'static str,
    expect_ok: bool,
    error_substrings: &'static [&'static str],
}

const FIXTURES: &[FixtureCase] = &[
    FixtureCase {
        file: "alias_bomb.yaml",
        expect_ok: false,
        error_substrings: &["alias expansion limit exceeded"],
    },
    FixtureCase {
        file: "deep_nesting.yaml",
        expect_ok: false,
        error_substrings: &["unknown field", "recursion limit exceeded"],
    },
    FixtureCase {
        file: "large_scalar.yaml",
        expect_ok: true,
        error_substrings: &[],
    },
    FixtureCase {
        file: "tabs.yaml",
        expect_ok: false,
        error_substrings: &["valid YAML whitespace"],
    },
    FixtureCase {
        file: "bom_crlf.yaml",
        expect_ok: true,
        error_substrings: &[],
    },
    FixtureCase {
        file: "duplicate_keys_at_depth.yaml",
        expect_ok: false,
        error_substrings: &["duplicate mapping key"],
    },
    FixtureCase {
        file: "merge_keys.yaml",
        expect_ok: false,
        error_substrings: &["merge keys are not allowed"],
    },
];

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

fn read_fixture(name: &str) -> String {
    let bytes = fs::read(fixture_path(name)).unwrap_or_else(|error| {
        panic!("failed to read fixture {name}: {error}");
    });
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).unwrap_or_else(|error| {
        panic!("fixture {name} is not valid UTF-8: {error}");
    })
}

enum BoundedParse {
    Ok,
    YamlError(String),
    Panicked(String),
    TimedOut,
}

fn parse_fixture_bounded(source: &str, timeout: Duration) -> (BoundedParse, Duration) {
    let source = source.to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| parse_yaml(&source)));
        let _ = tx.send(outcome);
    });

    let started = Instant::now();
    let bounded = match rx.recv_timeout(timeout) {
        Ok(Ok(Ok(_))) => BoundedParse::Ok,
        Ok(Ok(Err(error))) => BoundedParse::YamlError(match error {
            FlowError::Yaml(message) => message,
            other => other.to_string(),
        }),
        Ok(Err(panic)) => BoundedParse::Panicked(format!("{panic:?}")),
        Err(mpsc::RecvTimeoutError::Timeout) => BoundedParse::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            BoundedParse::Panicked("parser thread exited without reporting".to_owned())
        }
    };
    (bounded, started.elapsed())
}

fn assert_structured_yaml_error(message: &str, expected: &[&str]) {
    assert!(
        message.starts_with("error:") || message.contains("invalid YAML"),
        "expected structured YAML error, got: {message}"
    );
    assert!(
        expected.iter().any(|fragment| message.contains(fragment)),
        "expected one of {expected:?} in error: {message}"
    );
}

#[test]
fn adversarial_yaml_corpus_is_bounded_and_structured() {
    for case in FIXTURES {
        let source = read_fixture(case.file);
        let (outcome, elapsed) = parse_fixture_bounded(&source, PARSE_TIMEOUT);

        assert!(
            elapsed < FIXTURE_WALL_CLOCK_BOUND,
            "{} exceeded wall-clock bound of {:?}: took {:?}",
            case.file,
            FIXTURE_WALL_CLOCK_BOUND,
            elapsed
        );

        match outcome {
            BoundedParse::TimedOut => {
                panic!("{} timed out after {:?}", case.file, PARSE_TIMEOUT);
            }
            BoundedParse::Panicked(message) => {
                panic!("{} panicked: {message}", case.file);
            }
            BoundedParse::Ok => {
                assert!(
                    case.expect_ok,
                    "{} unexpectedly parsed successfully",
                    case.file
                );
            }
            BoundedParse::YamlError(message) => {
                assert!(
                    !case.expect_ok,
                    "{} unexpectedly failed to parse: {message}",
                    case.file
                );
                assert_structured_yaml_error(&message, case.error_substrings);
            }
        }
    }
}

#[test]
fn large_scalar_fixture_exceeds_compile_limit_without_panicking() {
    let source = read_fixture("large_scalar.yaml");
    let result = compile_yaml_with_env(
        &source,
        "large_scalar.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let message = result.unwrap_err().to_string();
    assert!(message.contains("maximum scalar size"));
    assert!(source.len() > MAX_SCALAR_BYTES);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_bytes_never_panic_parse_yaml(bytes in prop::collection::vec(any::<u8>(), 0..=MAX_FLOW_SOURCE_BYTES)) {
        let source = String::from_utf8_lossy(&bytes);
        let outcome = catch_unwind(AssertUnwindSafe(|| parse_yaml(source.as_ref())));
        prop_assert!(outcome.is_ok(), "parse_yaml panicked on arbitrary input");
    }

    #[test]
    fn duplicate_keys_are_always_rejected(
        first in prop::string::string_regex("[a-z][a-z0-9_-]{0,31}").unwrap(),
        suffix in prop::string::string_regex("[0-9]{1,4}").unwrap(),
    ) {
        let second = format!("{first}-{suffix}");
        let source = format!(
            "version: 1\nname: {first}\nname: {second}\nsteps: [{{ open: https://x.test }}]\n"
        );
        let result = parse_yaml(&source);
        prop_assert!(result.is_err(), "duplicate top-level keys must be rejected");
        if let Err(FlowError::Yaml(message)) = result {
            prop_assert!(message.contains("duplicate mapping key"), "unexpected error: {message}");
        }
    }

    #[test]
    fn merge_keys_are_always_rejected(suffix in prop::string::string_regex("[a-z]{1,16}").unwrap()) {
        let source = format!(
            "version: 1\nname: {suffix}\n<<: {{ settings: {{ timeout: 30s }} }}\nsteps: [{{ open: https://x.test }}]\n"
        );
        let result = parse_yaml(&source);
        prop_assert!(result.is_err(), "merge keys must be rejected");
        if let Err(FlowError::Yaml(message)) = result {
            prop_assert!(
                message.contains("merge keys are not allowed"),
                "unexpected error: {message}"
            );
        }
    }

    #[test]
    fn over_limit_inputs_are_rejected_before_parsing(
        extra in 1usize..=4096,
    ) {
        let source = "x".repeat(MAX_FLOW_SOURCE_BYTES + extra);
        let result = compile_yaml_with_env(&source, "oversize.yaml", &BTreeMap::new(), &BTreeMap::new());
        prop_assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        prop_assert!(message.contains("flow source exceeds the maximum size"));
    }

    #[test]
    fn validish_flows_round_trip_without_panic(
        name in prop::string::string_regex("[a-z][a-z0-9_-]{0,31}").unwrap(),
        repeat in 1usize..=8,
    ) {
        let mut steps = String::new();
        for index in 0..repeat {
            steps.push_str(&format!("  - open: https://example-{index}.test/\n"));
        }
        let source = format!("version: 1\nname: {name}\nsteps:\n{steps}");
        let parse_outcome = catch_unwind(AssertUnwindSafe(|| parse_yaml(&source)));
        prop_assert!(parse_outcome.is_ok(), "parse_yaml panicked on valid-ish flow");
        let parsed = parse_outcome.unwrap().expect("valid-ish flow should parse");
        prop_assert_eq!(parsed.name, name);
        prop_assert_eq!(parsed.steps.len(), repeat);

        let compile_outcome = catch_unwind(AssertUnwindSafe(|| {
            compile_yaml_with_env(&source, "validish.yaml", &BTreeMap::new(), &BTreeMap::new())
        }));
        prop_assert!(compile_outcome.is_ok(), "compile_yaml_with_env panicked");
        prop_assert!(compile_outcome.unwrap().is_ok(), "valid-ish flow should compile");
    }
}
