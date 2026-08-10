mod support;

use support::{YAML_ADVERSARIAL_FILES, assert_success, playrust, yaml_fixture_path};

struct CliOutcome {
    file: &'static str,
    expect_success: bool,
    error_substrings: &'static [&'static str],
}

/// CLI-layer expectations keyed by shared fixture names.
const CLI_OUTCOMES: &[CliOutcome] = &[
    CliOutcome {
        file: "alias_bomb.yaml",
        expect_success: false,
        error_substrings: &["alias expansion limit exceeded"],
    },
    CliOutcome {
        file: "deep_nesting.yaml",
        expect_success: false,
        error_substrings: &["unknown field"],
    },
    CliOutcome {
        file: "tabs.yaml",
        expect_success: false,
        error_substrings: &["valid YAML whitespace"],
    },
    CliOutcome {
        file: "duplicate_keys_at_depth.yaml",
        expect_success: false,
        error_substrings: &["duplicate mapping key"],
    },
    CliOutcome {
        file: "merge_keys.yaml",
        expect_success: false,
        error_substrings: &["merge keys are not allowed"],
    },
    CliOutcome {
        file: "bom_crlf.yaml",
        expect_success: true,
        error_substrings: &[],
    },
    CliOutcome {
        file: "large_scalar.yaml",
        expect_success: false,
        error_substrings: &["exceeds the maximum scalar size of 65536 bytes"],
    },
];

#[test]
fn cli_fixture_table_covers_shared_corpus() {
    let mut names: Vec<_> = CLI_OUTCOMES.iter().map(|case| case.file).collect();
    names.sort_unstable();
    let mut shared: Vec<_> = YAML_ADVERSARIAL_FILES.to_vec();
    shared.sort_unstable();
    assert_eq!(names, shared);
}

#[test]
fn playrust_check_rejects_adversarial_yaml_corpus() {
    for case in CLI_OUTCOMES {
        let path = yaml_fixture_path(case.file);
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
    let path = yaml_fixture_path("alias_bomb.yaml");
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
