use super::*;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};

#[test]
fn presentation_overlays_parse_with_independent_options() {
    let flow = compile(
        "version: 1\nname: overlays\nsettings:\n  video: on\n  overlays: { step: true, url: true, pointer: true }\nsteps:\n  - open: https://example.com/\n",
    )
    .unwrap();

    assert_eq!(
        flow.settings.overlays,
        PresentationOverlays {
            step: true,
            url: true,
            pointer: true,
        }
    );
}

#[test]
fn presentation_overlays_default_to_disabled() {
    let flow = compile("version: 1\nname: plain\nsettings: { video: off }\nsteps:\n  - open: https://example.com/\n").unwrap();
    assert_eq!(flow.settings.overlays, PresentationOverlays::default());
}

fn compile(source: &str) -> Result<CompiledFlow, FlowError> {
    compile_yaml_with_env(
        source,
        "flows/example.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}

fn error(source: &str) -> String {
    compile(source).unwrap_err().to_string()
}

#[test]
fn inline_flows_can_consume_prior_outputs_but_not_workspace_files() {
    let available = BTreeSet::from(["prior".to_owned()]);
    let flow = compile_inline_yaml(
            "version: 1\nname: next\nsettings: { video: off }\nsteps:\n  - evaluate: { script: 'return args[0]', args: ['${prior}'], save_as: next }\n",
            "submission.yaml",
            &BTreeMap::new(),
            &available,
        )
        .expect("compile with persistent output");
    assert_eq!(flow.steps.len(), 1);

    for source in [
        "version: 1\nname: x\nsteps: [{ run: child.subflow.yaml }]\n",
        "version: 1\nname: x\nsettings: { video: off }\nsteps: [{ assert: { screenshot: { baseline: home.png } } }]\n",
    ] {
        assert!(
            compile_inline_yaml(
                source,
                "submission.yaml",
                &BTreeMap::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
    }
}

#[test]
fn compiles_all_canonical_operations_and_locators() {
    let source = r#"
version: 1
name: canonical
base_url: https://example.test/app/
settings:
  timeout: 2s
  viewport: { width: 800, height: 600 }
  video: retain-on-failure
steps:
  - id: open-home
    open: ../home
  - click:
      target: { css: "button.primary" }
  - fill:
      target: { test_id: email }
      value: user@example.test
  - press:
      target: { label: Search }
      key: Enter
      modifiers: [Control, Shift]
  - screenshot:
      name: search-results
      crop: { x: 10, y: 20, width: 400, height: 300 }
  - clear: cookies
  - clear: storage
  - click:
      target:
        text: { value: Welcome, match: contains }
  - click:
      target:
        role: { value: button, name: Sign in }
  - assert:
      visible: { text: Welcome }
  - assert:
      hidden: { css: .spinner }
  - assert:
      text:
        target: { test_id: status }
        equals: Saved
  - assert:
      text:
        target: { label: Status }
        contains: complete
  - assert:
      url: { equals: "https://example.test/dashboard" }
  - assert:
      url: { path: "/dashboard?q=a b" }
"#;

    let flow = compile(source).unwrap();
    assert_eq!(flow.settings.timeout, Duration::from_secs(2));
    assert_eq!(flow.settings.video, VideoMode::RetainOnFailure);
    assert_eq!(flow.steps.len(), 15);
    assert!(matches!(
        &flow.steps[0].operation,
        Operation::Open { url, .. } if url.expose().as_str() == "https://example.test/home"
    ));
    assert!(matches!(
        &flow.steps[3].operation,
        Operation::Press { key: Key::Named(NamedKey::Enter), modifiers, .. }
            if modifiers == &[Modifier::Control, Modifier::Shift]
    ));
    assert!(matches!(
        &flow.steps[4].operation,
        Operation::Screenshot {
            name,
            crop: Some(Crop { x: 10, y: 20, width: 400, height: 300 })
        } if name == "search-results"
    ));
    assert!(matches!(
        &flow.steps[7].operation,
        Operation::Click {
            target: Locator {
                strategy: LocatorStrategy::Text {
                    match_kind: TextMatch::Contains,
                    ..
                },
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        &flow.steps[14].operation,
        Operation::Assert(Assertion::Url(UrlExpectation::Path(path)))
            if path.expose() == "/dashboard?q=a%20b"
    ));
}

#[test]
fn video_defaults_on_and_can_be_disabled() {
    let enabled = compile("version: 1\nname: x\nsteps: [{ open: https://x.test }]\n").unwrap();
    assert_eq!(enabled.settings.video, VideoMode::On);

    let disabled = compile(
        "version: 1\nname: x\nsettings: { video: off }\nsteps: [{ open: https://x.test }]\n",
    )
    .unwrap();
    assert_eq!(disabled.settings.video, VideoMode::Off);
}

#[test]
fn compiles_and_validates_geolocation_settings() {
    let default_accuracy = compile(
            "version: 1\nname: x\nsettings: { geolocation: { latitude: 51.5, longitude: -0.12 } }\nsteps: [{ open: https://x.test }]\n",
        )
        .unwrap();
    assert_eq!(
        default_accuracy.settings.geolocation,
        Some(Geolocation {
            latitude: 51.5,
            longitude: -0.12,
            accuracy: 0.0,
        })
    );
    assert!(
        compile("version: 1\nname: x\nsteps: [{ open: https://x.test }]\n")
            .unwrap()
            .settings
            .geolocation
            .is_none()
    );

    for (geolocation, expected) in [
        ("{ latitude: .nan, longitude: 0 }", "latitude"),
        ("{ latitude: 91, longitude: 0 }", "latitude"),
        ("{ latitude: 0, longitude: -.inf }", "longitude"),
        ("{ latitude: 0, longitude: 181 }", "longitude"),
        ("{ latitude: 0, longitude: 0, accuracy: .inf }", "accuracy"),
        ("{ latitude: 0, longitude: 0, accuracy: -1 }", "accuracy"),
    ] {
        let source = format!(
            "version: 1\nname: x\nsettings: {{ geolocation: {geolocation} }}\nsteps: [{{ open: https://x.test }}]\n"
        );
        assert!(error(&source).contains(expected), "accepted {geolocation}");
    }
}

#[test]
fn validates_screenshot_names_crops_duplicates_and_secrets() {
    let valid = compile(
            "version: 1\nname: x\nsettings: { viewport: { width: 800, height: 600 } }\nsteps:\n  - screenshot: { name: full }\n  - screenshot: { name: corner_2, crop: { x: 700, y: 500, width: 100, height: 100 } }\n",
        )
        .unwrap();
    assert!(matches!(
        &valid.steps[0].operation,
        Operation::Screenshot { name, crop: None } if name == "full"
    ));

    for (source, expected) in [
        (
            "version: 1\nname: x\nsteps: [{ screenshot: { name: '../escape' } }]\n",
            "screenshot.name must be",
        ),
        (
            "version: 1\nname: x\nsteps: [{ screenshot: { name: x, crop: { x: 0, y: 0, width: 0, height: 1 } } }]\n",
            "greater than zero",
        ),
        (
            "version: 1\nname: x\nsettings: { viewport: { width: 10, height: 10 } }\nsteps: [{ screenshot: { name: x, crop: { x: 9, y: 0, width: 2, height: 1 } } }]\n",
            "fit within the 10x10 viewport",
        ),
        (
            "version: 1\nname: x\nsteps: [{ screenshot: { name: same } }, { screenshot: { name: same } }]\n",
            "duplicate screenshot name",
        ),
        (
            "version: 1\nname: x\nsteps: [{ screenshot: { name: Same } }, { screenshot: { name: same } }]\n",
            "duplicate screenshot name",
        ),
        (
            "version: 1\nname: x\nsteps: [{ screenshot: { name: Failure } }]\n",
            "cannot be 'failure'",
        ),
        (
            "version: 1\nname: x\nsteps: [{ screenshot: { name: NUL } }]\n",
            "screenshot.name must be",
        ),
    ] {
        assert!(error(source).contains(expected), "missing {expected:?}");
    }

    let source = "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ screenshot: { name: '${token}' } }]\n";
    let environment = BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]);
    let message = compile_yaml_with_env(source, "x.yaml", &BTreeMap::new(), &environment)
        .unwrap_err()
        .to_string();
    assert!(message.contains("screenshot.name cannot contain a secret"));
    assert!(!message.contains("canary-secret"));
}

#[test]
fn compiles_bounded_visual_assertions_relative_to_the_containing_flow() {
    let valid = compile(
            "version: 1\nname: x\nsettings: { viewport: { width: 800, height: 600 }, video: off }\nsteps:\n  - assert:\n      screenshot:\n        baseline: fixtures/home.png\n        crop: { x: 10, y: 20, width: 400, height: 300 }\n        channel_tolerance: 4\n        max_changed_ratio: 0.01\n",
        )
        .unwrap();
    assert!(matches!(
        &valid.steps[0].operation,
        Operation::Assert(Assertion::Screenshot(VisualExpectation {
            baseline,
            crop: Some(Crop { x: 10, y: 20, width: 400, height: 300 }),
            channel_tolerance: 4,
            max_changed_ratio,
        })) if baseline == Path::new("flows/fixtures/home.png") && *max_changed_ratio == 0.01
    ));

    for (baseline, ratio, expected) in [
        ("../home.png", "0", "relative .png path"),
        ("/home.png", "0", "relative .png path"),
        ("home.jpg", "0", "relative .png path"),
        ("home.png", "1.01", "between 0 and 1"),
        ("home.png", "-.inf", "between 0 and 1"),
    ] {
        let source = format!(
            "version: 1\nname: x\nsettings: {{ video: off }}\nsteps: [{{ assert: {{ screenshot: {{ baseline: '{baseline}', max_changed_ratio: {ratio} }} }} }}]\n"
        );
        assert!(
            error(&source).contains(expected),
            "accepted {baseline} {ratio}"
        );
    }
    assert!(error(
            "version: 1\nname: x\nsettings: { viewport: { width: 8192, height: 8192 }, video: off }\nsteps: [{ assert: { screenshot: { baseline: home.png } } }]\n"
        )
        .contains("visual image dimensions"));
}

#[test]
fn compiles_double_click_as_one_target_action() {
    let flow =
        compile("version: 1\nname: x\nsteps:\n  - double_click: { target: { test_id: item } }\n")
            .unwrap();
    assert!(matches!(
        &flow.steps[0].operation,
        Operation::DoubleClick {
            target: Locator {
                strategy: LocatorStrategy::TestId(value),
                ..
            },
            ..
        } if value.expose() == "item"
    ));
    assert!(error(
            "version: 1\nname: x\nsteps:\n  - click: { target: { css: x } }\n    double_click: { target: { css: x } }\n"
        )
        .contains("exactly one operation"));
}

#[test]
fn compiles_erase_select_scroll_and_back_as_strict_single_operations() {
    let flow = compile(
            "version: 1\nname: interactions\nsteps:\n  - erase: { target: { label: Search } }\n  - select: { target: { css: select }, value: '' }\n  - scroll: { y: 500 }\n  - back: {}\n",
        )
        .unwrap();
    assert!(matches!(flow.steps[0].operation, Operation::Erase { .. }));
    assert!(matches!(
        &flow.steps[1].operation,
        Operation::Select { value, .. } if value.expose().is_empty()
    ));
    assert!(matches!(
        flow.steps[2].operation,
        Operation::Scroll { x: 0, y: 500 }
    ));
    assert!(matches!(flow.steps[3].operation, Operation::Back));

    for (source, expected) in [
        (
            "version: 1\nname: x\nsteps: [{ scroll: {} }]\n",
            "non-zero x or y",
        ),
        (
            "version: 1\nname: x\nsteps: [{ back: { target: x } }]\n",
            "unknown field",
        ),
        (
            "version: 1\nname: x\nsteps: [{ erase: { target: { css: x } }, back: {} }]\n",
            "exactly one operation",
        ),
    ] {
        assert!(error(source).contains(expected), "missing {expected:?}");
    }
}

#[test]
fn compiles_advanced_interactions_and_waits_with_bounded_defaults() {
    let flow = compile(
            "version: 1\nname: advanced\nsteps:\n  - scroll_until_visible: { target: { text: Last }, y: 400 }\n  - swipe: { target: { css: .card }, x: -120 }\n  - long_press: { target: { test_id: menu }, duration: 750ms }\n  - timeout: 30s\n    wait_until_visible: { target: { css: .late } }\n  - wait_until_stable: { target: { css: .animated } }\n",
        )
        .unwrap();

    assert!(matches!(
        flow.steps[0].operation,
        Operation::ScrollUntilVisible { x: 0, y: 400, .. }
    ));
    assert!(matches!(
        flow.steps[1].operation,
        Operation::Swipe {
            x: -120,
            y: 0,
            duration: DEFAULT_SWIPE_DURATION,
            ..
        }
    ));
    assert!(matches!(
        flow.steps[2].operation,
        Operation::LongPress { duration, .. } if duration == Duration::from_millis(750)
    ));
    assert_eq!(flow.steps[3].timeout, Duration::from_secs(30));
    assert!(matches!(
        flow.steps[3].operation,
        Operation::WaitUntilVisible { .. }
    ));
    assert!(matches!(
        flow.steps[4].operation,
        Operation::WaitUntilStable { .. }
    ));
}

#[test]
fn rejects_unbounded_or_empty_advanced_gestures() {
    for (step, expected) in [
        (
            "scroll_until_visible: { target: { css: x } }",
            "non-zero x or y",
        ),
        (
            "swipe: { target: { css: x }, x: 10001 }",
            "between -10000 and 10000",
        ),
        (
            "long_press: { target: { css: x }, duration: 11s }",
            "must not exceed 10 seconds",
        ),
        (
            "swipe: { target: { css: x }, y: 1, duration: 0ms }",
            "outside the supported range",
        ),
    ] {
        let source = format!("version: 1\nname: x\nsteps:\n  - {step}\n");
        assert!(error(&source).contains(expected), "accepted {step}");
    }
}

#[test]
fn clear_accepts_each_explicit_state_target_as_a_scalar() {
    let flow = compile(
            "version: 1\nname: clear\nsteps:\n  - clear: cookies\n  - clear: storage\n  - clear: indexeddb\n  - clear: cache-storage\n  - clear: service-workers\n",
        )
        .unwrap();
    assert!(matches!(
        flow.steps[0].operation,
        Operation::Clear(ClearTarget::Cookies)
    ));
    assert!(matches!(
        flow.steps[1].operation,
        Operation::Clear(ClearTarget::Storage)
    ));
    assert!(matches!(
        flow.steps[2].operation,
        Operation::Clear(ClearTarget::Indexeddb)
    ));
    assert!(matches!(
        flow.steps[3].operation,
        Operation::Clear(ClearTarget::CacheStorage)
    ));
    assert!(matches!(
        flow.steps[4].operation,
        Operation::Clear(ClearTarget::ServiceWorkers)
    ));

    for value in ["cache", "indexed-db", "service_worker", "{ cookies: true }"] {
        assert!(
            parse_yaml(&format!(
                "version: 1\nname: clear\nsteps: [{{ clear: {value} }}]\n"
            ))
            .is_err(),
            "accepted clear value {value:?}"
        );
    }
}

#[test]
fn native_dialog_responses_compile_and_validate_prompt_text() {
    let flow = compile(
            "version: 1\nname: dialogs\nvars: { answer: yes }\nsteps:\n  - dialog: { action: accept }\n  - dialog: { action: accept, text: '${answer}' }\n  - dialog: { action: dismiss }\n",
        )
        .unwrap();
    assert!(matches!(
        flow.steps[0].operation,
        Operation::Dialog {
            action: NativeDialogResponse::Accept,
            text: None
        }
    ));
    assert!(matches!(
        flow.steps[2].operation,
        Operation::Dialog {
            action: NativeDialogResponse::Dismiss,
            text: None
        }
    ));
    assert!(
        error("version: 1\nname: dialog\nsteps: [{ dialog: { action: dismiss, text: no } }]\n")
            .contains("only valid with action accept")
    );
}

#[test]
fn recording_controls_require_one_ordered_pair() {
    let flow = compile(
            "version: 1\nname: recording\nsteps:\n  - recording: start\n  - open: https://x.test\n  - recording: stop\n",
        )
        .unwrap();
    assert!(matches!(
        flow.steps[0].operation,
        Operation::Recording(RecordingControl::Start)
    ));
    assert!(matches!(
        flow.steps[2].operation,
        Operation::Recording(RecordingControl::Stop)
    ));

    for (steps, expected) in [
        ("  - recording: start\n", "requires one later"),
        ("  - recording: stop\n", "must follow"),
        (
            "  - recording: start\n  - recording: start\n  - recording: stop\n",
            "only one",
        ),
        (
            "  - recording: start\n  - recording: stop\n  - recording: stop\n",
            "only one",
        ),
    ] {
        let source = format!("version: 1\nname: recording\nsteps:\n{steps}");
        assert!(error(&source).contains(expected), "accepted {steps:?}");
    }
}

#[test]
fn skipped_recording_controls_still_disable_automatic_recording() {
    let flow = compile(
            "version: 1\nname: recording\nvars: { mode: disabled }\nsteps:\n  - open: https://x.test\n  - when: { variable: { name: mode, equals: enabled } }\n    recording: start\n  - when: { variable: { name: mode, equals: enabled } }\n    recording: stop\n",
        )
        .unwrap();

    assert!(flow.manual_recording);
    assert!(
        flow.steps
            .iter()
            .all(|step| !matches!(step.operation, Operation::Recording(_)))
    );
}

#[test]
fn compiles_page_evaluation_http_and_later_runtime_values() {
    let flow = compile(
        r#"version: 1
name: runtime
steps:
  - evaluate:
      script: "return { token: args[0], count: 2 };"
      args: [seed]
      save_as: page_value
  - request:
      method: post
      url: https://example.test/setup
      headers: { x-token: "${page_value}" }
      body: "${page_value}"
      expected_status: 201
      save_as: response
  - fill: { target: { css: input }, value: "${response}" }
"#,
    )
    .unwrap();

    assert!(matches!(
        &flow.steps[0].operation,
        Operation::Evaluate { save_as: Some(name), args, .. }
            if name == "page_value" && args[0].expose() == "seed"
    ));
    assert!(matches!(
        &flow.steps[1].operation,
        Operation::Request { method, expected_status: 201, save_as: Some(name), .. }
            if method == "POST" && name == "response"
    ));
    let Operation::Fill { value, .. } = &flow.steps[2].operation else {
        panic!("expected fill");
    };
    assert!(value.is_secret());
}

#[test]
fn runtime_outputs_are_ordered_unique_and_bounded_at_compile_time() {
    for (source, expected) in [
        (
            "version: 1\nname: x\nsteps: [{ fill: { target: { css: x }, value: '${later}' } }, { evaluate: { script: 'return 1', save_as: later } }]\n",
            "before it is saved",
        ),
        (
            "version: 1\nname: x\nsteps: [{ evaluate: { script: 'return 1', save_as: same } }, { request: { method: GET, url: https://x.test, expected_status: 200, save_as: same } }]\n",
            "duplicate runtime output",
        ),
        (
            "version: 1\nname: x\nvars: { same: value }\nsteps: [{ evaluate: { script: 'return 1', save_as: same } }]\n",
            "conflicts with an input",
        ),
        (
            "version: 1\nname: x\nsteps: [{ request: { method: TRACE, url: https://x.test, expected_status: 200 } }]\n",
            "method is unsupported",
        ),
        (
            "version: 1\nname: x\nsteps: [{ request: { method: GET, url: https://x.test, expected_status: 999 } }]\n",
            "between 100 and 599",
        ),
    ] {
        assert!(error(source).contains(expected), "missing {expected:?}");
    }
}

#[test]
fn runtime_outputs_support_repeated_and_conditional_expansions() {
    let flow = compile(
            "version: 1\nname: x\nsteps:\n  - repeat: 2\n    evaluate: { script: 'return 1', save_as: repeated }\n  - when: { visible: { css: .optional } }\n    evaluate: { script: 'return 2', save_as: conditional }\n  - fill: { target: { css: input }, value: '${repeated}-${conditional}' }\n",
        )
        .unwrap();

    assert_eq!(flow.steps.len(), 4);
    assert!(matches!(
        &flow.steps[3].operation,
        Operation::Fill { value, .. }
            if value.output_names().cloned().collect::<Vec<_>>()
                == ["conditional", "repeated"]
    ));
}

#[test]
fn compiles_bounded_pause_duration_and_rejects_invalid_values() {
    let flow = compile("version: 1\nname: pause\nsteps: [{ pause: 1500ms }]\n").unwrap();
    assert!(matches!(
        flow.steps[0].operation,
        Operation::Pause { duration } if duration == Duration::from_millis(1500)
    ));
    for duration in ["0ms", "61s", "not-a-duration"] {
        let source = format!("version: 1\nname: pause\nsteps: [{{ pause: {duration} }}]\n");
        assert!(
            error(&source).contains("pause"),
            "accepted pause {duration}"
        );
    }
}

#[test]
fn rejects_unknown_duplicate_merge_and_alias_yaml() {
    let unknown = "version: 1\nname: x\nunknown: true\nsteps: [{ open: https://x.test }]\n";
    assert!(parse_yaml(unknown).is_err());

    let duplicate = "version: 1\nname: first\nname: second\nsteps: [{ open: https://x.test }]\n";
    assert!(parse_yaml(duplicate).is_err());

    let merge = r#"
defaults: &defaults
  name: merged
version: 1
<<: *defaults
steps: [{ open: https://x.test }]
"#;
    assert!(parse_yaml(merge).is_err());

    let alias = r#"
version: 1
name: alias
vars:
  first: &value hello
  second: *value
steps: [{ open: https://x.test }]
"#;
    assert!(parse_yaml(alias).is_err());
}

#[test]
fn rejects_oversized_yaml_sources_before_parsing_or_file_decoding() {
    let source = "x".repeat(MAX_FLOW_SOURCE_BYTES + 1);
    assert!(error(&source).contains("flow source exceeds the maximum size"));

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.yaml");
    fs::write(&path, source).unwrap();
    let message = compile_file(path, &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(message.contains("flow source exceeds the maximum size"));
}

#[test]
fn rejects_excess_steps_scalars_and_interpolation_growth() {
    let steps = "  - open: https://x.test\n".repeat(MAX_FLOW_STEPS + 1);
    let source = format!("version: 1\nname: x\nsteps:\n{steps}");
    assert!(error(&source).contains("steps must not exceed 10000"));

    let large = "x".repeat(MAX_SCALAR_BYTES + 1);
    let source = format!("version: 1\nname: {large}\nsteps: [{{ open: https://x.test }}]\n");
    assert!(error(&source).contains("maximum scalar size"));

    let source = "version: 1\nname: x\nvars: { chunk: { env: CHUNK } }\nsteps:\n  - fill: { target: { css: x }, value: '${chunk}${chunk}' }\n";
    let environment = BTreeMap::from([("CHUNK".to_owned(), "x".repeat(MAX_SCALAR_BYTES))]);
    let message = compile_yaml_with_env(source, "x.yaml", &BTreeMap::new(), &environment)
        .unwrap_err()
        .to_string();
    assert!(message.contains("maximum scalar size"));
}

#[test]
fn accepts_timeout_ceiling_and_rejects_larger_flow_and_step_timeouts() {
    let flow = compile(
            "version: 1\nname: x\nsettings: { timeout: 60m }\nsteps: [{ timeout: 3600s, open: https://x.test }]\n",
        )
        .unwrap();
    assert_eq!(flow.settings.timeout, MAX_TIMEOUT);
    assert_eq!(flow.steps[0].timeout, MAX_TIMEOUT);

    assert!(
        error(
            "version: 1\nname: x\nsettings: { timeout: 61m }\nsteps: [{ open: https://x.test }]\n"
        )
        .contains("must not exceed 3600 seconds")
    );
    assert!(
        error("version: 1\nname: x\nsteps: [{ timeout: 3601s, open: https://x.test }]\n")
            .contains("must not exceed 3600 seconds")
    );
}

#[test]
fn enforces_single_operations_assertions_and_locator_strategies() {
    assert!(error(
            "version: 1\nname: x\nsteps:\n  - open: https://x.test\n    click: { target: { css: x } }\n"
        )
        .contains("exactly one operation"));
    assert!(error(
            "version: 1\nname: x\nsteps:\n  - assert:\n      visible: { css: x }\n      hidden: { css: y }\n"
        )
        .contains("exactly one assertion"));
    assert!(
        error("version: 1\nname: x\nsteps:\n  - click:\n      target: { css: x, text: y }\n")
            .contains("exactly one strategy")
    );
}

#[test]
fn compiles_flat_locator_modifiers_without_counting_them_as_strategies() {
    let flow = compile(
            "version: 1\nname: x\nsteps:\n  - click:\n      position: { x: 4, y: 7 }\n      target: { css: option, index: 0, checked: false, selected: true, focused: false, enabled: true }\n",
        )
        .unwrap();
    let Operation::Click { target, position } = &flow.steps[0].operation else {
        panic!("expected click");
    };
    assert!(matches!(target.strategy, LocatorStrategy::Css(_)));
    assert_eq!(target.index, Some(0));
    assert_eq!(target.checked, Some(false));
    assert_eq!(target.selected, Some(true));
    assert_eq!(target.focused, Some(false));
    assert_eq!(target.enabled, Some(true));
    assert_eq!(*position, Some(RelativePoint { x: 4, y: 7 }));

    assert!(
        error("version: 1\nname: x\nsteps: [{ click: { target: { index: 0 } } }]\n")
            .contains("exactly one strategy")
    );
    assert!(
        parse_yaml("version: 1\nname: x\nsteps: [{ click: { target: { css: x, index: -1 } } }]\n")
            .is_err()
    );
    assert!(
        parse_yaml(
            "version: 1\nname: x\nsteps: [{ click: { target: { css: x, checked: yes } } }]\n"
        )
        .is_err()
    );
    assert!(parse_yaml(
            "version: 1\nname: x\nsteps: [{ erase: { target: { css: x }, position: { x: 1, y: 1 } } }]\n"
        )
        .is_err());
}

#[test]
fn compiles_only_in_bounds_targetless_click_points() {
    let flow = compile(
            "version: 1\nname: x\nsettings: { video: off, viewport: { width: 800, height: 600 } }\nsteps: [{ click: { point: { x: 799, y: 599 } } }]\n",
        )
        .unwrap();
    assert!(matches!(
        flow.steps[0].operation,
        Operation::ClickPoint {
            point: ViewportPoint { x: 799, y: 599 }
        }
    ));

    for (click, message) in [
        ("{ point: { x: 800, y: 10 } }", "outside viewport 800x600"),
        ("{ point: { x: 10, y: 600 } }", "outside viewport 800x600"),
        ("{}", "requires exactly one of target or point"),
        (
            "{ target: { css: button }, point: { x: 10, y: 10 } }",
            "cannot be combined with target or position",
        ),
        (
            "{ point: { x: 10, y: 10 }, position: { x: 1, y: 1 } }",
            "cannot be combined with target or position",
        ),
    ] {
        let source = format!(
            "version: 1\nname: x\nsettings: {{ video: off, viewport: {{ width: 800, height: 600 }} }}\nsteps: [{{ click: {click} }}]\n"
        );
        assert!(error(&source).contains(message), "{}", error(&source));
    }
    assert!(
        error("version: 1\nname: x\nsteps: [{ double_click: { point: { x: 1, y: 1 } } }]\n")
            .contains("double_click does not support point")
    );
}

#[test]
fn compiles_recursive_relations_and_bounds_their_depth() {
    let flow = compile(
            "version: 1\nname: x\nsteps:\n  - click:\n      target:\n        css: button\n        within: { css: .panel }\n        child_of: { css: .toolbar }\n        has: { text: Save }\n        above: { test_id: footer }\n        below: { css: header }\n        left: { label: Cancel }\n        right: { role: { value: img, name: Logo } }\n",
        )
        .unwrap();
    let Operation::Click { target, .. } = &flow.steps[0].operation else {
        panic!("expected click");
    };
    assert_eq!(
        target
            .relations
            .iter()
            .map(|relation| relation.kind)
            .collect::<Vec<_>>(),
        [
            RelationKind::Within,
            RelationKind::ChildOf,
            RelationKind::Has,
            RelationKind::Above,
            RelationKind::Below,
            RelationKind::Left,
            RelationKind::Right,
        ]
    );

    let mut locator = "{ css: leaf }".to_owned();
    for _ in 0..=MAX_LOCATOR_DEPTH {
        locator = format!("{{ css: node, has: {locator} }}");
    }
    let source = format!("version: 1\nname: x\nsteps: [{{ click: {{ target: {locator} }} }}]\n");
    assert!(error(&source).contains("maximum relation depth 8"));
}

#[test]
fn validates_required_values_version_ids_duration_viewport_and_keys() {
    let invalid_cases = [
        (
            "version: 2\nname: x\nsteps: [{ open: https://x.test }]\n",
            "version must be 1",
        ),
        (
            "version: 1\nname: '  '\nsteps: [{ open: https://x.test }]\n",
            "name must not be empty",
        ),
        (
            "version: 1\nname: x\nsteps: []\n",
            "steps must not be empty",
        ),
        (
            "version: 1\nname: x\nsettings: { timeout: 0s }\nsteps: [{ open: https://x.test }]\n",
            "outside the supported range",
        ),
        (
            "version: 1\nname: x\nsettings: { timeout: 1.5s }\nsteps: [{ open: https://x.test }]\n",
            "not a valid duration",
        ),
        (
            "version: 1\nname: x\nsettings: { video: on, viewport: { width: 801, height: 600 } }\nsteps: [{ open: https://x.test }]\n",
            "even viewport",
        ),
        (
            "version: 1\nname: x\nsteps:\n  - id: same\n    open: https://x.test\n  - id: same\n    open: https://x.test\n",
            "duplicate step id",
        ),
        (
            "version: 1\nname: x\nsteps:\n  - press: { target: { css: x }, key: F1 }\n",
            "unsupported key",
        ),
        (
            "version: 1\nname: x\nsteps:\n  - press: { target: { css: x }, key: Enter, modifiers: [Alt, Alt] }\n",
            "duplicate modifier",
        ),
        (
            "version: 1\nname: x\nsteps:\n  - fill: { target: { css: x }, value: '' }\n",
            "fill.value must not be empty",
        ),
    ];
    for (source, expected) in invalid_cases {
        assert!(error(source).contains(expected), "missing {expected:?}");
    }
}

#[test]
fn resolves_cli_env_defaults_and_secret_taint_without_debug_leaks() {
    let source = r#"
version: 1
name: "login-${region}"
base_url: "https://${host}"
vars:
  region: local
  host: { env: TEST_HOST, default: default.test }
  username: { env: TEST_USER }
secrets:
  password: { env: TEST_PASSWORD }
steps:
  - fill:
      target: { label: Password }
      value: "prefix-${password}"
  - fill:
      target: { label: User }
      value: "${username}"
"#;
    let cli = BTreeMap::from([("region".to_owned(), "ci".to_owned())]);
    let env = BTreeMap::from([
        ("TEST_HOST".to_owned(), "example.test".to_owned()),
        ("TEST_USER".to_owned(), "alice".to_owned()),
        ("TEST_PASSWORD".to_owned(), "canary-secret".to_owned()),
    ]);
    let flow = compile_yaml_with_env(source, "login.yaml", &cli, &env).unwrap();

    assert_eq!(flow.name, "login-ci");
    assert_eq!(
        flow.base_url.as_ref().unwrap().expose().host_str(),
        Some("example.test")
    );
    let Operation::Fill { value, .. } = &flow.steps[0].operation else {
        panic!("expected fill");
    };
    assert!(value.is_secret());
    assert_eq!(value.expose(), "prefix-canary-secret");
    assert_eq!(format!("{value:?}"), REDACTED);
    assert!(!format!("{flow:?}").contains("canary-secret"));
    assert_eq!(
        flow.redactor.redact("failed with canary-secret visible"),
        "failed with [REDACTED] visible"
    );
}

#[test]
fn rejects_unresolved_unknown_empty_and_secret_identity_inputs() {
    assert!(
        error(
            "version: 1\nname: x\nsteps: [{ fill: { target: { css: x }, value: '${missing}' } }]\n"
        )
        .contains("unknown variable")
    );
    assert!(
        error("version: 1\nname: x\nvars: { empty: '' }\nsteps: [{ open: https://x.test }]\n")
            .contains("must not be empty")
    );

    let source = "version: 1\nname: '${token}'\nsecrets: { token: { env: TOKEN } }\nsteps: [{ open: https://x.test }]\n";
    let env = BTreeMap::from([("TOKEN".to_owned(), "canary-value".to_owned())]);
    let message = compile_yaml_with_env(source, "x.yaml", &BTreeMap::new(), &env)
        .unwrap_err()
        .to_string();
    assert!(message.contains("name cannot contain a secret"));
    assert!(!message.contains("canary-value"));
}

#[test]
fn validates_open_and_assertion_urls() {
    assert!(
        error("version: 1\nname: x\nsteps: [{ open: /relative }]\n")
            .contains("base_url is not set")
    );
    assert!(
        error("version: 1\nname: x\nbase_url: file:///tmp\nsteps: [{ open: /x }]\n")
            .contains("http or https")
    );
    assert!(
        error("version: 1\nname: x\nsteps: [{ assert: { url: { equals: /relative } } }]\n")
            .contains("absolute URL")
    );
    assert!(
        error("version: 1\nname: x\nsteps: [{ assert: { url: { path: '//host/path' } } }]\n")
            .contains("one slash")
    );
    assert!(
        error("version: 1\nname: x\nsteps: [{ assert: { url: { path: '/path#fragment' } } }]\n")
            .contains("fragment")
    );
}

#[test]
fn cli_values_only_override_declared_non_secret_vars() {
    let source = "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ open: https://x.test }]\n";
    let cli = BTreeMap::from([("token".to_owned(), "not-allowed".to_owned())]);
    let env = BTreeMap::from([("TOKEN".to_owned(), "secret".to_owned())]);
    let message = compile_yaml_with_env(source, "x.yaml", &cli, &env)
        .unwrap_err()
        .to_string();
    assert!(message.contains("not declared under vars"));
    assert!(!message.contains("not-allowed"));
}

#[test]
fn discovers_yaml_recursively_in_stable_order_and_builds_stable_artifact_keys() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(directory.path().join("z.yml"), "").unwrap();
    fs::write(directory.path().join("a.yaml"), "").unwrap();
    fs::write(directory.path().join("ignored.txt"), "").unwrap();
    fs::write(nested.join("b.yaml"), "").unwrap();
    fs::write(nested.join("shared.subflow.yaml"), "").unwrap();
    fs::write(nested.join("shared.subflow.yml"), "").unwrap();

    let files = discover_flow_files(directory.path()).unwrap();
    let relative = files
        .iter()
        .map(|file| normalized_path(file.strip_prefix(directory.path()).expect("under root")))
        .collect::<Vec<_>>();
    assert_eq!(relative, ["a.yaml", "nested/b.yaml", "z.yml"]);

    let first = artifact_key(directory.path(), nested.join("b.yaml"));
    let second = artifact_key(directory.path(), nested.join("b.yaml"));
    assert_eq!(first, second);
    assert!(first.starts_with("nested-b-"));
    assert_ne!(
        artifact_key(directory.path(), directory.path().join("b.yaml")),
        first
    );
}

#[test]
fn rejects_non_yaml_file_and_empty_directory_discovery() {
    let directory = tempfile::tempdir().unwrap();
    assert!(discover_flow_files(directory.path()).is_err());
    let text = directory.path().join("flow.txt");
    fs::write(&text, "").unwrap();
    assert!(discover_flow_files(text).is_err());
}

#[test]
fn expands_nested_subflows_in_place_with_file_scoped_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let root = directory.path().join("root.yaml");
    let child = nested.join("child.subflow.yaml");
    let grandchild = directory.path().join("grandchild.subflow.yml");
    fs::write(
            &root,
            "version: 1\nname: root-${root_name}\nbase_url: https://root.test\nsettings: { timeout: 9s, video: off }\nvars: { root_name: root }\nsteps:\n  - open: /before\n  - run: ./nested/child.subflow.yaml\n  - assert: { url: { path: /after } }\n",
        )
        .unwrap();
    fs::write(
            &child,
            "version: 1\nname: child\nbase_url: https://child.test/base/\nsettings: { timeout: 2s }\nvars: { child_value: default }\nsteps:\n  - open: page\n  - run: ../grandchild.subflow.yml\n",
        )
        .unwrap();
    fs::write(
            &grandchild,
            "version: 1\nname: grandchild\nvars: { leaf: default }\nsecrets: { token: { env: TOKEN } }\nsteps:\n  - fill: { target: { css: input }, value: '${leaf}-${token}' }\n",
        )
        .unwrap();
    let cli = BTreeMap::from([
        ("root_name".to_owned(), "entry".to_owned()),
        ("child_value".to_owned(), "unused".to_owned()),
        ("leaf".to_owned(), "value".to_owned()),
    ]);
    let environment = BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]);

    let flow = compile_file_with_env(&root, &cli, &environment).unwrap();

    assert_eq!(flow.name, "root-entry");
    assert_eq!(flow.settings.timeout, Duration::from_secs(9));
    assert_eq!(flow.settings.video, VideoMode::Off);
    assert_eq!(flow.steps.len(), 4);
    assert_eq!(
        flow.steps.iter().map(|step| step.index).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(flow.steps[1].source, child);
    assert_eq!(flow.steps[1].source_index, 1);
    assert_eq!(
        fs::canonicalize(&flow.steps[2].source).unwrap(),
        fs::canonicalize(&grandchild).unwrap()
    );
    assert_eq!(flow.steps[2].source_index, 1);
    assert_eq!(flow.steps[1].timeout, Duration::from_secs(2));
    assert_eq!(flow.steps[2].timeout, DEFAULT_TIMEOUT);
    assert!(matches!(
        &flow.steps[1].operation,
        Operation::Open { url, .. } if url.expose().as_str() == "https://child.test/base/page"
    ));
    assert!(matches!(
        &flow.steps[2].operation,
        Operation::Fill { value, .. }
            if value.expose() == "value-canary-secret" && value.is_secret()
    ));
    assert_eq!(flow.redactor.redact("canary-secret"), REDACTED);
    assert_eq!(flow.inputs["root_name"].expose(), "entry");
    assert!(!flow.inputs.contains_key("leaf"));
}

#[test]
fn nested_subflows_validate_bounds_against_the_entrypoint_viewport() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    let grandchild = directory.path().join("grandchild.subflow.yaml");
    fs::write(
        &child,
        "version: 1\nname: child\nsteps: [{ run: ./grandchild.subflow.yaml }]\n",
    )
    .unwrap();
    fs::write(
        &grandchild,
        "version: 1\nname: grandchild\nsteps: [{ click: { point: { x: 1400, y: 800 } } }]\n",
    )
    .unwrap();

    fs::write(
        &root,
        "version: 1\nname: root\nsettings: { viewport: { width: 1601, height: 901 }, video: off }\nsteps: [{ run: ./child.subflow.yaml }]\n",
    )
    .unwrap();
    assert!(compile_file(&root, &BTreeMap::new()).is_ok());

    fs::write(
        &root,
        "version: 1\nname: root\nsettings: { viewport: { width: 800, height: 600 }, video: off }\nsteps: [{ run: ./child.subflow.yaml }]\n",
    )
    .unwrap();
    let message = compile_file(&root, &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(message.contains("outside viewport 800x600"), "{message}");
}

#[test]
fn subflows_are_reusable_but_canonical_active_stack_cycles_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("shared.subflow.yaml");
    fs::write(
            &root,
            "version: 1\nname: root\nsettings: { video: off }\nsteps:\n  - run: ./shared.subflow.yaml\n  - run: ./shared.subflow.yaml\n",
        )
        .unwrap();
    fs::write(
        &child,
        "version: 1\nname: shared\nsteps: [{ open: https://example.test }]\n",
    )
    .unwrap();
    assert_eq!(
        compile_file(&root, &BTreeMap::new()).unwrap().steps.len(),
        2
    );

    fs::create_dir(directory.path().join("nested")).unwrap();
    fs::write(
        &child,
        "version: 1\nname: cycle\nsteps: [{ run: './nested/../shared.subflow.yaml' }]\n",
    )
    .unwrap();
    let message = compile_file(&root, &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(message.contains("subflow include cycle"), "{message}");
    assert!(message.contains("shared.subflow.yaml"), "{message}");
}

#[test]
fn rejects_unsafe_or_ambiguous_subflow_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    let compile_case = |root_source: &str, child_source: &str| {
        fs::write(&root, root_source).unwrap();
        fs::write(&child, child_source).unwrap();
        compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string()
    };
    let root_source = "version: 1\nname: root\nsteps: [{ run: ./child.subflow.yaml }]\n";
    for child_source in [
        "version: 1\nname: child\nsteps: []\n",
        "version: 1\nname: child\nsettings: { viewport: { width: 800, height: 600 } }\nsteps: [{ open: https://example.test }]\n",
        "version: 1\nname: child\nsettings: { video: off }\nsteps: [{ open: https://example.test }]\n",
    ] {
        let message = compile_case(root_source, child_source);
        assert!(
            message.contains("steps must not be empty") || message.contains("subflows cannot set"),
            "invalid child was accepted: {message}"
        );
    }

    fs::write(
        &child,
        "version: 1\nname: child\nsteps: [{ open: https://example.test }]\n",
    )
    .unwrap();
    for (step, expected) in [
        ("{ id: x, run: ./child.subflow.yaml }", "only field"),
        ("{ timeout: 1s, run: ./child.subflow.yaml }", "only field"),
        (
            "{ run: ./child.subflow.yaml, open: https://x.test }",
            "only field",
        ),
        (
            "{ run: ./child.subflow.yaml, dialog: { action: accept } }",
            "only field",
        ),
        ("{ run: ./child.yaml }", ".subflow.yaml"),
    ] {
        fs::write(&root, format!("version: 1\nname: root\nsteps: [{step}]\n")).unwrap();
        let message = compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(message.contains(expected), "{message}");
    }
    let absolute_child = std::env::temp_dir().join("child.subflow.yaml");
    fs::write(
        &root,
        format!(
            "version: 1\nname: root\nsteps: [{{ run: '{}' }}]\n",
            absolute_child.display()
        ),
    )
    .unwrap();
    assert!(
        compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("must be relative")
    );
    assert!(
        compile("version: 1\nname: memory\nsteps: [{ run: ./child.subflow.yaml }]\n")
            .unwrap_err()
            .to_string()
            .contains("require compiling a flow file")
    );
}

#[test]
fn validates_expanded_uniqueness_and_reports_child_compile_locations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    fs::write(
            &root,
            "version: 1\nname: root\nsteps:\n  - id: same\n    screenshot: { name: same }\n  - run: ./child.subflow.yaml\n",
        )
        .unwrap();
    fs::write(
        &child,
        "version: 1\nname: child\nsteps:\n  - id: same\n    open: https://example.test\n",
    )
    .unwrap();
    assert!(
        compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("duplicate step id")
    );

    fs::write(
            &child,
            "version: 1\nname: child\nsteps:\n  - open: https://example.test\n  - click: { target: { css: x, text: y } }\n",
        )
        .unwrap();
    let message = compile_file(&root, &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(message.contains("child.subflow.yaml"), "{message}");
    assert!(message.contains("step 2 locator"), "{message}");

    fs::write(
        &child,
        "version: 1\nname: child\nsteps: [{ screenshot: { name: Same } }]\n",
    )
    .unwrap();
    assert!(
        compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("duplicate screenshot name")
    );
}

#[test]
fn enforces_expanded_step_and_subflow_depth_limits() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("many.subflow.yaml");
    let root_steps = "  - open: https://root.test\n".repeat(MAX_FLOW_STEPS / 2);
    let child_steps = "  - open: https://child.test\n".repeat(MAX_FLOW_STEPS / 2 + 1);
    fs::write(
            &root,
            format!(
                "version: 1\nname: root\nsettings: {{ video: off }}\nsteps:\n{root_steps}  - run: ./many.subflow.yaml\n"
            ),
        )
        .unwrap();
    fs::write(
        &child,
        format!("version: 1\nname: child\nsteps:\n{child_steps}"),
    )
    .unwrap();
    assert!(
        compile_file(&root, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("expanded steps must not exceed")
    );

    for depth in 0..=MAX_SUBFLOW_DEPTH {
        let path = if depth == 0 {
            root.clone()
        } else {
            directory.path().join(format!("{depth}.subflow.yaml"))
        };
        let next = depth + 1;
        fs::write(
            path,
            format!("version: 1\nname: depth-{depth}\nsteps: [{{ run: ./{next}.subflow.yaml }}]\n"),
        )
        .unwrap();
    }
    fs::write(
        directory
            .path()
            .join(format!("{}.subflow.yaml", MAX_SUBFLOW_DEPTH + 1)),
        "version: 1\nname: leaf\nsteps: [{ open: https://example.test }]\n",
    )
    .unwrap();
    let message = compile_file(&root, &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(message.contains("maximum subflow depth 32"), "{message}");
}

#[test]
fn compiles_when_repeat_and_assertion_retries_without_runtime_control_flow() {
    let flow = compile(
        r#"version: 1
name: control
vars: { mode: enabled }
steps:
  - when: { variable: { name: mode, equals: enabled } }
    repeat: 2
    click: { target: { css: button } }
  - when: { variable: { name: mode, equals: disabled } }
    click: { target: { css: skipped } }
  - when: { visible: { css: .ready } }
    assert: { hidden: { css: .loading } }
  - retry: 3
    assert: { visible: { css: .eventual } }
"#,
    )
    .unwrap();

    assert_eq!(flow.steps.len(), 4);
    assert!(matches!(flow.steps[0].operation, Operation::Click { .. }));
    assert!(matches!(flow.steps[1].operation, Operation::Click { .. }));
    assert!(matches!(flow.steps[2].when, Some(When::Visible(_))));
    assert_eq!(flow.steps[3].retries, 3);
}

#[test]
fn compiles_web_expressions_and_bounded_while_guards() {
    let flow = compile(
        r#"version: 1
name: control
vars: { mode: enabled, flag: "true" }
steps:
  - when: { platform: web }
    open: https://example.test
  - when:
      expression:
        all:
          - equals: { left: "${mode}", right: enabled }
          - not: { not_equals: { left: same, right: same } }
          - boolean: "${flag}"
    click: { target: { css: button } }
  - while:
      expression: { any: [{ boolean: "false" }, { equals: { left: x, right: x } }] }
      max_iterations: 3
    click: { target: { css: .next } }
"#,
    )
    .unwrap();

    assert!(flow.steps[0].when.is_none());
    assert!(matches!(flow.steps[1].when, Some(When::Expression(_))));
    assert_eq!(flow.steps.len(), 5);
    let guards = flow.steps[2..]
        .iter()
        .map(|step| &step.guards[0])
        .collect::<Vec<_>>();
    assert!(guards.iter().all(|guard| guard.first));
    assert_eq!(
        guards.iter().map(|guard| guard.id).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        guards
            .iter()
            .all(|guard| matches!(guard.kind, GuardKind::While { loop_id: 1, .. }))
    );
}

#[test]
fn expressions_and_while_are_strictly_bounded() {
    for (control, expected) in [
        (
            "while: { expression: { boolean: 'true' }, max_iterations: 0 }",
            "while.max_iterations must be between 1 and 100",
        ),
        (
            "while: { expression: { boolean: 'true' }, max_iterations: 101 }",
            "while.max_iterations must be between 1 and 100",
        ),
        (
            "when: { expression: { all: [] } }",
            "expression list must not be empty",
        ),
        (
            "when: { expression: { boolean: 'true', equals: { left: x, right: x } } }",
            "expression must contain exactly one operator",
        ),
    ] {
        let source =
            format!("version: 1\nname: x\nsteps:\n  - {control}\n    open: https://x.test\n");
        assert!(error(&source).contains(expected), "accepted {control}");
    }

    let mut expression = "{ boolean: 'true' }".to_owned();
    for _ in 0..=MAX_EXPRESSION_DEPTH {
        expression = format!("{{ not: {expression} }}");
    }
    let source = format!(
        "version: 1\nname: x\nsteps:\n  - when: {{ expression: {expression} }}\n    open: https://x.test\n"
    );
    assert!(error(&source).contains("maximum depth 8"));
}

#[test]
fn rejects_unbounded_or_unsafe_step_controls() {
    for (source, expected) in [
        (
            "version: 1\nname: x\nsteps: [{ repeat: 0, open: https://x.test }]\n",
            "repeat must be between 1 and 100",
        ),
        (
            "version: 1\nname: x\nsteps: [{ repeat: 101, open: https://x.test }]\n",
            "repeat must be between 1 and 100",
        ),
        (
            "version: 1\nname: x\nsteps: [{ retry: 11, assert: { visible: { css: x } } }]\n",
            "retry must be between 1 and 10",
        ),
        (
            "version: 1\nname: x\nsteps: [{ retry: 1, click: { target: { css: x } } }]\n",
            "only supported for assertions",
        ),
        (
            "version: 1\nname: x\nsteps: [{ id: x, repeat: 2, open: https://x.test }]\n",
            "cannot combine id and repeat",
        ),
        (
            "version: 1\nname: x\nvars: { mode: off }\nsteps: [{ when: { variable: { name: mode, equals: on } }, click: { target: { css: x, text: y } } }]\n",
            "exactly one strategy",
        ),
    ] {
        assert!(error(source).contains(expected), "missing {expected:?}");
    }
}

#[test]
fn mapped_run_arguments_preserve_taint_and_repeat_while_scalar_run_still_works() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    fs::write(
            &root,
            "version: 1\nname: root\nsettings: { video: off }\nvars: { mode: enabled }\nsecrets: { token: { env: TOKEN } }\nsteps:\n  - run: ./child.subflow.yaml\n  - repeat: 2\n    run: { path: ./child.subflow.yaml, vars: { value: '${mode}-${token}' } }\n",
        )
        .unwrap();
    fs::write(
            &child,
            "version: 1\nname: child\nvars: { value: default }\nsteps: [{ fill: { target: { css: input }, value: '${value}' } }]\n",
        )
        .unwrap();
    let environment = BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]);

    let flow = compile_file_with_env(&root, &BTreeMap::new(), &environment).unwrap();

    assert_eq!(flow.steps.len(), 3);
    assert_eq!(
        flow.steps
            .iter()
            .map(|step| (step.index, step.source.as_path(), step.source_index))
            .collect::<Vec<_>>(),
        [
            (1, child.as_path(), 1),
            (2, child.as_path(), 1),
            (3, child.as_path(), 1)
        ]
    );
    let Operation::Fill { value, .. } = &flow.steps[1].operation else {
        panic!("expected child fill");
    };
    assert_eq!(value.expose(), "enabled-canary-secret");
    assert!(value.is_secret());
    assert!(!format!("{flow:?}").contains("canary-secret"));
    assert_eq!(flow.redactor.redact("canary-secret"), REDACTED);
}

#[test]
fn run_retry_expands_only_across_assertion_only_subflows() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    fs::write(
        &root,
        "version: 1\nname: root\nsteps: [{ run: ./child.subflow.yaml, retry: 2 }]\n",
    )
    .unwrap();
    fs::write(
        &child,
        "version: 1\nname: child\nsteps: [{ assert: { visible: { css: x } } }]\n",
    )
    .unwrap();
    let flow = compile_file(&root, &BTreeMap::new()).unwrap();
    assert_eq!(flow.steps[0].retries, 2);

    fs::write(
        &child,
        "version: 1\nname: child\nsteps: [{ click: { target: { css: x } } }]\n",
    )
    .unwrap();
    fs::write(
            &root,
            "version: 1\nname: root\nvars: { mode: off }\nsteps: [{ run: ./child.subflow.yaml, retry: 2, when: { variable: { name: mode, equals: on } } }]\n",
        )
        .unwrap();
    let message = compile_file(&root, &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(message.contains("assertion-only subflow"), "{message}");
}

#[test]
fn open_accepts_a_wait_until_visible_settle_condition() {
    let source = "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: { url: /article, wait_until: { visible: { css: '#firstHeading' } } }\n";
    let flow = compile(source).expect("compile");
    let Operation::Open { url, settle } = &flow.steps[0].operation else {
        panic!("expected open: {:?}", flow.steps[0].operation);
    };
    assert_eq!(url.expose().as_str(), "https://example.test/article");
    assert!(matches!(settle, Some(SettleCondition::Visible(_))));
}

#[test]
fn open_accepts_a_wait_until_stable_settle_condition() {
    let source = "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: { url: /x, wait_until: { stable: { test_id: hero } } }\n";
    let flow = compile(source).expect("compile");
    let Operation::Open { url: _, settle } = &flow.steps[0].operation else {
        panic!("expected open: {:?}", flow.steps[0].operation);
    };
    assert!(matches!(settle, Some(SettleCondition::Stable(_))));
}

#[test]
fn open_settle_is_optional_and_backward_compatible() {
    let source =
        "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: /home\n";
    let flow = compile(source).expect("compile");
    assert!(matches!(
        &flow.steps[0].operation,
        Operation::Open { settle: None, .. }
    ));
}

#[test]
fn open_accepts_a_structured_url_without_wait_until() {
    let source = "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: { url: /home }\n";
    let flow = compile(source).expect("compile");
    assert!(matches!(
        &flow.steps[0].operation,
        Operation::Open {
            settle: None,
            url
        } if url.expose().as_str() == "https://example.test/home"
    ));
}

#[test]
fn open_wait_until_rejects_both_visible_and_stable() {
    let source = "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: { url: /x, wait_until: { visible: { css: a }, stable: { css: b } } }\n";
    let message = error(source);
    assert!(
        message.contains("wait_until accepts exactly one of visible or stable"),
        "{message}"
    );
}

#[test]
fn open_wait_until_rejects_an_empty_condition() {
    let source = "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: { url: /x, wait_until: {} }\n";
    let message = error(source);
    assert!(
        message.contains("wait_until requires exactly one of visible or stable"),
        "{message}"
    );
}

#[test]
fn open_wait_until_requires_a_url() {
    let source = "version: 1\nname: settle\nbase_url: https://example.test/\nsteps:\n  - open: { wait_until: { visible: { css: a } } }\n";
    let message = error(source);
    assert!(message.contains("open requires url"), "{message}");
}

fn compile_with_env(source: &str, environment: &[(&str, &str)]) -> CompiledFlow {
    let environment: BTreeMap<String, String> = environment
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    compile_yaml_with_env(source, "flows/example.yaml", &BTreeMap::new(), &environment)
        .expect("compile")
}

#[test]
fn recording_secret_warning_is_absent_when_video_is_off_without_screenshots() {
    let flow = compile_with_env(
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: off }\nsteps:\n  - fill: { target: { css: email }, value: '${token}' }\n",
        &[("TOKEN", "supersecret")],
    );
    assert!(flow.recording_secret_warning().is_none());
}

#[test]
fn recording_secret_warning_is_present_for_secret_fill_with_screenshot_and_video_off() {
    let flow = compile_with_env(
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: off }\nsteps:\n  - fill: { target: { css: email }, value: '${token}' }\n  - screenshot: { name: captured }\n",
        &[("TOKEN", "supersecret")],
    );
    let warning = flow.recording_secret_warning().expect("warning");
    assert!(warning.contains("secret-derived"));
    assert!(!warning.contains("supersecret"));
}

#[test]
fn recording_secret_warning_is_present_for_a_secret_fill_with_video_on() {
    let flow = compile_with_env(
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: on }\nsteps:\n  - fill: { target: { css: email }, value: '${token}' }\n",
        &[("TOKEN", "supersecret")],
    );
    let warning = flow.recording_secret_warning().expect("warning");
    assert!(warning.contains("secret-derived"));
    assert!(!warning.contains("supersecret"));
}

