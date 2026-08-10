mod support;

use std::path::PathBuf;

use support::{assert_success, playrust};

const FIXTURE_DIR: &str = "tests/fixtures/yaml";

struct CliCase {
    file: &'static str,
    expect_success: bool,
    error_substrings: &'static [&'static str],
}

const CASES: &[CliCase] = &[
    CliCase {
        file: "alias_bomb.yaml",
        expect_success: false,
        error_substrings: &["alias expansion limit exceeded"],
    },
    CliCase {
        file: "deep_nesting.yaml",
        expect_success: false,
        error_substrings: &["unknown field"],
    },
    CliCase {
        file: "tabs.yaml",
        expect_success: false,
        error_substrings: &["valid YAML whitespace"],
    },
    CliCase {
        file: "duplicate_keys_at_depth.yaml",
        expect_success: false,
        error_substrings: &["duplicate mapping key"],
    },
    CliCase {
        file: "merge_keys.yaml",
        expect_success: false,
        error_substrings: &["merge keys are not allowed"],
    },
    CliCase {
        file: "bom_crlf.yaml",
        expect_success: true,
        error_substrings: &[],
    },
    CliCase {
        file: "large_scalar.yaml",
        expect_success: false,
        error_substrings: &["exceeds the maximum scalar size of 65536 bytes"],
    },
];

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

#[test]
fn playrust_check_rejects_adversarial_yaml_corpus() {
    for case in CASES {
        let path = fixture_path(case.file);
        let output = playrust(&["check", path.to_str().unwrap()], &[]);
        if case.expect_success {
            assert_success(&format!("check {}", case.file), &output);
            continue;
        }
        assert!(
            !output.status.success(),
            "expected check {} to fail\nstdout:\n{}\nstderr:\n{}",
            case.file,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for substring in case.error_substrings {
            assert!(
                combined.contains(substring),
                "expected {:?} in output for {}:\n{combined}",
                substring,
                case.file
            );
        }
    }
}

#[test]
fn playrust_run_rejects_alias_bomb_before_browser_launch() {
    let path = fixture_path("alias_bomb.yaml");
    let output = playrust(
        &[
            "run",
            path.to_str().unwrap(),
            "--artifacts",
            "target/tmp-yaml-adversarial-run",
        ],
        &[],
    );
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("alias expansion limit exceeded"),
        "{combined}"
    );
}
