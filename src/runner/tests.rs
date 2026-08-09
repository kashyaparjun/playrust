use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::install::{self, PINNED_CHROME_VERSION, Platform};

fn require_browser(test_name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(install::CHROME_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(root) = install::cache_root()
        && let Ok(platform) = Platform::current()
        && let Ok(path) = install::cached_browser_path(&root, PINNED_CHROME_VERSION, platform)
        && path.is_file()
    {
        return Some(path);
    }
    if std::env::var_os("PLAYRUST_REQUIRE_BROWSER").is_some_and(|value| {
        !matches!(
            value.to_str(),
            Some("0") | Some("false") | Some("no") | Some("")
        )
    }) {
        panic!(
            "{test_name}: no Chrome available (set PLAYRUST_CHROME or run `playrust browser install`)"
        );
    }
    eprintln!(
        "SKIP {test_name}: no Chrome available (set PLAYRUST_CHROME or run `playrust browser install`)"
    );
    None
}
use crate::flow::{compile_file, compile_yaml_with_env};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn pause_waits_until_duration_and_honors_deadline() {
    let started = Instant::now();
    assert!(
        pause_until(
            Duration::from_millis(25),
            Instant::now() + Duration::from_secs(1)
        )
        .await
        .is_ok()
    );
    assert!(started.elapsed() >= Duration::from_millis(20));
    let error = pause_until(
        Duration::from_secs(1),
        Instant::now() + Duration::from_millis(10),
    )
    .await
    .unwrap_err();
    assert_eq!(error.category, FailureCategory::Timeout);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_an_in_flight_compiled_pause_interrupts_the_flow() {
    let Some(chrome) =
        require_browser("cancelling_an_in_flight_compiled_pause_interrupts_the_flow")
    else {
        return;
    };
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let flow = crate::flow::compile_yaml(
        "version: 1\nname: cancelled-pause\nsettings: { timeout: 30s, video: off }\nsteps: [{ pause: 20s }]\n",
        "cancelled-pause.yaml",
        &BTreeMap::new(),
    )
    .unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let (pause_started, pause_started_rx) = tokio::sync::oneshot::channel();
    let pause_started = Arc::new(Mutex::new(Some(pause_started)));
    let mut options = RunOptions::new(artifacts.path()).with_cancellation(cancellation.clone());
    options.step_started_observer = Some(StepStartedObserver(Arc::new(move |operation| {
        if operation == "pause"
            && let Some(sender) = pause_started.lock().unwrap().take()
        {
            let _ = sender.send(());
        }
    })));
    let run = run_flow(&host, &flow, &options);
    let cancel_during_pause = async {
        tokio::time::timeout(Duration::from_secs(5), pause_started_rx)
            .await
            .expect("runner should enter the pause")
            .expect("pause observer should remain open");
        cancellation.cancel();
    };
    let (report, ()) = tokio::time::timeout(Duration::from_secs(7), async {
        tokio::join!(run, cancel_during_pause)
    })
    .await
    .expect("cancelled pause should return promptly");
    assert_eq!(report.status, FlowStatus::Interrupted);
    host.shutdown().await.unwrap();
}

#[test]
fn modifier_bits_follow_cdp() {
    assert_eq!(modifier_mask(&[]), 0);
    assert_eq!(
        modifier_mask(&[
            Modifier::Alt,
            Modifier::Control,
            Modifier::Meta,
            Modifier::Shift
        ]),
        15
    );
}

#[test]
fn previous_history_index_rejects_the_first_entry() {
    let error = previous_history_index(0).unwrap_err();
    assert_eq!(error.category, FailureCategory::Navigation);
    assert_eq!(error.message, "no previous history entry");
    assert!(matches!(previous_history_index(1), Ok(0)));
}

#[tokio::test]
async fn visual_publication_failure_does_not_replace_the_assertion() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    let primary = StepError::assertion("visual screenshot assertion did not match").observed("5%");
    let artifacts = VisualArtifacts {
        actual_path: blocked.join("actual.png"),
        diff_path: blocked.join("diff.png"),
        actual_png: vec![1],
        diff_png: vec![2],
    };

    let secondary = publish_visual_artifacts(&blocked, &artifacts)
        .await
        .unwrap_err();

    assert_eq!(primary.category, FailureCategory::Assertion);
    assert_eq!(primary.message, "visual screenshot assertion did not match");
    assert_eq!(primary.last_observed.as_deref(), Some("5%"));
    assert_eq!(secondary.category, FailureCategory::Protocol);
}

#[test]
fn runtime_json_is_compact_secret_and_size_bounded() {
    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - evaluate: { script: 'return 1', save_as: saved }\n  - fill: { target: { css: input }, value: 'prefix-${saved}' }\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    let Operation::Fill { value, .. } = &flow.steps[1].operation else {
        panic!("expected fill");
    };
    let outputs = BTreeMap::from([(
        "saved".to_owned(),
        Resolved::new(serde_json::json!({ "token": "canary" }), true),
    )]);
    let resolved =
        resolve_runtime(value, &outputs).unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(resolved.expose(), "prefix-{\"token\":\"canary\"}");
    assert!(resolved.is_secret());

    let mut stored = BTreeMap::new();
    let mut redactor = Redactor::default();
    store_output(
        &mut stored,
        &mut redactor,
        "small",
        Value::String("canary".to_owned()),
    )
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(redactor.redact("value=canary"), "value=[REDACTED]");
    assert!(
        store_output(
            &mut stored,
            &mut redactor,
            "large",
            Value::String("x".repeat(MAX_RUNTIME_VALUE_BYTES + 1)),
        )
        .is_err()
    );
}

#[test]
fn short_runtime_string_outputs_are_not_registered_for_redaction() {
    let mut stored = BTreeMap::new();
    let mut redactor = Redactor::default();
    store_output(
        &mut stored,
        &mut redactor,
        "short",
        Value::String("ab".to_owned()),
    )
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        redactor.redact("ab and \"ab\" and visible"),
        "ab and \"ab\" and visible"
    );

    store_output(
        &mut stored,
        &mut redactor,
        "long",
        Value::String("abcd".to_owned()),
    )
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(redactor.redact("abcd"), "[REDACTED]");
}