#[test]
fn recording_secret_warning_is_present_for_a_runtime_output_fill() {
    let flow = compile_with_env(
        "version: 1\nname: x\nsettings: { video: on }\nsteps:\n  - evaluate: { script: 'return 1', save_as: later }\n  - fill: { target: { css: input }, value: '${later}' }\n",
        &[],
    );
    assert!(flow.recording_secret_warning().is_some());
}

#[test]
fn recording_secret_warning_is_absent_for_a_plain_fill_with_video_on() {
    let flow = compile_with_env(
        "version: 1\nname: x\nvars: { name: arjun }\nsettings: { video: on }\nsteps:\n  - fill: { target: { css: input }, value: '${name}' }\n",
        &[],
    );
    assert!(flow.recording_secret_warning().is_none());
}

#[test]
fn recording_secret_warning_is_present_for_a_secret_evaluate_arg() {
    let flow = compile_with_env(
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: on }\nsteps:\n  - evaluate: { script: 'return args[0]', args: ['${token}'] }\n",
        &[("TOKEN", "supersecret")],
    );
    assert!(flow.recording_secret_warning().is_some());
}

#[test]
fn recording_secret_warning_is_absent_when_secrets_are_only_used_in_requests() {
    let flow = compile_with_env(
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: on }\nsteps:\n  - request: { method: GET, url: https://api.test/, headers: { Authorization: 'Bearer ${token}' }, expected_status: 200 }\n",
        &[("TOKEN", "supersecret")],
    );
    assert!(
        flow.recording_secret_warning().is_none(),
        "request-only secret usage should not trip the page-rendering warning"
    );
}

