use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::Message;
use chromiumoxide::{Command, Page};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::locator::POLL_INTERVAL;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Sessions {
    by_target: HashMap<String, String>,
    by_session: HashMap<String, String>,
    execution_contexts: HashMap<(String, String), i64>,
    failed: Option<String>,
}

enum Request {
    Command {
        target: String,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value>>,
    },
    Close(oneshot::Sender<()>),
}

enum Pending {
    Attach(String),
    Command(oneshot::Sender<Result<Value>>),
    Ignore,
}

pub struct OopifRouter {
    requests: mpsc::Sender<Request>,
    sessions: Arc<RwLock<Sessions>>,
    changed: Arc<Notify>,
}

impl OopifRouter {
    pub async fn connect(websocket: &str, browser_context: &str) -> Result<Arc<Self>> {
        let (socket, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            async_tungstenite::tokio::connect_async(websocket),
        )
        .await
        .map_err(|_| anyhow!("connect OOPIF CDP router timed out"))?
        .context("connect OOPIF CDP router")?;
        let (requests, receiver) = mpsc::channel(32);
        let sessions = Arc::new(RwLock::new(Sessions::default()));
        let changed = Arc::new(Notify::new());
        tokio::spawn(run(
            socket,
            receiver,
            browser_context.to_owned(),
            Arc::clone(&sessions),
            Arc::clone(&changed),
        ));
        Ok(Arc::new(Self {
            requests,
            sessions,
            changed,
        }))
    }

    pub async fn wait_for_target(&self, target: &str, deadline: Instant) -> Result<()> {
        loop {
            let notified = self.changed.notified();
            {
                let sessions = self.sessions.read().expect("OOPIF session state poisoned");
                if sessions.by_target.contains_key(target) {
                    return Ok(());
                }
                if let Some(error) = &sessions.failed {
                    bail!("OOPIF CDP router failed: {error}");
                }
            }
            if Instant::now() >= deadline {
                bail!("OOPIF target did not attach before the step deadline");
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))) => {}
            }
        }
    }

    pub fn has_target(&self, target: &str) -> bool {
        self.sessions
            .read()
            .expect("OOPIF session state poisoned")
            .by_target
            .contains_key(target)
    }

    async fn command(&self, target: &str, method: &str, mut params: Value) -> Result<Value> {
        if (method == "DOM.resolveNode"
            || method == "Runtime.callFunctionOn"
                && params.get("objectId").is_none()
                && params.get("executionContextId").is_none())
            && let Some(context) = self
                .sessions
                .read()
                .expect("OOPIF session state poisoned")
                .execution_contexts
                .get(&(target.to_owned(), target.to_owned()))
        {
            params["executionContextId"] = (*context).into();
        }
        let (reply, response) = oneshot::channel();
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            self.requests
                .send(Request::Command {
                    target: target.to_owned(),
                    method: method.to_owned(),
                    params,
                    reply,
                })
                .await
                .map_err(|_| anyhow!("OOPIF CDP router stopped"))?;
            response
                .await
                .map_err(|_| anyhow!("OOPIF CDP router stopped"))?
        })
        .await
        .map_err(|_| anyhow!("OOPIF CDP command {method} timed out"))?
    }

    pub async fn close(&self) -> Result<()> {
        let (reply, closed) = oneshot::channel();
        tokio::time::timeout(CLOSE_TIMEOUT, async {
            self.requests
                .send(Request::Close(reply))
                .await
                .map_err(|_| anyhow!("OOPIF CDP router stopped"))?;
            closed
                .await
                .map_err(|_| anyhow!("OOPIF CDP router stopped"))
        })
        .await
        .map_err(|_| anyhow!("close OOPIF CDP router timed out"))??;
        Ok(())
    }

    pub fn execution_context(&self, target: &str, frame: &str) -> Option<i64> {
        self.sessions
            .read()
            .expect("OOPIF session state poisoned")
            .execution_contexts
            .get(&(target.to_owned(), frame.to_owned()))
            .copied()
    }
}

