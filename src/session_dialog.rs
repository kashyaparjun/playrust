//! Native JavaScript dialog state for a persistent browser session.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::page::{
    DialogType, EventJavascriptDialogClosed, EventJavascriptDialogOpening,
    HandleJavaScriptDialogParams,
};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

const DIALOG_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogPolicy {
    Explicit,
    Accept,
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    BeforeUnload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingDialog {
    #[serde(rename = "type")]
    pub kind: DialogKind,
    pub message: String,
    pub url: String,
    #[serde(skip_serializing)]
    pub(crate) frame_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<String>,
    pub has_browser_handler: bool,
    pub opened_at_ms: u64,
    pub opening_revision: u64,
}

impl From<&EventJavascriptDialogOpening> for PendingDialog {
    fn from(event: &EventJavascriptDialogOpening) -> Self {
        Self {
            kind: match &event.r#type {
                DialogType::Alert => DialogKind::Alert,
                DialogType::Confirm => DialogKind::Confirm,
                DialogType::Prompt => DialogKind::Prompt,
                DialogType::Beforeunload => DialogKind::BeforeUnload,
            },
            message: event.message.clone(),
            url: event.url.clone(),
            frame_id: event.frame_id.as_ref().to_owned(),
            default_prompt: event.default_prompt.clone(),
            has_browser_handler: event.has_browser_handler,
            opened_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            opening_revision: 0,
        }
    }
}

struct Pending {
    id: u64,
    metadata: PendingDialog,
    handling: bool,
}

struct Response {
    id: u64,
    accept: bool,
    prompt_text: Option<String>,
}

struct State {
    generation: u64,
    next_dialog_id: u64,
    page: Option<Page>,
    pending: Option<Pending>,
    last_error: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            generation: 0,
            next_dialog_id: 0,
            page: None,
            pending: None,
            last_error: None,
        }
    }

    fn bind(&mut self, page: Page) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.page = Some(page);
        self.pending = None;
        self.last_error = None;
        self.generation
    }

    fn unbind(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.page = None;
        self.pending = None;
    }

    fn unbind_if(&mut self, generation: u64) {
        if generation == self.generation {
            self.unbind();
        }
    }

    fn opened(
        &mut self,
        generation: u64,
        mut dialog: PendingDialog,
        policy: DialogPolicy,
    ) -> Option<Response> {
        if generation != self.generation {
            return None;
        }
        self.next_dialog_id = self.next_dialog_id.wrapping_add(1);
        self.last_error = None;
        let id = self.next_dialog_id;
        dialog.opening_revision = id;
        let prompt_text =
            (policy == DialogPolicy::Accept && dialog.kind == DialogKind::Prompt).then(String::new);
        self.pending = Some(Pending {
            id,
            metadata: dialog,
            handling: policy != DialogPolicy::Explicit,
        });
        match policy {
            DialogPolicy::Explicit => None,
            DialogPolicy::Accept => Some(Response {
                id,
                accept: true,
                prompt_text,
            }),
            DialogPolicy::Dismiss => Some(Response {
                id,
                accept: false,
                prompt_text: None,
            }),
        }
    }

    fn closed(&mut self, generation: u64, frame_id: &str) {
        if generation == self.generation
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.metadata.frame_id == frame_id)
        {
            self.pending = None;
        }
    }

    fn begin_response(
        &mut self,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<(Page, Response)> {
        let page = self
            .page
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native dialog listener is not bound to a page"))?;
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no native dialog is pending"))?;
        if pending.handling {
            bail!("native dialog response is already in progress");
        }
        validate_prompt_text(pending.metadata.kind, accept, prompt_text)?;
        pending.handling = true;
        Ok((
            page,
            Response {
                id: pending.id,
                accept,
                prompt_text: prompt_text.map(str::to_owned),
            },
        ))
    }

    fn complete(&mut self, id: u64, error: Option<String>) {
        let Some(pending) = self.pending.as_mut().filter(|pending| pending.id == id) else {
            return;
        };
        if let Some(error) = error {
            pending.handling = false;
            self.last_error = Some(error);
        } else {
            self.pending = None;
            self.last_error = None;
        }
    }
}

struct Listener {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

pub(crate) struct NativeDialogState {
    policy: DialogPolicy,
    state: Arc<Mutex<State>>,
    pending_notify: Arc<Notify>,
    listener: Option<Listener>,
}

// Invariant: the Mutex<State> is only poisoned if a previous lock holder
// panicked. We guard all lock().expect() sites because native-dialog state
// lives behind Arc<Mutex<State>> shared across the CDP listener and the
// runner; poisoning would make the session permanently unusable.

#[allow(clippy::expect_used)]
fn lock_dialog_state(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().expect("native dialog state poisoned")
}

impl NativeDialogState {
    pub(crate) fn new(policy: DialogPolicy) -> Self {
        Self {
            policy,
            state: Arc::new(Mutex::new(State::new())),
            pending_notify: Arc::new(Notify::new()),
            listener: None,
        }
    }