#[test]
fn recording_secret_warning_is_present_for_secret_open_and_select_and_dialog() {
    for source in [
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: on }\nbase_url: https://example.test/\nsteps:\n  - open: 'https://example.test/${token}'\n",
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: on }\nsteps:\n  - select: { target: { css: select }, value: '${token}' }\n",
        "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsettings: { video: on }\nsteps:\n  - dialog: { action: accept, text: '${token}' }\n",
    ] {
        let flow = compile_with_env(source, &[("TOKEN", "supersecret")]);
        assert!(
            flow.recording_secret_warning().is_some(),
            "expected warning for {source}"
        );
    }
}

fn assert_percent_encoded_secret_redacted(upper_hex: bool) {
    let mut redactor = Redactor::default();
    redactor.add_secret("p@ss w0rd/x=y".to_owned());
    let (slash_encoded, equals_encoded) = if upper_hex {
        ("%2F", "%3D")
    } else {
        ("%2f", "%3d")
    };
    let encoded = format!("p%40ss%20w0rd{slash_encoded}x{equals_encoded}y");
    let form_encoded = format!("p%40ss+w0rd{slash_encoded}x{equals_encoded}y");
    let url = format!("https://api.test/?q={encoded}&keep=visible");
    let redacted = redactor.redact(&url);
    assert!(redacted.contains("[REDACTED]"), "{redacted}");
    assert!(!redacted.contains(&encoded), "{redacted}");
    assert!(redacted.contains("visible"));
    let form = format!("grant_type=password&q={form_encoded}&keep=visible");
    let redacted = redactor.redact(&form);
    assert!(redacted.contains("[REDACTED]"), "{redacted}");
    assert!(!redacted.contains(&form_encoded), "{redacted}");
    assert!(redacted.contains("visible"));
}