#[test]
fn structured_expressions_resolve_runtime_json_without_exposing_values() {
    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - evaluate: { script: 'return true', save_as: saved }\n  - when: { expression: { all: [{ boolean: '${saved}' }, { not_equals: { left: x, right: y } }] } }\n    open: https://x.test\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    let Some(When::Expression(expression)) = &flow.steps[1].when else {
        panic!("expected expression");
    };
    let outputs = BTreeMap::from([("saved".to_owned(), Resolved::new(Value::Bool(true), true))]);
    assert!(matches!(
        evaluate_expression(expression, &outputs),
        Ok(true)
    ));

    let outputs = BTreeMap::from([(
        "saved".to_owned(),
        Resolved::new(Value::String("canary-secret".to_owned()), true),
    )]);
    let message = evaluate_expression(expression, &outputs)
        .unwrap_err()
        .message;
    assert!(!message.contains("canary-secret"));
}

#[test]
fn while_guards_snapshot_subflow_iterations_and_stop_permanently() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    std::fs::write(
            &root,
            "version: 1\nname: root\nsteps:\n  - evaluate: { script: 'return true', save_as: state }\n  - while: { expression: { boolean: '${state}' }, max_iterations: 3 }\n    run: ./child.subflow.yaml\n",
        )
        .unwrap();
    std::fs::write(
        &child,
        "version: 1\nname: child\nsteps:\n  - open: https://one.test\n  - open: https://two.test\n",
    )
    .unwrap();
    let flow = compile_file(&root, &BTreeMap::new()).unwrap();
    assert_eq!(flow.steps.len(), 7);
    assert_eq!(flow.steps[1].source, child);

    let mut outputs =
        BTreeMap::from([("state".to_owned(), Resolved::new(Value::Bool(true), true))]);
    let mut results = BTreeMap::new();
    let mut stopped = BTreeSet::new();
    assert!(matches!(
        guards_match(&flow.steps[1].guards, &outputs, &mut results, &mut stopped),
        Ok(true)
    ));
    outputs.insert("state".to_owned(), Resolved::new(Value::Bool(false), true));
    assert!(matches!(
        guards_match(&flow.steps[2].guards, &outputs, &mut results, &mut stopped),
        Ok(true)
    ));
    assert!(matches!(
        guards_match(&flow.steps[3].guards, &outputs, &mut results, &mut stopped),
        Ok(false)
    ));
    outputs.insert("state".to_owned(), Resolved::new(Value::Bool(true), true));
    assert!(matches!(
        guards_match(&flow.steps[5].guards, &outputs, &mut results, &mut stopped),
        Ok(false)
    ));
}