    pub(crate) async fn bind(&mut self, page: &Page) -> Result<()> {
        self.stop_listener().await;

        let mut opened = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .context("listen for native dialog opening events")?;
        let mut closed = page
            .event_listener::<EventJavascriptDialogClosed>()
            .await
            .context("listen for native dialog closed events")?;
        let generation = lock_dialog_state(&self.state).bind(page.clone());
        let state = Arc::clone(&self.state);
        let pending_notify = Arc::clone(&self.pending_notify);
        let policy = self.policy;
        let task_page = page.clone();
        let (stop, mut stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    event = opened.next() => {
                        let Some(event) = event else { break };
                        let response = lock_dialog_state(&state).opened(generation, PendingDialog::from(event.as_ref()), policy);
                        pending_notify.notify_waiters();
                        if let Some(response) = response {
                            tokio::select! {
                                _ = &mut stop_rx => break,
                                 result = send_response_bounded(&task_page, &response) => {
                                    finish_response(&state, response.id, &result);
                                }
                            }
                        }
                    }
                    event = closed.next() => {
                        let Some(event) = event else { break };
                        lock_dialog_state(&state).closed(generation, event.frame_id.as_ref());
                    }
                }
            }
            lock_dialog_state(&state).unbind_if(generation);
        });
        self.listener = Some(Listener {
            stop: Some(stop),
            task,
        });
        Ok(())
    }

    pub(crate) fn pending(&self) -> Option<PendingDialog> {
        lock_dialog_state(&self.state)
            .pending
            .as_ref()
            .map(|pending| pending.metadata.clone())
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        lock_dialog_state(&self.state).last_error.take()
    }

    pub(crate) async fn wait_for_pending(&self) {
        loop {
            let notified = self.pending_notify.notified();
            if self.pending().is_some() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn accept(&self, prompt_text: Option<&str>) -> Result<()> {
        self.respond(true, prompt_text).await
    }

    pub(crate) async fn dismiss(&self) -> Result<()> {
        self.respond(false, None).await
    }

    pub(crate) async fn shutdown(&mut self) {
        self.stop_listener().await;
    }

    async fn respond(&self, accept: bool, prompt_text: Option<&str>) -> Result<()> {
        let (page, response) =
            lock_dialog_state(&self.state).begin_response(accept, prompt_text)?;
        let result = send_response_bounded(&page, &response).await;
        finish_response(&self.state, response.id, &result);
        result
    }

    async fn stop_listener(&mut self) {
        lock_dialog_state(&self.state).unbind();
        if let Some(mut listener) = self.listener.take() {
            if let Some(stop) = listener.stop.take() {
                let _ = stop.send(());
            }
            let _ = listener.task.await;
        }
    }
}

impl Drop for NativeDialogState {
    fn drop(&mut self) {
        lock_dialog_state(&self.state).unbind();
        if let Some(listener) = self.listener.take() {
            listener.task.abort();
        }
    }
}

async fn send_response(page: &Page, response: &Response) -> Result<()> {
    let mut params = HandleJavaScriptDialogParams::new(response.accept);
    params.prompt_text = response.prompt_text.clone();
    page.execute(params).await.context("handle native dialog")?;
    Ok(())
}

async fn send_response_bounded(page: &Page, response: &Response) -> Result<()> {
    tokio::time::timeout(DIALOG_RESPONSE_TIMEOUT, send_response(page, response))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("native dialog response timed out")))
}

fn finish_response(state: &Mutex<State>, id: u64, result: &Result<()>) {
    lock_dialog_state(state).complete(id, result.as_ref().err().map(ToString::to_string));
}

