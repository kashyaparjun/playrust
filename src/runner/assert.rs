#![allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxPropertyName, AxValue, GetFullAxTreeParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetContentQuadsParams, GetFrameOwnerParams,
    ResolveNodeParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EventJavascriptDialogOpening, EventScreencastFrame, FrameId,
    GetFrameTreeParams, GetNavigationHistoryParams, HandleJavaScriptDialogParams, NavigateParams,
    NavigateToHistoryEntryParams, ScreencastFrameAckParams, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams, Viewport as ScreenshotViewport,
};
use chromiumoxide::cdp::browser_protocol::storage::{ClearCookiesParams, ClearDataForOriginParams};
use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, ExecutionContextId, ReleaseObjectParams,
};
use chromiumoxide::error::CdpError;
use chromiumoxide::keys::get_key_definition;
use chromiumoxide::listeners::EventStream;
use chromiumoxide::page::ScreenshotParams;
use futures_util::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, oneshot};

use super::StepError;
use super::actions::call_on_target;
use super::actions::sleep_until_poll;
use super::context::ActiveContext;
use super::{INNER_TEXT_FUNCTION, VisualArtifacts, publish_bytes, screenshot_bytes};
use super::{assertion_locator_error, failure, locator_error, path_text, protocol, safe};
use crate::browser::{BrowserContext, BrowserHost, BrowserStatus, Geolocation, Viewport};
use crate::flow::{
    Assertion, ClearTarget, CompiledFlow, CompiledStep, Crop, Expression, FrameSwitch, GuardKind,
    Key, Locator, LocatorStrategy, MAX_RUNTIME_VALUE_BYTES, Modifier, NamedKey,
    NativeDialogResponse, Operation, PageSwitch, PresentationOverlays, RecordingControl, Redactor,
    RelationKind, RelativePoint, Resolved, SettleCondition, TextMatch, UrlExpectation, VideoMode,
    VisualExpectation, When,
};
use crate::locator::{
    Actionability, LocatorEngine, LocatorError, Observation, POLL_INTERVAL, ResolvedElement,
    id_selector, retryable, retryable_cdp_message, text_matches,
};
use crate::oopif::{CdpTarget, OopifRouter};
use crate::report::{
    ArtifactPaths, Failure, FailureCategory, FlowReport, FlowStatus, SafeText, StepContext,
};
use crate::session_snapshot::{
    Bounds as SnapshotBounds, CapturedElement, CapturedSnapshot, LocatorIdentity,
    Scroll as SnapshotScroll, SemanticNode, SemanticState, Viewport as SnapshotViewport,
};
use crate::video::{VideoConfig, VideoRecorder};
use crate::visual;

pub(crate) async fn assert(
    active: &ActiveContext,
    assertion: &Assertion,
    deadline: Instant,
) -> Result<(), StepError> {
    match assertion {
        Assertion::Visible(locator) => active
            .locator()
            .wait_unique(locator, Actionability::VISIBLE, None, deadline)
            .await
            .map(|_| ())
            .map_err(assertion_locator_error),
        Assertion::Hidden(locator) => assert_hidden(active, locator, deadline).await,
        Assertion::Text {
            target,
            expected,
            match_kind,
        } => assert_text(active, target, expected.expose(), *match_kind, deadline).await,
        Assertion::Url(expectation) => assert_url(active, expectation, deadline).await,
        Assertion::Screenshot(_) => unreachable!("visual assertions are executed with artifacts"),
    }
}

pub(crate) async fn assert_screenshot(
    active: &ActiveContext,
    expectation: &VisualExpectation,
    step: usize,
    artifact_directory: &Path,
) -> Result<(), StepError> {
    let actual_png = screenshot_bytes(active, expectation.crop).await?;
    let baseline = expectation.baseline.clone();
    let comparison_png = actual_png.clone();
    let tolerance = expectation.channel_tolerance;
    let comparison =
        tokio::task::spawn_blocking(move || visual::compare(&baseline, &comparison_png, tolerance))
            .await
            .map_err(|_| protocol("visual comparison task failed"))?
            .map_err(|error| match error {
                visual::VisualError::ActualDecode => protocol(error),
                _ => StepError::assertion(error.to_string()),
            })?;
    if comparison.dimensions_match && comparison.ratio() <= expectation.max_changed_ratio {
        return Ok(());
    }

    let diff_png = visual::encode_png(&comparison.diff).map_err(protocol)?;
    let actual_path = artifact_directory.join(format!("__visual-{step}-actual.png"));
    let diff_path = artifact_directory.join(format!("__visual-{step}-diff.png"));
    let observed = if comparison.dimensions_match {
        format!(
            "{} of {} pixels changed ({:.6}); maximum changed ratio is {:.6}",
            comparison.changed_pixels,
            comparison.total_pixels,
            comparison.ratio(),
            expectation.max_changed_ratio
        )
    } else {
        "baseline and actual dimensions differ".to_owned()
    };
    Err(
        StepError::assertion("visual screenshot assertion did not match")
            .observed(observed)
            .visual_artifacts(actual_path, diff_path, actual_png, diff_png),
    )
}

