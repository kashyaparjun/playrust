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

use super::super::StepError;
use super::super::context::ActiveContext;
use super::super::state::RuntimeState;
use super::super::{
    FRAME_SIZE_FUNCTION, INNER_TEXT_FUNCTION, key_name, modifier_mask, publish_bytes,
    screenshot_bytes, settle_video,
};
use super::super::{assertion_locator_error, failure, locator_error, path_text, protocol, safe};
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

pub(crate) async fn dispatch_scroll(
    active: &ActiveContext,
    x: i64,
    y: i64,
) -> Result<(), StepError> {
    let [width, height]: [f64; 2] = evaluate_value(active, "[innerWidth, innerHeight]").await?;
    let (center_x, center_y) = page_point(active, width / 2.0, height / 2.0).await?;
    let event = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(center_x)
        .y(center_y)
        .delta_x(x as f64)
        .delta_y(y as f64)
        .build()
        .expect("all mandatory wheel event fields are set");
    active.page.execute(event).await.map_err(protocol)?;
    Ok(())
}

pub(crate) async fn scroll_until_visible(
    active: &ActiveContext,
    target: &Locator,
    x: i32,
    y: i32,
    deadline: Instant,
) -> Result<(), StepError> {
    let engine = active.locator();
    loop {
        let observation = match engine.observe_unique(target, Actionability::VISIBLE).await {
            Ok(Observation::Ready(_)) => return Ok(()),
            Ok(observation) => observation,
            Err(error) if retryable(&error) => Observation::Unavailable {
                message: error.to_string(),
            },
            Err(error) => return Err(locator_error(error)),
        };
        if Instant::now() >= deadline {
            return Err(StepError::new(
                FailureCategory::Timeout,
                "scroll_until_visible deadline expired",
            )
            .deadline()
            .observed(observation.to_string()));
        }
        dispatch_scroll(active, i64::from(x), i64::from(y)).await?;
        sleep_until_poll(deadline).await;
    }
}

pub(crate) async fn dispatch_swipe(
    active: &ActiveContext,
    element: &ResolvedElement,
    x: i32,
    y: i32,
    duration: Duration,
    deadline: Instant,
) -> Result<(), StepError> {
    let end_x = element.center.x + f64::from(x);
    let end_y = element.center.y + f64::from(y);
    let [width, height]: [f64; 2] = evaluate_value(active, "[innerWidth, innerHeight]").await?;
    if end_x < 0.0 || end_y < 0.0 || end_x >= width || end_y >= height {
        return Err(StepError::new(
            FailureCategory::Actionability,
            "swipe endpoint is outside the viewport",
        ));
    }
    require_gesture_time(duration, deadline, "swipe")?;
    let (start_x, start_y) = page_point(active, element.center.x, element.center.y).await?;
    let (end_x, end_y) = page_point(active, end_x, end_y).await?;
    dispatch_pointer(
        &active.page,
        DispatchMouseEventType::MousePressed,
        start_x,
        start_y,
        1,
    )
    .await?;
    tokio::time::sleep(duration).await;
    let moved = dispatch_pointer(
        &active.page,
        DispatchMouseEventType::MouseMoved,
        end_x,
        end_y,
        1,
    )
    .await;
    let released = dispatch_pointer(
        &active.page,
        DispatchMouseEventType::MouseReleased,
        end_x,
        end_y,
        0,
    )
    .await;
    moved.and(released)
}

pub(crate) async fn dispatch_long_press(
    active: &ActiveContext,
    element: &ResolvedElement,
    duration: Duration,
    deadline: Instant,
) -> Result<(), StepError> {
    require_gesture_time(duration, deadline, "long_press")?;
    let (x, y) = page_point(active, element.center.x, element.center.y).await?;
    dispatch_pointer(&active.page, DispatchMouseEventType::MousePressed, x, y, 1).await?;
    tokio::time::sleep(duration).await;
    dispatch_pointer(&active.page, DispatchMouseEventType::MouseReleased, x, y, 0).await
}

pub(crate) fn require_gesture_time(
    duration: Duration,
    deadline: Instant,
    operation: &str,
) -> Result<(), StepError> {
    if Instant::now()
        .checked_add(duration)
        .is_none_or(|finished| finished >= deadline)
    {
        return Err(StepError::new(
            FailureCategory::Timeout,
            format!("{operation} duration exceeds the remaining step deadline"),
        )
        .deadline());
    }
    Ok(())
}