#[derive(Clone, Copy)]
pub enum CdpTarget<'a> {
    Root(&'a Page),
    Oopif(&'a OopifRouter, &'a str),
}

impl<'a> CdpTarget<'a> {
    pub async fn execute<C>(&self, command: C) -> Result<C::Response>
    where
        C: Command,
    {
        match self {
            Self::Root(page) => page
                .execute(command)
                .await
                .map(|response| response.result)
                .map_err(anyhow::Error::from),
            Self::Oopif(router, target) => {
                let method = command.identifier();
                let result = router
                    .command(
                        target,
                        method.as_ref(),
                        serde_json::to_value(command).context("serialize CDP command")?,
                    )
                    .await?;
                C::response_from_value(result).context("decode CDP response")
            }
        }
    }

    pub fn execution_context(&self, frame: &str) -> Option<i64> {
        match self {
            Self::Oopif(router, target) => router.execution_context(target, frame),
            Self::Root(_) => None,
        }
    }
}

async fn run<S>(
    mut socket: S,
    mut requests: mpsc::Receiver<Request>,
    browser_context: String,
    sessions: Arc<RwLock<Sessions>>,
    changed: Arc<Notify>,
) where
    S: StreamExt<Item = Result<Message, async_tungstenite::tungstenite::Error>>
        + SinkExt<Message, Error = async_tungstenite::tungstenite::Error>
        + Unpin,
{
    let mut next_id = 1_u64;
    let mut pending = HashMap::new();
    let mut attaching = HashSet::new();
    if send(
        &mut socket,
        &mut next_id,
        &mut pending,
        "Target.setDiscoverTargets",
        json!({"discover": true}),
        None,
        Pending::Ignore,
    )
    .await
    .is_err()
        || send(
            &mut socket,
            &mut next_id,
            &mut pending,
            "Target.getTargets",
            json!({}),
            None,
            Pending::Ignore,
        )
        .await
        .is_err()
    {
        fail(&sessions, &changed, "initialize OOPIF CDP discovery");
        return;
    }

    loop {
        tokio::select! {
            request = requests.recv() => match request {
                Some(Request::Command { target, method, params, reply }) => {
                    let session = sessions.read().expect("OOPIF session state poisoned")
                        .by_target.get(&target).cloned();
                    let Some(session) = session else {
                        let _ = reply.send(Err(anyhow!("active OOPIF target is detached")));
                        continue;
                    };
                    if send(
                        &mut socket,
                        &mut next_id,
                        &mut pending,
                        "Runtime.runIfWaitingForDebugger",
                        json!({}),
                        Some(&session),
                        Pending::Ignore,
                    ).await.is_err() {
                        fail(&sessions, &changed, "resume OOPIF target");
                        return;
                    }
                    if let Err(error) = send(&mut socket, &mut next_id, &mut pending, &method, params, Some(&session), Pending::Command(reply)).await {
                        fail(&sessions, &changed, &error.to_string());
                        return;
                    }
                }
                Some(Request::Close(reply)) => {
                    let _ = tokio::time::timeout(IO_TIMEOUT, socket.close()).await;
                    let _ = reply.send(());
                    return;
                }
                None => return,
            },
            message = socket.next() => {
                let Some(message) = message else {
                    fail(&sessions, &changed, "OOPIF CDP connection closed");
                    return;
                };
                let message = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Ping(data)) => {
                        if !matches!(
                            tokio::time::timeout(IO_TIMEOUT, socket.send(Message::Pong(data))).await,
                            Ok(Ok(()))
                        ) {
                            fail(&sessions, &changed, "OOPIF CDP connection failed");
                            return;
                        }
                        continue;
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        fail(&sessions, &changed, "OOPIF CDP connection closed");
                        return;
                    }
                    _ => continue,
                };
                let Ok(message): Result<Value, _> = serde_json::from_str(message.as_ref()) else { continue };
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let action = pending.remove(&id);
                    if matches!(action, Some(Pending::Ignore))
                        && let Some(infos) = message.pointer("/result/targetInfos").and_then(Value::as_array)
                    {
                        for info in infos {
                            attach_if_oopif(info, &browser_context, &mut socket, &mut next_id, &mut pending, &mut attaching).await;
                        }
                    }
                    match action {
                        Some(Pending::Attach(target)) => {
                            attaching.remove(&target);
                            if let Some(session) = message.pointer("/result/sessionId").and_then(Value::as_str).map(str::to_owned) {
                                {
                                    let mut state = sessions.write().expect("OOPIF session state poisoned");
                                    if let Some(previous) = state.by_target.insert(target.clone(), session.clone()) {
                                        state.by_session.remove(&previous);
                                    }
                                    state.by_session.insert(session.clone(), target);
                                }
                                changed.notify_waiters();
                                let _ = send(
                                    &mut socket,
                                    &mut next_id,
                                    &mut pending,
                                    "Runtime.runIfWaitingForDebugger",
                                    json!({}),
                                    Some(&session),
                                    Pending::Ignore,
                                ).await;
                                for method in [
                                    "Page.enable",
                                    "DOM.enable",
                                    "Runtime.enable",
                                    "Accessibility.enable",
                                ] {
                                    let _ = send(
                                        &mut socket,
                                        &mut next_id,
                                        &mut pending,
                                        method,
                                        json!({}),
                                        Some(&session),
                                        Pending::Ignore,
                                    ).await;
                                }
                            }
                        }
                        Some(Pending::Command(reply)) => {
                            let result = if let Some(error) = message.get("error") {
                                Err(anyhow!(error.get("message").and_then(Value::as_str).unwrap_or("CDP command failed").to_owned()))
                            } else {
                                Ok(message.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = reply.send(result);
                        }
                        _ => {}
                    }
                    continue;
                }
                match message.get("method").and_then(Value::as_str) {
                    Some("Runtime.executionContextCreated")
                        if message.pointer("/params/context/auxData/isDefault").and_then(Value::as_bool) == Some(true) =>
                    {
                        if let (Some(session), Some(context)) = (
                            message.get("sessionId").and_then(Value::as_str),
                            message.pointer("/params/context/id").and_then(Value::as_i64),
                        ) {
                            let mut state = sessions.write().expect("OOPIF session state poisoned");
                            if let (Some(target), Some(frame)) = (
                                state.by_session.get(session).cloned(),
                                message.pointer("/params/context/auxData/frameId").and_then(Value::as_str),
                            )
                            {
                                state.execution_contexts.insert((target, frame.to_owned()), context);
                            }
                        }
                    }
                    Some("Runtime.executionContextsCleared") => {
                        if let Some(session) = message.get("sessionId").and_then(Value::as_str) {
                            let mut state = sessions.write().expect("OOPIF session state poisoned");
                            if let Some(target) = state.by_session.get(session).cloned() {
                                state.execution_contexts.retain(|(endpoint, _), _| endpoint != &target);
                            }
                        }
                    }
                    Some("Page.frameNavigated") => {
                        if let Some(session) = message.get("sessionId").and_then(Value::as_str) {
                            for method in ["DOM.disable", "DOM.enable"] {
                                let _ = send(
                                    &mut socket,
                                    &mut next_id,
                                    &mut pending,
                                    method,
                                    json!({}),
                                    Some(session),
                                    Pending::Ignore,
                                ).await;
                            }
                        }
                    }
                    Some("Target.targetCreated") => {
                        if let Some(info) = message.pointer("/params/targetInfo") {
                            attach_if_oopif(info, &browser_context, &mut socket, &mut next_id, &mut pending, &mut attaching).await;
                        }
                    }
                    Some("Target.attachedToTarget") => {
                        if message.pointer("/params/targetInfo/type").and_then(Value::as_str) != Some("iframe")
                            || message.pointer("/params/targetInfo/browserContextId").and_then(Value::as_str) != Some(&browser_context)
                        {
                            continue;
                        }
                        if let (Some(target), Some(session)) = (
                            message.pointer("/params/targetInfo/targetId").and_then(Value::as_str),
                            message.pointer("/params/sessionId").and_then(Value::as_str),
                        ) {
                            {
                                let mut state = sessions.write().expect("OOPIF session state poisoned");
                                if let Some(previous) = state.by_target.insert(target.to_owned(), session.to_owned()) {
                                    state.by_session.remove(&previous);
                                }
                                state.by_session.insert(session.to_owned(), target.to_owned());
                            }
                            changed.notify_waiters();
                            for method in [
                                "Runtime.runIfWaitingForDebugger",
                                "Page.enable",
                                "DOM.enable",
                                "Runtime.enable",
                                "Accessibility.enable",
                            ] {
                                let _ = send(
                                    &mut socket,
                                    &mut next_id,
                                    &mut pending,
                                    method,
                                    json!({}),
                                    Some(session),
                                    Pending::Ignore,
                                ).await;
                            }
                        }
                    }
                    Some("Target.detachedFromTarget") => {
                        if let Some(session) = message.pointer("/params/sessionId").and_then(Value::as_str) {
                            let mut state = sessions.write().expect("OOPIF session state poisoned");
                            if let Some(target) = state.by_session.remove(session)
                                && state
                                    .by_target
                                    .get(&target)
                                    .is_some_and(|current| current == session)
                            {
                                    state.by_target.remove(&target);
                                    state.execution_contexts.retain(|(endpoint, _), _| endpoint != &target);
                            }
                            drop(state);
                            changed.notify_waiters();
                        }
                    }
                    Some("Target.targetDestroyed") => {
                        if let Some(target) = message.pointer("/params/targetId").and_then(Value::as_str) {
                            let mut state = sessions.write().expect("OOPIF session state poisoned");
                            if let Some(session) = state.by_target.remove(target) {
                                state.by_session.remove(&session);
                            }
                            state.execution_contexts.retain(|(endpoint, _), _| endpoint != target);
                            drop(state);
                            changed.notify_waiters();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn attach_if_oopif<S>(
    info: &Value,
    browser_context: &str,
    socket: &mut S,
    next_id: &mut u64,
    pending: &mut HashMap<u64, Pending>,
    attaching: &mut HashSet<String>,
) where
    S: SinkExt<Message, Error = async_tungstenite::tungstenite::Error> + Unpin,
{
    if info.get("type").and_then(Value::as_str) != Some("iframe")
        || info.get("browserContextId").and_then(Value::as_str) != Some(browser_context)
    {
        return;
    }
    let Some(target) = info.get("targetId").and_then(Value::as_str) else {
        return;
    };
    if !attaching.insert(target.to_owned()) {
        return;
    }
    let _ = send(
        socket,
        next_id,
        pending,
        "Target.attachToTarget",
        json!({"targetId": target, "flatten": true}),
        None,
        Pending::Attach(target.to_owned()),
    )
    .await;
}

async fn send<S>(
    socket: &mut S,
    next_id: &mut u64,
    pending: &mut HashMap<u64, Pending>,
    method: &str,
    params: Value,
    session: Option<&str>,
    action: Pending,
) -> Result<()>
where
    S: SinkExt<Message, Error = async_tungstenite::tungstenite::Error> + Unpin,
{
    let id = *next_id;
    *next_id = next_id.wrapping_add(1);
    pending.insert(id, action);
    let mut message = json!({"id": id, "method": method, "params": params});
    if let Some(session) = session {
        message["sessionId"] = Value::String(session.to_owned());
    }
    tokio::time::timeout(
        IO_TIMEOUT,
        socket.send(Message::Text(message.to_string().into())),
    )
    .await
    .map_err(|_| anyhow!("send OOPIF CDP command {method} timed out"))?
    .context("send OOPIF CDP command")
}

fn fail(sessions: &RwLock<Sessions>, changed: &Notify, error: &str) {
    sessions
        .write()
        .expect("OOPIF session state poisoned")
        .failed = Some(error.to_owned());
    changed.notify_waiters();
}