#[test]
fn redactor_redacts_percent_encoded_secrets_in_both_space_forms() {
    assert_percent_encoded_secret_redacted(true);
    let mut redactor = Redactor::default();
    redactor.add_secret("p@ss w0rd/x=y".to_owned());
    assert_eq!(redactor.redact("p@ss w0rd/x=y"), REDACTED);
}

#[test]
fn redactor_redacts_lowercase_percent_encoded_secrets() {
    assert_percent_encoded_secret_redacted(false);
}

#[test]
fn redactor_redacts_base64_encoded_secrets_in_all_padding_and_alphabet_variants() {
    let mut redactor = Redactor::default();
    let secret = "PLAINTEXT+SECRET/VALUE";
    redactor.add_secret(secret.to_owned());
    let standard = STANDARD.encode(secret.as_bytes());
    let standard_no_pad = STANDARD_NO_PAD.encode(secret.as_bytes());
    let urlsafe = URL_SAFE.encode(secret.as_bytes());
    let urlsafe_no_pad = URL_SAFE_NO_PAD.encode(secret.as_bytes());
    for encoding in [&standard, &standard_no_pad, &urlsafe, &urlsafe_no_pad] {
        assert_ne!(encoding, secret);
    }
    let redacted = redactor.redact(&format!(
        "auth={standard}&std_nopad={standard_no_pad}&url={urlsafe}&url_nopad={urlsafe_no_pad}"
    ));
    assert!(redacted.contains("[REDACTED]"), "{redacted}");
    for encoding in [&standard, &standard_no_pad, &urlsafe, &urlsafe_no_pad] {
        assert!(!redacted.contains(encoding), "{redacted}");
    }
    assert_eq!(
        redactor.redact(&format!("bearer {standard}")),
        "bearer [REDACTED]"
    );
}