pub(crate) async fn dispatch_pointer(
    page: &Page,
    event_type: DispatchMouseEventType,
    x: f64,
    y: f64,
    buttons: i64,
) -> Result<(), StepError> {
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(event_type)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .buttons(buttons)
            .build()
            .expect("all mandatory pointer event fields are set"),
    )
    .await
    .map_err(protocol)?;
    Ok(())
}

pub(crate) async fn page_point(
    active: &ActiveContext,
    mut x: f64,
    mut y: f64,
) -> Result<(f64, f64), StepError> {
    let Some(mut index) = active.frames.len().checked_sub(1) else {
        return Ok((x, y));
    };
    loop {
        let frame = &active.frames[index];
        let target = active.target_before(index);
        let owner = target
            .execute(GetFrameOwnerParams::new(frame.id.clone()))
            .await
            .map_err(protocol)?
            .backend_node_id;
        let [width, height]: [f64; 2] =
            call_on_target(target, owner, FRAME_SIZE_FUNCTION, &[]).await?;
        let quad = target
            .execute(
                GetContentQuadsParams::builder()
                    .backend_node_id(owner)
                    .build(),
            )
            .await
            .map_err(protocol)?
            .quads
            .into_iter()
            .next()
            .ok_or_else(|| protocol("active frame has no content quad"))?;
        (x, y) = map_frame_point(quad.inner(), width, height, x, y)?;
        let Some(parent_oopif) = active.frames[..index].iter().rposition(|frame| {
            active
                .router
                .as_deref()
                .is_some_and(|router| router.has_target(frame.id.as_ref()))
        }) else {
            break;
        };
        index = parent_oopif;
    }
    Ok((x, y))
}

pub(crate) fn map_frame_point(
    quad: &[f64],
    width: f64,
    height: f64,
    x: f64,
    y: f64,
) -> Result<(f64, f64), StepError> {
    if quad.len() != 8 || width <= 0.0 || height <= 0.0 {
        return Err(protocol("active frame has invalid content geometry"));
    }
    let horizontal = x / width;
    let vertical = y / height;
    Ok((
        quad[0] + (quad[2] - quad[0]) * horizontal + (quad[6] - quad[0]) * vertical,
        quad[1] + (quad[3] - quad[1]) * horizontal + (quad[7] - quad[1]) * vertical,
    ))
}

pub(crate) async fn evaluate(active: &ActiveContext, expression: &str) -> Result<(), StepError> {
    evaluate_value::<serde_json::Value>(active, expression)
        .await
        .map(|_| ())
}

pub(crate) async fn evaluate_value<T: DeserializeOwned>(
    active: &ActiveContext,
    expression: &str,
) -> Result<T, StepError> {
    if active.local_frame().is_none() {
        let params = EvaluateParams::builder()
            .expression(expression)
            .return_by_value(true)
            .await_promise(true)
            .build()
            .map_err(protocol)?;
        let response = active.target().execute(params).await.map_err(protocol)?;
        if let Some(exception) = response.exception_details {
            return Err(protocol(format!(
                "page expression threw: {}",
                exception.text
            )));
        }
        return serde_json::from_value(
            response
                .result
                .value
                .ok_or_else(|| protocol("page expression returned no value"))?,
        )
        .map_err(protocol);
    }
    if let CdpTarget::Oopif(_, _) = active.target() {
        let frame = active.local_frame().expect("local frame checked");
        let context = active
            .target()
            .execution_context(frame.as_ref())
            .ok_or_else(|| protocol("active frame has no executable context"))?;
        let params = EvaluateParams::builder()
            .expression(expression)
            .context_id(ExecutionContextId::new(context))
            .return_by_value(true)
            .await_promise(true)
            .build()
            .map_err(protocol)?;
        let response = active.target().execute(params).await.map_err(protocol)?;
        if let Some(exception) = response.exception_details {
            return Err(protocol(format!(
                "page expression threw: {}",
                exception.text
            )));
        }
        return serde_json::from_value(
            response
                .result
                .value
                .ok_or_else(|| protocol("page expression returned no value"))?,
        )
        .map_err(protocol);
    }
    let context = active
        .page
        .frame_execution_context(active.frame().expect("frame checked").clone())
        .await
        .map_err(protocol)?
        .ok_or_else(|| {
            StepError::new(
                FailureCategory::Protocol,
                "active frame has no executable context",
            )
        })?;
    let params = EvaluateParams::builder()
        .expression(expression)
        .context_id(context)
        .return_by_value(true)
        .await_promise(true)
        .build()
        .map_err(protocol)?;
    let response = active.page.execute(params).await.map_err(protocol)?.result;
    if let Some(exception) = response.exception_details {
        return Err(StepError::new(
            FailureCategory::Protocol,
            format!("page expression threw: {}", exception.text),
        ));
    }
    serde_json::from_value(response.result.value.ok_or_else(|| {
        StepError::new(
            FailureCategory::Protocol,
            "page expression returned no value",
        )
    })?)
    .map_err(protocol)
}