pub(crate) async fn publish_visual_artifacts(
    artifact_directory: &Path,
    artifacts: &VisualArtifacts,
) -> Result<(), StepError> {
    publish_bytes(
        artifact_directory,
        &artifacts.actual_path,
        &artifacts.actual_png,
    )
    .await?;
    publish_bytes(
        artifact_directory,
        &artifacts.diff_path,
        &artifacts.diff_png,
    )
    .await
}

pub(crate) async fn assert_hidden(
    active: &ActiveContext,
    locator: &Locator,
    deadline: Instant,
) -> Result<(), StepError> {
    loop {
        let observation = match active.locator().observe_any_visible(locator).await {
            Ok(observation) => observation,
            Err(error) if retryable(&error) => Observation::Unavailable {
                message: error.to_string(),
            },
            Err(error) => return Err(assertion_locator_error(error)),
        };
        match observation {
            Observation::NoMatch | Observation::Detached | Observation::Hidden => return Ok(()),
            other => {
                if Instant::now() >= deadline {
                    return Err(StepError::assertion("target remained visible")
                        .deadline()
                        .observed(other.to_string()));
                }
            }
        }
        sleep_until_poll(deadline).await;
    }
}

pub(crate) async fn assert_text(
    active: &ActiveContext,
    locator: &Locator,
    expected: &str,
    match_kind: TextMatch,
    deadline: Instant,
) -> Result<(), StepError> {
    let engine = active.locator();
    loop {
        let observation = match engine.observe_unique(locator, Actionability::VISIBLE).await {
            Ok(observation) => observation,
            Err(error) if retryable(&error) => Observation::Unavailable {
                message: error.to_string(),
            },
            Err(error) => return Err(assertion_locator_error(error)),
        };
        match observation {
            Observation::Ready(element) => {
                let actual: String = match call_on_target(
                    active.target(),
                    element.backend_node_id,
                    INNER_TEXT_FUNCTION,
                    &[],
                )
                .await
                {
                    Ok(actual) => actual,
                    Err(error)
                        if error.category == FailureCategory::Protocol
                            && retryable_cdp_message(&error.message) =>
                    {
                        if Instant::now() >= deadline {
                            return Err(StepError::assertion("text target was unavailable")
                                .deadline()
                                .observed(error.message));
                        }
                        sleep_until_poll(deadline).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if text_matches(&actual, expected, match_kind) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(StepError::assertion("text assertion did not match")
                        .deadline()
                        .observed(format!("expected {expected:?}; text was {actual:?}")));
                }
            }
            observation => {
                if Instant::now() >= deadline {
                    return Err(StepError::assertion("text target was not visible")
                        .deadline()
                        .observed(observation.to_string()));
                }
            }
        }
        sleep_until_poll(deadline).await;
    }
}

pub(crate) async fn assert_url(
    active: &ActiveContext,
    expectation: &UrlExpectation,
    deadline: Instant,
) -> Result<(), StepError> {
    loop {
        let actual = active.url().await.map_err(protocol)?.unwrap_or_default();
        if url_matches(&actual, expectation) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(StepError::assertion("URL assertion did not match")
                .deadline()
                .observed(format!("expected {expectation:?}; URL was {actual:?}")));
        }
        sleep_until_poll(deadline).await;
    }
}

pub(crate) fn url_matches(actual: &str, expectation: &UrlExpectation) -> bool {
    match expectation {
        UrlExpectation::Equals(expected) => actual == expected.expose().as_str(),
        UrlExpectation::Path(expected) => url::Url::parse(actual).is_ok_and(|actual| {
            let mut path = actual.path().to_owned();
            if let Some(query) = actual.query() {
                path.push('?');
                path.push_str(query);
            }
            path == *expected.expose()
        }),
    }
}