#[test]
fn nested_runtime_json_strings_are_redacted_from_urls_and_diagnostics() {
    let mut stored = BTreeMap::new();
    let mut redactor = Redactor::default();
    store_output(
        &mut stored,
        &mut redactor,
        "secret",
        serde_json::json!({
            "auth": { "token": "object-canary" },
            "items": ["array-canary", { "value": "nested-array-canary" }]
        }),
    )
    .unwrap_or_else(|error| panic!("{}", error.message));

    let url = redactor
        .redact("https://example.test/object-canary/array-canary?nested=nested-array-canary");
    let diagnostic =
        redactor.redact("request failed for object-canary, array-canary, and nested-array-canary");
    for canary in ["object-canary", "array-canary", "nested-array-canary"] {
        assert!(!url.contains(canary), "secret leaked in URL: {url}");
        assert!(
            !diagnostic.contains(canary),
            "secret leaked in diagnostic: {diagnostic}"
        );
    }
}

#[tokio::test]
async fn http_requests_do_not_follow_redirects_with_custom_headers() {
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_url = format!("http://{}/target", target.local_addr().unwrap());
    let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let length = stream.read(&mut request).await.unwrap();
        request.truncate(length);
        let _ = request_sender.send(String::from_utf8_lossy(&request).into_owned());
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });

    let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect_url = format!("http://{}/redirect", redirect.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = redirect.accept().await.unwrap();
        let mut request = [0; 4096];
        let length = stream.read(&mut request).await.unwrap();
        assert!(length > 0);
        stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
    });

    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - request:\n      method: GET\n      url: http://example.test\n      headers: { x-api-key: redirect-canary }\n      expected_status: 200\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    let Operation::Request { headers, .. } = &flow.steps[0].operation else {
        panic!("expected request");
    };
    let response = http_request(
        "GET",
        &redirect_url,
        headers,
        None,
        302,
        false,
        &BTreeMap::new(),
    )
    .await;
    let redirected_request =
        tokio::time::timeout(Duration::from_millis(100), request_receiver).await;

    assert!(
        response.is_ok(),
        "redirect response was not returned: {}",
        response
            .err()
            .map(|error| error.message)
            .unwrap_or_default()
    );
    assert!(
        redirected_request.is_err(),
        "redirect target received x-api-key: {redirected_request:?}"
    );
}