pub(crate) async fn navigate_back(page: &Page, deadline: Instant) -> Result<(), StepError> {
    let history = page
        .execute(GetNavigationHistoryParams::default())
        .await
        .map_err(protocol)?
        .result;
    let target_index = previous_history_index(history.current_index)?;
    let target = history.entries.get(target_index as usize).ok_or_else(|| {
        StepError::new(
            FailureCategory::Protocol,
            "Chromium navigation history omitted the previous entry",
        )
    })?;

    page.execute(NavigateToHistoryEntryParams::new(target.id))
        .await
        .map_err(|error| StepError::new(FailureCategory::Navigation, error.to_string()))?;

    loop {
        match page.execute(GetNavigationHistoryParams::default()).await {
            Ok(response) if response.result.current_index == target_index => {
                match page.evaluate("document.readyState !== 'loading'").await {
                    Ok(value) => {
                        if value.into_value::<bool>().unwrap_or(false) {
                            return Ok(());
                        }
                    }
                    Err(error) if retryable_cdp_message(&error.to_string()) => {}
                    Err(error) => return Err(protocol(error)),
                }
            }
            Ok(_) => {}
            Err(error) if retryable_cdp_message(&error.to_string()) => {}
            Err(error) => return Err(protocol(error)),
        }
        if Instant::now() >= deadline {
            return Err(StepError::new(
                FailureCategory::Timeout,
                "back navigation deadline expired",
            )
            .deadline());
        }
        sleep_until_poll(deadline).await;
    }
}

pub(crate) fn previous_history_index(current_index: i64) -> Result<i64, StepError> {
    current_index
        .checked_sub(1)
        .filter(|index| *index >= 0)
        .ok_or_else(|| StepError::new(FailureCategory::Navigation, "no previous history entry"))
}

pub(crate) async fn dispatch_key(
    page: &Page,
    key: &Key,
    modifiers: &[Modifier],
) -> Result<(), StepError> {
    let character = match key {
        Key::Character(character) => Some(character_text(*character, modifiers)),
        Key::Named(_) => None,
    };
    let name = character
        .as_ref()
        .map_or_else(|| key_name(key), |(text, _)| text.clone());
    let definition = get_key_definition(&name);
    if definition.is_none() && !matches!(key, Key::Character(_)) {
        return Err(StepError::new(
            FailureCategory::Protocol,
            format!("Chromium has no key definition for {name:?}"),
        ));
    }
    let code = definition.map_or("", |definition| definition.code);
    let key_code = definition.map_or(0, |definition| definition.key_code);
    let modifier_bits = modifier_mask(modifiers);
    let command = |event_type| {
        DispatchKeyEventParams::builder()
            .r#type(event_type)
            .modifiers(modifier_bits)
            .key(&name)
            .code(code)
            .windows_virtual_key_code(key_code)
            .native_virtual_key_code(key_code)
            .build()
            .expect("all mandatory key event fields are set")
    };
    page.execute(command(DispatchKeyEventType::RawKeyDown))
        .await
        .map_err(protocol)?;

    let character_result = if let Some((text, unmodified_text)) = character
        && !modifiers
            .iter()
            .any(|value| matches!(value, Modifier::Alt | Modifier::Control | Modifier::Meta))
    {
        let mut character_event = command(DispatchKeyEventType::Char);
        character_event.text = Some(text);
        character_event.unmodified_text = Some(unmodified_text);
        page.execute(character_event)
            .await
            .map(|_| ())
            .map_err(protocol)
    } else {
        Ok(())
    };

    let release_result = page
        .execute(command(DispatchKeyEventType::KeyUp))
        .await
        .map(|_| ())
        .map_err(protocol);
    character_result.and(release_result)
}

pub(crate) fn character_text(character: char, modifiers: &[Modifier]) -> (String, String) {
    let unmodified = character.to_string();
    if !modifiers.contains(&Modifier::Shift) {
        return (unmodified.clone(), unmodified);
    }
    let shifted = match character {
        'a'..='z' => character.to_ascii_uppercase(),
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => character,
    };
    (shifted.to_string(), unmodified)
}