fn validate_prompt_text(kind: DialogKind, accept: bool, prompt_text: Option<&str>) -> Result<()> {
    if prompt_text.is_some() && (!accept || kind != DialogKind::Prompt) {
        bail!("prompt text is only valid when accepting a prompt dialog");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::browser_protocol::page::FrameId;

    fn dialog(kind: DialogKind) -> PendingDialog {
        PendingDialog {
            kind,
            message: "Continue?".to_owned(),
            url: "https://example.test/".to_owned(),
            frame_id: "frame-1".to_owned(),
            default_prompt: (kind == DialogKind::Prompt).then(|| "default".to_owned()),
            has_browser_handler: true,
            opened_at_ms: 1,
            opening_revision: 0,
        }
    }

    fn bound_state() -> State {
        let mut state = State::new();
        // Pure state tests do not need a Page; begin_response is covered through validation below.
        state.generation = 1;
        state
    }

    #[test]
    fn explicit_policy_keeps_dialog_pending_until_response() {
        let mut state = bound_state();
        assert!(
            state
                .opened(1, dialog(DialogKind::Confirm), DialogPolicy::Explicit)
                .is_none()
        );
        let mut expected = dialog(DialogKind::Confirm);
        expected.opening_revision = 1;
        assert_eq!(
            state.pending.as_ref().map(|pending| &pending.metadata),
            Some(&expected)
        );
        assert!(!state.pending.as_ref().unwrap().handling);
    }

    #[test]
    fn opening_event_preserves_metadata_for_every_dialog_kind() {
        for (cdp_kind, kind) in [
            (DialogType::Alert, DialogKind::Alert),
            (DialogType::Confirm, DialogKind::Confirm),
            (DialogType::Prompt, DialogKind::Prompt),
            (DialogType::Beforeunload, DialogKind::BeforeUnload),
        ] {
            let pending = PendingDialog::from(&EventJavascriptDialogOpening {
                url: "https://example.test/".to_owned(),
                frame_id: FrameId::new("frame-1"),
                message: "Continue?".to_owned(),
                r#type: cdp_kind,
                has_browser_handler: true,
                default_prompt: Some("default".to_owned()),
            });
            assert_eq!(pending.kind, kind);
            assert_eq!(pending.message, "Continue?");
            assert_eq!(pending.url, "https://example.test/");
            assert_eq!(pending.frame_id, "frame-1");
            assert_eq!(pending.default_prompt.as_deref(), Some("default"));
            assert!(pending.has_browser_handler);
            assert!(pending.opened_at_ms > 0);
            assert_eq!(pending.opening_revision, 0);
        }
    }

    #[test]
    fn automatic_policies_create_one_response() {
        let mut state = bound_state();
        let accept = state
            .opened(1, dialog(DialogKind::Prompt), DialogPolicy::Accept)
            .unwrap();
        assert!(accept.accept);
        assert_eq!(accept.prompt_text.as_deref(), Some(""));
        assert!(state.pending.as_ref().unwrap().handling);

        let dismiss = state
            .opened(1, dialog(DialogKind::Alert), DialogPolicy::Dismiss)
            .unwrap();
        assert!(!dismiss.accept);
        assert_eq!(dismiss.prompt_text, None);
    }

    #[test]
    fn completion_clears_success_and_releases_failure_for_retry() {
        let mut state = bound_state();
        let response = state
            .opened(1, dialog(DialogKind::Confirm), DialogPolicy::Accept)
            .unwrap();
        state.complete(response.id, Some("CDP failed".to_owned()));
        assert!(!state.pending.as_ref().unwrap().handling);
        assert_eq!(state.last_error.as_deref(), Some("CDP failed"));

        state.pending.as_mut().unwrap().handling = true;
        state.complete(response.id, None);
        assert!(state.pending.is_none());
    }

    #[test]
    fn closed_event_only_clears_current_generation_and_frame() {
        let mut state = bound_state();
        state.opened(1, dialog(DialogKind::Alert), DialogPolicy::Explicit);
        state.closed(0, "frame-1");
        state.closed(1, "another-frame");
        assert!(state.pending.is_some());
        state.closed(1, "frame-1");
        assert!(state.pending.is_none());
    }

    #[test]
    fn stale_opening_event_cannot_replace_rebound_state() {
        let mut state = bound_state();
        assert!(
            state
                .opened(0, dialog(DialogKind::Alert), DialogPolicy::Accept)
                .is_none()
        );
        assert!(state.pending.is_none());
    }

    #[test]
    fn prompt_text_validation_is_independent_of_cdp() {
        assert!(validate_prompt_text(DialogKind::Prompt, true, Some("answer")).is_ok());
        assert!(validate_prompt_text(DialogKind::Prompt, true, None).is_ok());
        assert!(validate_prompt_text(DialogKind::Confirm, true, Some("answer")).is_err());
        assert!(validate_prompt_text(DialogKind::Prompt, false, Some("answer")).is_err());
    }

    #[tokio::test]
    async fn pending_wait_handles_notifications_before_and_after_polling() {
        let dialog_state = NativeDialogState::new(DialogPolicy::Explicit);
        {
            let mut state = dialog_state.state.lock().unwrap();
            state.generation = 1;
            state.opened(1, dialog(DialogKind::Alert), DialogPolicy::Explicit);
        }
        dialog_state.pending_notify.notify_waiters();
        tokio::time::timeout(Duration::from_millis(10), dialog_state.wait_for_pending())
            .await
            .unwrap();

        let dialog_state = Arc::new(NativeDialogState::new(DialogPolicy::Explicit));
        let waiter = tokio::spawn({
            let dialog_state = Arc::clone(&dialog_state);
            async move { dialog_state.wait_for_pending().await }
        });
        tokio::task::yield_now().await;
        {
            let mut state = dialog_state.state.lock().unwrap();
            state.generation = 1;
            state.opened(1, dialog(DialogKind::Alert), DialogPolicy::Explicit);
        }
        dialog_state.pending_notify.notify_waiters();
        tokio::time::timeout(Duration::from_millis(10), waiter)
            .await
            .unwrap()
            .unwrap();
    }
}