#[test]
fn redactor_longest_first_ordering_wins_across_encodings_when_one_secret_is_a_prefix() {
    let mut redactor = Redactor::default();
    redactor.add_secret("secret-prefix".to_owned());
    redactor.add_secret("secret-prefix-extension".to_owned());
    assert_eq!(
        redactor.redact("secret-prefix-extension and secret-prefix"),
        "[REDACTED] and [REDACTED]"
    );

    let mut redactor = Redactor::default();
    redactor.add_secret("p@ss w0rd".to_owned());
    redactor.add_secret("p@ss w0rd/long".to_owned());
    let encoded_long = "p%40ss%20w0rd%2Flong";
    assert_eq!(
        redactor.redact(&format!("{encoded_long} and p@ss w0rd")),
        "[REDACTED] and [REDACTED]"
    );
}

#[test]
fn declared_secrets_shorter_than_four_characters_are_rejected_at_compile_time() {
    for short in ["ab", "x", "one"] {
        let environment = BTreeMap::from([("TOKEN".to_owned(), short.to_owned())]);
        let result = compile_yaml_with_env(
            "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ open: https://x.test }]\n",
            "flows/example.yaml",
            &BTreeMap::new(),
            &environment,
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("token"), "{message} for {short:?}");
        assert!(
            message.contains("4") || message.contains("four"),
            "{message} for {short:?}"
        );
        assert!(
            !message.contains(short),
            "error leaked short secret {short:?}: {message}"
        );
    }
}

#[test]
fn runtime_output_secrets_shorter_than_four_characters_are_not_redacted_away() {
    let mut redactor = Redactor::default();
    redactor.add_secret("ab".to_owned());
    redactor.add_secret("visible".to_owned());
    let redacted = redactor.redact("ab and visible");
    assert_eq!(redacted, "ab and [REDACTED]");
}