#[tokio::test]
async fn http_transport_status_and_body_failures_are_request_failures() {
    let outputs = BTreeMap::new();
    let headers = BTreeMap::new();
    let invalid = http_request("GET", "not-a-url", &headers, None, 200, false, &outputs)
        .await
        .unwrap_err();
    assert_eq!(invalid.category, FailureCategory::Request);

    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable_url = format!("http://{}", unavailable.local_addr().unwrap());
    drop(unavailable);
    let transport = http_request(
        "GET",
        &unavailable_url,
        &headers,
        None,
        200,
        false,
        &outputs,
    )
    .await
    .unwrap_err();
    assert_eq!(transport.category, FailureCategory::Request);

    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_url = format!("http://{}", server.local_addr().unwrap());
    tokio::spawn(async move {
        for response in [
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RUNTIME_VALUE_BYTES + 1
            ),
        ] {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let status = http_request("GET", &server_url, &headers, None, 200, false, &outputs)
        .await
        .unwrap_err();
    assert_eq!(status.category, FailureCategory::Request);
    let body = http_request("GET", &server_url, &headers, None, 200, true, &outputs)
        .await
        .unwrap_err();
    assert_eq!(body.category, FailureCategory::Request);
}

#[test]
fn every_v1_named_key_has_a_chromium_definition() {
    for key in [
        NamedKey::Enter,
        NamedKey::Tab,
        NamedKey::Escape,
        NamedKey::Space,
        NamedKey::Backspace,
        NamedKey::Delete,
        NamedKey::ArrowUp,
        NamedKey::ArrowDown,
        NamedKey::ArrowLeft,
        NamedKey::ArrowRight,
        NamedKey::Home,
        NamedKey::End,
        NamedKey::PageUp,
        NamedKey::PageDown,
    ] {
        assert!(get_key_definition(key_name(&Key::Named(key))).is_some());
    }
}

#[test]
fn url_expectations_compare_exact_urls_or_path_and_query() {
    let flow = compile_yaml_with_env(
        "version: 1\nname: x\nsteps: [{ assert: { url: { path: '/a?q=1' } } }]\n",
        "x.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let Operation::Assert(Assertion::Url(expectation)) = &flow.steps[0].operation else {
        panic!("expected URL assertion");
    };
    assert!(url_matches(
        "https://example.test/a?q=1#fragment",
        expectation
    ));
    assert!(!url_matches("https://example.test/a?q=2", expectation));
}

#[test]
fn secret_locators_are_never_rendered_in_step_context() {
    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsecrets: { token: { env: TOKEN } }\nsteps: [{ click: { target: { css: button, has: { text: '${token}' } } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::from([("TOKEN".to_owned(), "canary-secret".to_owned())]),
        )
        .unwrap();
    let context = step_context(&flow, &flow.steps[0]);
    assert_eq!(context.locator.unwrap().as_str(), "[REDACTED]");
}

#[test]
fn public_locator_diagnostics_include_modifiers() {
    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps: [{ click: { target: { css: button, index: 1, checked: false, focused: true, enabled: true, child_of: { test_id: panel } } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    let context = step_context(&flow, &flow.steps[0]);
    assert_eq!(
        context.locator.unwrap().as_str(),
        "css=\"button\" index=1 checked=false focused=true enabled=true child_of=(test_id=\"panel\")"
    );
}

#[test]
fn viewport_click_diagnostics_do_not_claim_a_locator() {
    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsettings: { video: off, viewport: { width: 800, height: 600 } }\nsteps: [{ click: { point: { x: 100, y: 200 } } }]\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    let context = step_context(&flow, &flow.steps[0]);
    assert_eq!(context.operation, "click.point");
    assert!(context.locator.is_none());
}

#[test]
fn double_click_step_context_uses_its_action_name_and_target() {
    let flow = compile_yaml_with_env(
        "version: 1\nname: x\nsteps: [{ double_click: { target: { css: button } } }]\n",
        "x.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let context = step_context(&flow, &flow.steps[0]);
    assert_eq!(context.operation, "double_click");
    assert_eq!(context.locator.unwrap().as_str(), "css=\"button\"");
}

#[test]
fn interaction_step_contexts_include_only_targeted_locators() {
    let flow = compile_yaml_with_env(
            "version: 1\nname: x\nsteps:\n  - erase: { target: { css: input } }\n  - select: { target: { css: select }, value: x }\n  - scroll: { y: 1 }\n  - scroll_until_visible: { target: { css: .item }, y: 100 }\n  - swipe: { target: { css: .card }, x: 1 }\n  - long_press: { target: { css: button } }\n  - wait_until_visible: { target: { css: .late } }\n  - wait_until_stable: { target: { css: .moving } }\n  - back: {}\n",
            "x.yaml",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    for (step, name, locator) in [
        (&flow.steps[0], "erase", Some("css=\"input\"")),
        (&flow.steps[1], "select", Some("css=\"select\"")),
        (&flow.steps[2], "scroll", None),
        (
            &flow.steps[3],
            "scroll_until_visible",
            Some("css=\".item\""),
        ),
        (&flow.steps[4], "swipe", Some("css=\".card\"")),
        (&flow.steps[5], "long_press", Some("css=\"button\"")),
        (&flow.steps[6], "wait_until_visible", Some("css=\".late\"")),
        (&flow.steps[7], "wait_until_stable", Some("css=\".moving\"")),
        (&flow.steps[8], "back", None),
    ] {
        let context = step_context(&flow, step);
        assert_eq!(context.operation, name);
        assert_eq!(context.locator.as_ref().map(SafeText::as_str), locator);
    }
}

#[test]
fn open_settle_step_context_includes_the_settle_locator() {
    let flow = compile_yaml_with_env(
        "version: 1\nname: x\nbase_url: https://example.test/\nsteps:\n  - open: { url: /a, wait_until: { visible: { css: '#heading' } } }\n  - open: { url: /b, wait_until: { stable: { test_id: hero } } }\n  - open: /c\n",
        "x.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let visible = step_context(&flow, &flow.steps[0]);
    assert_eq!(visible.operation, "open");
    assert_eq!(
        visible.locator.as_ref().map(SafeText::as_str),
        Some("css=\"#heading\"")
    );
    let stable = step_context(&flow, &flow.steps[1]);
    assert_eq!(stable.operation, "open");
    assert_eq!(
        stable.locator.as_ref().map(SafeText::as_str),
        Some("test_id=\"hero\"")
    );
    let plain = step_context(&flow, &flow.steps[2]);
    assert_eq!(plain.operation, "open");
    assert!(plain.locator.is_none());
}

#[test]
fn open_settle_budget_is_exhausted_at_or_past_the_deadline() {
    let past = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("deadline in the past");
    let error = prepare_open_settle(past).expect_err("budget exhausted");
    assert_eq!(error.category, FailureCategory::Timeout);
    assert!(error.deadline_based);
    assert!(
        error.message.contains(
            "navigation completed without enough remaining time for the open settle condition"
        ),
        "{}",
        error.message
    );
}

#[test]
fn open_settle_deadline_keeps_slack_before_the_step_deadline() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let settle_by = prepare_open_settle(deadline).unwrap_or_else(|error| {
        panic!("budget available: {}", error.message);
    });
    assert!(settle_by < deadline);
    assert!(deadline.duration_since(settle_by) >= OPEN_SETTLE_DEADLINE_SLACK);
}

#[test]
fn open_settle_budget_is_exhausted_when_slack_does_not_fit() {
    let deadline = Instant::now() + OPEN_SETTLE_DEADLINE_SLACK / 2;
    let error = prepare_open_settle(deadline).expect_err("slack must not fit");
    assert_eq!(error.category, FailureCategory::Timeout);
    assert!(error.deadline_based);
    assert!(
        error.message.contains(
            "navigation completed without enough remaining time for the open settle condition"
        ),
        "{}",
        error.message
    );
}

#[test]
fn included_step_context_preserves_child_source_and_local_number() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root.yaml");
    let child = directory.path().join("child.subflow.yaml");
    std::fs::write(
            &root,
            "version: 1\nname: root\nsettings: { video: off }\nsteps: [{ run: ./child.subflow.yaml }]\n",
        )
        .unwrap();
    std::fs::write(
            &child,
            "version: 1\nname: child\nsteps:\n  - open: https://example.test\n  - assert: { visible: { css: missing } }\n",
        )
        .unwrap();
    let flow = crate::flow::compile_file(&root, &BTreeMap::new()).unwrap();

    let context = step_context(&flow, &flow.steps[1]);

    assert_eq!(context.number, 2);
    assert_eq!(context.source_step, Some(2));
    assert!(
        context
            .source
            .as_deref()
            .is_some_and(|source| source.ends_with("child.subflow.yaml"))
    );
}

#[test]
fn deadline_based_failures_include_timeout_for_all_automation_categories() {
    for error in [
        locator_error(LocatorError::Timeout {
            last: Observation::NoMatch,
        }),
        locator_error(LocatorError::Timeout {
            last: Observation::Hidden,
        }),
        assertion_locator_error(LocatorError::Timeout {
            last: Observation::NoMatch,
        }),
    ] {
        assert_eq!(
            deadline_timeout_ms(&error, Duration::from_millis(321)),
            Some(321)
        );
    }
}

#[test]
fn report_preserves_failure_order_and_uses_infrastructure_precedence() {
    let flow = compile_yaml_with_env(
        "version: 1\nname: x\nsteps: [{ open: https://x.test }]\n",
        "x.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let failures = vec![
        failure(&flow, FailureCategory::Assertion, "automation", None),
        failure(&flow, FailureCategory::Recording, "cleanup", None),
    ];
    let report = report(
        &flow,
        Instant::now(),
        ArtifactPaths::default(),
        failures,
        false,
    );

    assert_eq!(report.failures[0].category, FailureCategory::Assertion);
    assert_eq!(report.failures[1].category, FailureCategory::Recording);
    assert_eq!(report.exit_code(), crate::report::ExitCode::Infrastructure);
}

#[test]
fn shifted_character_events_use_shifted_and_unmodified_text() {
    assert_eq!(
        character_text('a', &[Modifier::Shift]),
        ("A".to_owned(), "a".to_owned())
    );
    assert_eq!(
        character_text('1', &[Modifier::Shift]),
        ("!".to_owned(), "1".to_owned())
    );
    assert_eq!(character_text('a', &[]), ("a".to_owned(), "a".to_owned()));
}

#[test]
fn frame_points_follow_scaled_and_rotated_content_quads() {
    assert_eq!(
        map_frame_point(
            &[10.0, 20.0, 210.0, 20.0, 210.0, 120.0, 10.0, 120.0],
            100.0,
            50.0,
            25.0,
            10.0
        )
        .unwrap_or_else(|error| panic!("{}", error.message)),
        (60.0, 40.0)
    );
    assert_eq!(
        map_frame_point(
            &[100.0, 0.0, 100.0, 100.0, 50.0, 100.0, 50.0, 0.0],
            100.0,
            50.0,
            20.0,
            10.0
        )
        .unwrap_or_else(|error| panic!("{}", error.message)),
        (90.0, 20.0)
    );
}

#[test]
fn snapshot_bounds_are_viewport_relative_for_scaled_and_rotated_frames() {
    let bounds = SnapshotTransform {
        origin: (100.0, 20.0),
        horizontal: (0.0, 2.0),
        vertical: (-3.0, 0.0),
    }
    .bounds(crate::locator::Rect {
        x: 10.0,
        y: 5.0,
        width: 20.0,
        height: 10.0,
    });

    assert_eq!(bounds.x, 55.0);
    assert_eq!(bounds.y, 40.0);
    assert_eq!(bounds.width, 30.0);
    assert_eq!(bounds.height, 40.0);
}

#[test]
fn fill_clears_text_controls_without_unsupported_select_or_change_events() {
    assert!(PREPARE_FILL_FUNCTION.contains("HTMLInputElement.prototype, 'value'"));
    assert!(PREPARE_FILL_FUNCTION.contains("HTMLTextAreaElement.prototype, 'value'"));
    assert!(PREPARE_FILL_FUNCTION.contains("this.isContentEditable"));
    assert!(PREPARE_FILL_FUNCTION.contains("range.selectNodeContents(this)"));
    assert!(!PREPARE_FILL_FUNCTION.contains("this.select()"));
    assert!(!PREPARE_FILL_FUNCTION.contains("dispatchEvent"));
}

#[test]
fn erase_and_select_dispatch_native_form_events_once() {
    assert_eq!(ERASE_FUNCTION.matches("dispatchEvent").count(), 2);
    assert!(ERASE_FUNCTION.contains("HTMLInputElement.prototype, 'value'"));
    assert!(ERASE_FUNCTION.contains("this.replaceChildren()"));
    assert_eq!(SELECT_FUNCTION.matches("dispatchEvent").count(), 2);
    assert!(SELECT_FUNCTION.contains("this instanceof HTMLSelectElement"));
    assert!(SELECT_FUNCTION.contains("this.multiple"));
    assert!(SELECT_FUNCTION.contains("option.value === value"));
}

#[tokio::test(flavor = "current_thread")]
async fn fill_replaces_all_supported_text_controls_in_chrome() {
    let Some(chrome) = require_browser("fill_replaces_all_supported_text_controls_in_chrome")
    else {
        return;
    };
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let context = host
        .create_context(Viewport::new(800, 600).unwrap(), None)
        .await
        .unwrap();
    let page = context.page().clone();
    page.set_content(
            r#"<input id="text" value="old"><input id="search" type="search" value="old">
                <input id="email" type="email" value="old@example.test">
                <input id="url" type="url" value="https://old.test"><input id="tel" type="tel" value="old">
                <input id="password" type="password" value="old"><textarea id="textarea">old</textarea>
                <div id="editable" contenteditable>old</div>"#,
        )
        .await
        .unwrap();

    for id in [
        "text", "search", "email", "url", "tel", "password", "textarea", "editable",
    ] {
        let element = page.find_element(format!("#{id}")).await.unwrap();
        prepare_fill(CdpTarget::Root(&page), element.backend_node_id)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        page.execute(InsertTextParams::new("replacement"))
            .await
            .unwrap();
        let value: String = call_on_target(
            CdpTarget::Root(&page),
            element.backend_node_id,
            "function() { return this.isContentEditable ? this.innerText : this.value; }",
            &[],
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(value, "replacement", "failed to replace #{id}");
    }

    host.dispose_context(context).await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn incompatible_and_mixed_namespace_nodes_do_not_break_text_locators_in_chrome() {
    let Some(chrome) = require_browser(
        "incompatible_and_mixed_namespace_nodes_do_not_break_text_locators_in_chrome",
    ) else {
        return;
    };
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let context = host
        .create_context(Viewport::new(800, 600).unwrap(), None)
        .await
        .unwrap();
    let page = context.page().clone();
    page.set_content(
        r#"<script>
              customElements.define('poison-text', class extends HTMLElement {
                get innerText() { throw new TypeError('incompatible text node'); }
              });
            </script>
            <poison-text>unrelated</poison-text>
            <svg width="100" height="20"><text x="0" y="15">unrelated</text></svg>
            <math><mtext>also unrelated</mtext></math>
            <button id="2fa">Continue</button>"#,
    )
    .await
    .unwrap();

    let target = page.find_element(id_selector("2fa")).await.unwrap();
    let locator = simple_locator(LocatorStrategy::Text {
        value: Resolved::new("Continue".to_owned(), false),
        match_kind: TextMatch::Exact,
    });
    let candidates = LocatorEngine::new(&page)
        .resolve_all(&locator)
        .await
        .unwrap();
    assert_eq!(candidates.backend_node_ids, [target.backend_node_id]);

    host.dispose_context(context).await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn labels_resolve_native_wrapping_and_aria_names_in_chrome() {
    let Some(chrome) = require_browser("labels_resolve_native_wrapping_and_aria_names_in_chrome")
    else {
        return;
    };
    let host = BrowserHost::launch(chrome, false).await.unwrap();
    let context = host
        .create_context(Viewport::new(800, 600).unwrap(), None)
        .await
        .unwrap();
    let page = context.page().clone();
    page.set_content(
            r#"<style>label { text-transform: uppercase }</style>
                <label>Email <input id="wrapped"></label>
                <label>Ignored aria <input id="aria" aria-label="Alias"></label>
                <span id="account">Account</span><span id="owner"> owner</span>
                <label>Ignored labelled <input id="labelled" aria-labelledby="account owner"></label>"#,
        )
        .await
        .unwrap();
    let flow = compile_yaml_with_env(
        r#"version: 1
name: labels
steps:
  - fill: { target: { label: Email }, value: wrapped }
  - fill: { target: { label: Alias }, value: aria }
  - fill: { target: { label: Account owner }, value: labelled }
  - assert: { hidden: { label: Ignored aria } }
  - assert: { hidden: { label: Ignored labelled } }
  - assert: { hidden: { label: email } }
"#,
        "labels.yaml",
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let mut active = ActiveContext::new(page.clone());

    let mut runtime = RuntimeState {
        outputs: BTreeMap::new(),
        redactor: flow.redactor.clone(),
        page_settings: PageSettings {
            viewport: Viewport::new(800, 600).unwrap(),
            geolocation: None,
        },
        guard_results: BTreeMap::new(),
        stopped_loops: BTreeSet::new(),
        expects_dialog: false,
        dialog_listener: None,
        presentation_overlays: PresentationOverlays::default(),
        presentation_overlay_recording: false,
    };
    for step in &flow.steps {
        execute_step(
            &host,
            context.id(),
            &mut active,
            step,
            Instant::now() + Duration::from_secs(2),
            Path::new("."),
            &mut runtime,
        )
        .await
        .unwrap_or_else(|error| panic!("{}: {:?}", error.message, error.last_observed));
    }
    for (id, expected) in [
        ("wrapped", "wrapped"),
        ("aria", "aria"),
        ("labelled", "labelled"),
    ] {
        let element = page.find_element(format!("#{id}")).await.unwrap();
        let value: String = call_on_target(
            CdpTarget::Root(&page),
            element.backend_node_id,
            "function() { return this.value; }",
            &[],
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(value, expected);
    }

    host.dispose_context(context).await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_token_wakes_existing_and_future_waiters() {
    let token = CancellationToken::new();
    let waiter = tokio::spawn({
        let token = token.clone();
        async move { token.cancelled().await }
    });

    token.cancel();
    waiter.await.unwrap();
    token.cancelled().await;
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn video_start_await_obeys_cancellation_and_deadline() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        await_video_start(
            Some(&cancellation),
            Instant::now() + Duration::from_secs(1),
            std::future::pending::<()>(),
        )
        .await,
        VideoStartAwait::Cancelled
    ));
    assert!(matches!(
        await_video_start(None, Instant::now(), std::future::pending::<()>()).await,
        VideoStartAwait::Deadline
    ));
}

#[test]
fn screencast_errors_retain_failure_only_video() {
    assert!(should_retain_video(false, &["stream failed".to_owned()]));
    assert!(should_retain_video(true, &[]));
    assert!(!should_retain_video(false, &[]));
}