pub(crate) async fn dialog_listener<'a>(
    page: &Page,
    runtime: &'a mut RuntimeState,
) -> Result<Option<&'a mut EventStream<EventJavascriptDialogOpening>>, StepError> {
    if !runtime.expects_dialog {
        return Ok(None);
    }
    if runtime.dialog_listener.is_none() {
        runtime.dialog_listener = Some(
            page.event_listener::<EventJavascriptDialogOpening>()
                .await
                .map_err(protocol)?,
        );
    }
    Ok(runtime.dialog_listener.as_mut())
}

pub(crate) async fn dispatch_flow_click(
    page: &Page,
    x: f64,
    y: f64,
    clicks: i64,
    runtime: &mut RuntimeState,
) -> Result<(), StepError> {
    let settle_after_mouse_press =
        runtime.presentation_overlays_active() && runtime.presentation_overlays.pointer;
    let dialogs = dialog_listener(page, runtime).await?;
    dispatch_click(page, x, y, clicks, settle_after_mouse_press, dialogs).await
}

pub(crate) async fn dispatch_click(
    page: &Page,
    x: f64,
    y: f64,
    clicks: i64,
    settle_after_mouse_press: bool,
    mut dialogs: Option<&mut EventStream<EventJavascriptDialogOpening>>,
) -> Result<(), StepError> {
    page.move_mouse(chromiumoxide::layout::Point::new(x, y))
        .await
        .map_err(protocol)?;
    for click_count in 1..=clicks {
        let event = |event_type| {
            DispatchMouseEventParams::builder()
                .r#type(event_type)
                .x(x)
                .y(y)
                .button(MouseButton::Left)
                .click_count(click_count)
                .build()
                .expect("all mandatory mouse event fields are set")
        };
        if dispatch_mouse_event(
            page,
            event(DispatchMouseEventType::MousePressed),
            dialogs.as_deref_mut(),
        )
        .await?
        {
            return Ok(());
        }
        if settle_after_mouse_press {
            settle_video(page).await;
        }
        if dispatch_mouse_event(
            page,
            event(DispatchMouseEventType::MouseReleased),
            dialogs.as_deref_mut(),
        )
        .await?
        {
            return Ok(());
        }
    }
    Ok(())
}

pub(crate) async fn dispatch_mouse_event(
    page: &Page,
    event: DispatchMouseEventParams,
    dialogs: Option<&mut EventStream<EventJavascriptDialogOpening>>,
) -> Result<bool, StepError> {
    let mut command = Box::pin(page.execute(event));
    let Some(dialogs) = dialogs else {
        return command.await.map(|_| false).map_err(protocol);
    };
    tokio::select! {
        result = &mut command => result.map(|_| false).map_err(protocol),
        dialog = dialogs.next() => match dialog {
            Some(_) => Ok(true),
            None => command.await.map(|_| false).map_err(protocol),
        },
    }
}

pub(crate) async fn sleep_until_poll(deadline: Instant) {
    tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))).await;
}

pub(crate) async fn call_on_target<T: DeserializeOwned>(
    target: CdpTarget<'_>,
    node: BackendNodeId,
    function: &str,
    arguments: &[serde_json::Value],
) -> Result<T, StepError> {
    let object = target
        .execute(ResolveNodeParams::builder().backend_node_id(node).build())
        .await
        .map_err(protocol)?
        .object;
    let object_id = object.object_id.ok_or_else(|| {
        StepError::new(
            FailureCategory::Protocol,
            "resolved DOM node had no object id",
        )
    })?;
    let params = CallFunctionOnParams::builder()
        .function_declaration(function)
        .object_id(object_id.clone())
        .arguments(arguments.iter().cloned().map(|value| {
            chromiumoxide::cdp::js_protocol::runtime::CallArgument::builder()
                .value(value)
                .build()
        }))
        .return_by_value(true)
        .await_promise(false)
        .build()
        .map_err(|error| StepError::new(FailureCategory::Protocol, error))?;
    let response = target.execute(params).await.map_err(protocol)?;
    let _ = target.execute(ReleaseObjectParams::new(object_id)).await;
    if let Some(exception) = response.exception_details {
        return Err(StepError::new(
            FailureCategory::Protocol,
            format!("page function threw: {}", exception.text),
        ));
    }
    let value = response.result.value.ok_or_else(|| {
        StepError::new(FailureCategory::Protocol, "page function returned no value")
    })?;
    serde_json::from_value(value)
        .map_err(|error| StepError::new(FailureCategory::Protocol, error.to_string()))
}
