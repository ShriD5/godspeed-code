//! The engine half of the `preview_*` agent tools: route each action to the
//! desktop window that owns the preview browser, and enforce the policy that
//! must hold whatever the window does.
//!
//! The browser lives in the UI process, and the engine may be embedded in it
//! or a separate daemon, so the window *attaches*: it holds a
//! `WatchPreviewDriver` stream open, receives [`PreviewDriverRequest`]s on it,
//! and answers each with `AnswerPreviewRequest`. The newest attached window
//! serves requests. With none attached (a headless engine) every tool fails
//! with [`PREVIEW_NEEDS_WINDOW`].
//!
//! Policy kept here, not in the window:
//! - the URL policy ([`preview_url`]): http(s) only;
//! - a non-loopback address needs the chat's access mode to allow network:
//!   Auto and Full access pass; Supervised and Auto-accept edits raise a
//!   network approval card on the chat's live turn (approve-for-session
//!   remembers the origin for this chat);
//! - input (click, type, scroll) is rate-limited per chat;
//! - every request has a deadline.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
use zeron_proto::preview_agent::{
    PREVIEW_NEEDS_WINDOW, PreviewAction, PreviewDriverAnswer, PreviewDriverRequest, preview_url,
};
use zeron_proto::{AccessMode, ApprovalDecision, ApprovalKind, ApprovalRequest};

/// Input actions allowed per chat in [`INPUT_WINDOW`].
const INPUT_BURST: usize = 20;
const INPUT_WINDOW: Duration = Duration::from_secs(10);
/// The gap kept between two input actions on one chat.
const INPUT_GAP: Duration = Duration::from_millis(120);

/// How long the window gets for one action.
fn deadline_for(action: &PreviewAction) -> Duration {
    match action {
        PreviewAction::Open { .. } | PreviewAction::Navigate { .. } => Duration::from_secs(30),
        PreviewAction::Wait { .. } => Duration::from_millis(action.wait_timeout_ms()) + Duration::from_secs(5),
        PreviewAction::Screenshot => Duration::from_secs(20),
        _ => Duration::from_secs(15),
    }
}

/// What the network gate needs from the session engine.
#[async_trait]
pub trait PreviewChats: Send + Sync {
    /// The chat's access mode, from its latest run on this device.
    fn access_mode(&self, chat_id: &str) -> Option<AccessMode>;
    /// Raise `request` on the chat's live turn and wait for the answer.
    /// `None` when the chat has no live turn here to ask on.
    async fn ask(&self, chat_id: &str, request: ApprovalRequest) -> Option<ApprovalDecision>;
}

struct Driver {
    id: u64,
    tx: mpsc::UnboundedSender<PreviewDriverRequest>,
}

struct Pending {
    driver: u64,
    reply: oneshot::Sender<Result<serde_json::Value, String>>,
}

#[derive(Default)]
struct State {
    next_driver: u64,
    drivers: Vec<Driver>,
    pending: HashMap<String, Pending>,
    /// Origins a user approved for the rest of a chat's session.
    approved: HashMap<String, HashSet<String>>,
    /// Recent input actions per chat.
    input: HashMap<String, VecDeque<Instant>>,
}

/// Shared by every clone of the engine's RPC service.
#[derive(Clone, Default)]
pub struct PreviewDriverHub {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Detaches its driver when the window's stream is dropped (closed window,
/// lost connection), failing whatever was in flight on it.
struct Attachment {
    hub: PreviewDriverHub,
    driver: u64,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let mut state = lock(&self.hub.state);
        state.drivers.retain(|d| d.id != self.driver);
        let lost: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, p)| p.driver == self.driver)
            .map(|(id, _)| id.clone())
            .collect();
        for id in lost {
            if let Some(pending) = state.pending.remove(&id) {
                let _ = pending
                    .reply
                    .send(Err("The Godspeed window closed before the preview answered".into()));
            }
        }
    }
}

impl PreviewDriverHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a window is attached.
    pub fn attached(&self) -> bool {
        !lock(&self.state).drivers.is_empty()
    }

    /// A window attaches; requests arrive on the stream until it is dropped.
    pub fn attach(&self) -> BoxStream<'static, serde_json::Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        let driver = {
            let mut state = lock(&self.state);
            state.next_driver += 1;
            let id = state.next_driver;
            state.drivers.push(Driver { id, tx });
            id
        };
        let attachment = Attachment {
            hub: self.clone(),
            driver,
        };
        Box::pin(futures::stream::unfold(
            (rx, attachment),
            |(mut rx, attachment)| async move {
                let request = rx.recv().await?;
                let value = serde_json::to_value(&request).ok()?;
                Some((value, (rx, attachment)))
            },
        ))
    }

    /// The window's answer to one request. Unknown ids (late answers after a
    /// timeout) are dropped.
    pub fn answer(&self, answer: PreviewDriverAnswer) {
        let pending = lock(&self.state).pending.remove(&answer.id);
        if let Some(pending) = pending {
            let result = match (answer.ok, answer.err) {
                (_, Some(err)) => Err(err),
                (Some(ok), None) => Ok(ok),
                (None, None) => Ok(serde_json::Value::Null),
            };
            let _ = pending.reply.send(result);
        }
    }

    /// Run one action for `chat_id`: validate, apply the policy, hand it to
    /// the newest attached window, and wait for its answer.
    pub async fn act(
        &self,
        chats: &dyn PreviewChats,
        chat_id: &str,
        action: PreviewAction,
    ) -> Result<serde_json::Value, String> {
        if chat_id.trim().is_empty() {
            return Err("preview tools only work from an agent Godspeed launched in a chat".into());
        }
        action.validate()?;
        // Fail fast before any approval card: nothing could act on it.
        if !self.attached() {
            return Err(PREVIEW_NEEDS_WINDOW.into());
        }
        let action = match action {
            PreviewAction::Open { url } => PreviewAction::Open {
                url: self.network_gate(chats, chat_id, &url).await?,
            },
            PreviewAction::Navigate { url } => PreviewAction::Navigate {
                url: self.network_gate(chats, chat_id, &url).await?,
            },
            other => other,
        };
        if action.is_input() {
            self.pace_input(chat_id).await?;
        }
        self.dispatch(chat_id, action).await
    }

    /// Check `url` and, off loopback, the chat's network access. Returns the
    /// normalized URL.
    async fn network_gate(
        &self,
        chats: &dyn PreviewChats,
        chat_id: &str,
        url: &str,
    ) -> Result<String, String> {
        let checked = preview_url(url)?;
        if checked.local {
            return Ok(checked.url);
        }
        let mode = chats.access_mode(chat_id).unwrap_or(AccessMode::Supervised);
        if matches!(mode, AccessMode::Auto | AccessMode::FullAccess) {
            return Ok(checked.url);
        }
        if lock(&self.state)
            .approved
            .get(chat_id)
            .is_some_and(|origins| origins.contains(&checked.origin))
        {
            return Ok(checked.url);
        }
        let mut request = ApprovalRequest::new(
            ApprovalKind::Network,
            format!("Open {} in the preview", checked.origin),
        )
        .with_decisions(&[
            ApprovalDecision::Approve,
            ApprovalDecision::ApproveForSession,
            ApprovalDecision::Decline,
        ]);
        request.reason = Some(format!(
            "The agent wants to load {} in this chat's preview browser.",
            checked.url
        ));
        let decision = chats.ask(chat_id, request).await.ok_or_else(|| {
            format!(
                "Opening {} needs network approval, and this chat has no running turn to ask in. Local dev servers (localhost) need no approval.",
                checked.origin
            )
        })?;
        if !decision.approves() {
            return Err(format!(
                "The user declined opening {} in the preview",
                checked.origin
            ));
        }
        if decision == ApprovalDecision::ApproveForSession {
            lock(&self.state)
                .approved
                .entry(chat_id.to_owned())
                .or_default()
                .insert(checked.origin);
        }
        Ok(checked.url)
    }

    /// Keep [`INPUT_GAP`] between input actions and refuse a burst beyond
    /// [`INPUT_BURST`] in [`INPUT_WINDOW`].
    async fn pace_input(&self, chat_id: &str) -> Result<(), String> {
        let wait = {
            let mut state = lock(&self.state);
            let now = Instant::now();
            let recent = state.input.entry(chat_id.to_owned()).or_default();
            while recent
                .front()
                .is_some_and(|at| now.duration_since(*at) > INPUT_WINDOW)
            {
                recent.pop_front();
            }
            if recent.len() >= INPUT_BURST {
                return Err(format!(
                    "Too many clicks and keystrokes: at most {INPUT_BURST} every {} seconds. Take a snapshot and check the page before acting again.",
                    INPUT_WINDOW.as_secs()
                ));
            }
            let next = recent
                .back()
                .map(|last| *last + INPUT_GAP)
                .filter(|next| *next > now)
                .unwrap_or(now);
            recent.push_back(next);
            next.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(())
    }

    async fn dispatch(
        &self,
        chat_id: &str,
        action: PreviewAction,
    ) -> Result<serde_json::Value, String> {
        let deadline = deadline_for(&action);
        let tool = action.tool();
        let id = crate::new_id();
        let (reply, answer) = oneshot::channel();
        {
            let mut state = lock(&self.state);
            // The newest window wins; a dead sender is pruned on the way.
            loop {
                let Some(driver) = state.drivers.last() else {
                    return Err(PREVIEW_NEEDS_WINDOW.into());
                };
                let request = PreviewDriverRequest {
                    id: id.clone(),
                    chat_id: chat_id.to_owned(),
                    action: action.clone(),
                };
                let driver_id = driver.id;
                if driver.tx.send(request).is_ok() {
                    state.pending.insert(
                        id.clone(),
                        Pending {
                            driver: driver_id,
                            reply,
                        },
                    );
                    break;
                }
                state.drivers.pop();
            }
        }
        match tokio::time::timeout(deadline, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PREVIEW_NEEDS_WINDOW.into()),
            Err(_) => {
                lock(&self.state).pending.remove(&id);
                Err(format!(
                    "{tool} timed out after {}s waiting for the preview",
                    deadline.as_secs()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    struct Chats {
        mode: Option<AccessMode>,
        decision: Option<ApprovalDecision>,
        asked: Mutex<Vec<ApprovalRequest>>,
    }

    impl Chats {
        fn new(mode: AccessMode, decision: Option<ApprovalDecision>) -> Self {
            Self {
                mode: Some(mode),
                decision,
                asked: Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl PreviewChats for Chats {
        fn access_mode(&self, _: &str) -> Option<AccessMode> {
            self.mode
        }
        async fn ask(&self, _: &str, request: ApprovalRequest) -> Option<ApprovalDecision> {
            self.asked.lock().unwrap().push(request);
            self.decision
        }
    }

    /// A fake window: answers every request with what `respond` returns
    /// (`None`: never answers).
    fn fake_ui(
        hub: &PreviewDriverHub,
        respond: impl Fn(&PreviewDriverRequest) -> Option<PreviewDriverAnswer> + Send + 'static,
    ) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Vec<PreviewDriverRequest>>>) {
        let mut stream = hub.attach();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let hub = hub.clone();
        let task = tokio::spawn(async move {
            while let Some(value) = stream.next().await {
                let request: PreviewDriverRequest = serde_json::from_value(value).unwrap();
                log.lock().unwrap().push(request.clone());
                if let Some(answer) = respond(&request) {
                    hub.answer(answer);
                }
            }
        });
        (task, seen)
    }

    fn ok(request: &PreviewDriverRequest, value: serde_json::Value) -> Option<PreviewDriverAnswer> {
        Some(PreviewDriverAnswer {
            id: request.id.clone(),
            ok: Some(value),
            err: None,
        })
    }

    #[tokio::test]
    async fn no_window_means_a_clear_error() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::FullAccess, None);
        let err = hub
            .act(&chats, "chat-1", PreviewAction::Snapshot)
            .await
            .unwrap_err();
        assert_eq!(err, PREVIEW_NEEDS_WINDOW);
        // A detached window is the same as none.
        let (task, _) = fake_ui(&hub, |r| ok(r, json!({})));
        assert!(hub.attached());
        task.abort();
        let _ = task.await;
        assert!(!hub.attached());
        assert_eq!(
            hub.act(&chats, "chat-1", PreviewAction::Snapshot)
                .await
                .unwrap_err(),
            PREVIEW_NEEDS_WINDOW
        );
    }

    #[tokio::test]
    async fn requests_reach_the_window_with_the_chat_and_its_answer_returns() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::Supervised, None);
        let (_task, seen) = fake_ui(&hub, |r| match &r.action {
            PreviewAction::Snapshot => ok(r, json!({"title": "Home"})),
            _ => Some(PreviewDriverAnswer {
                id: r.id.clone(),
                ok: None,
                err: Some("no such element".into()),
            }),
        });
        let value = hub
            .act(&chats, "chat-1", PreviewAction::Snapshot)
            .await
            .unwrap();
        assert_eq!(value["title"], "Home");
        let err = hub
            .act(
                &chats,
                "chat-1",
                PreviewAction::Click {
                    reference: Some("e3".into()),
                    selector: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err, "no such element");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|r| r.chat_id == "chat-1"));
    }

    #[tokio::test]
    async fn invalid_actions_never_reach_the_window() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::FullAccess, None);
        let (_task, seen) = fake_ui(&hub, |r| ok(r, json!({})));
        for action in [
            PreviewAction::Open {
                url: "file:///etc/passwd".into(),
            },
            PreviewAction::Click {
                reference: None,
                selector: None,
            },
            PreviewAction::Eval {
                expression: "document.cookie".into(),
            },
        ] {
            assert!(hub.act(&chats, "chat-1", action).await.is_err());
        }
        assert!(hub.act(&chats, "", PreviewAction::Snapshot).await.is_err());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_window_times_out() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::FullAccess, None);
        let (_task, _) = fake_ui(&hub, |_| None);
        let err = hub
            .act(&chats, "chat-1", PreviewAction::Snapshot)
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(lock(&hub.state).pending.is_empty());
    }

    #[tokio::test]
    async fn closing_the_window_fails_in_flight_requests() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::FullAccess, None);
        let (task, seen) = fake_ui(&hub, |_| None);
        let act = {
            let hub = hub.clone();
            tokio::spawn(async move {
                let chats = Chats::new(AccessMode::FullAccess, None);
                hub.act(&chats, "chat-1", PreviewAction::Screenshot).await
            })
        };
        while seen.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        task.abort();
        let err = act.await.unwrap().unwrap_err();
        assert!(err.contains("window closed"), "{err}");
        drop(chats);
    }

    #[tokio::test]
    async fn the_newest_window_serves() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::FullAccess, None);
        let (_old, old_seen) = fake_ui(&hub, |r| ok(r, json!("old")));
        let (_new, new_seen) = fake_ui(&hub, |r| ok(r, json!("new")));
        let value = hub
            .act(&chats, "chat-1", PreviewAction::Console { clear: false })
            .await
            .unwrap();
        assert_eq!(value, json!("new"));
        assert!(old_seen.lock().unwrap().is_empty());
        assert_eq!(new_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn localhost_needs_no_approval_and_the_web_follows_the_access_mode() {
        let hub = PreviewDriverHub::new();
        let (_task, seen) = fake_ui(&hub, |r| ok(r, json!({"url": r.action.url()})));
        let open = |url: &str| PreviewAction::Open { url: url.into() };

        // Loopback: never asks, normalized.
        let supervised = Chats::new(AccessMode::Supervised, Some(ApprovalDecision::Decline));
        let value = hub
            .act(&supervised, "chat-1", open("localhost:5173"))
            .await
            .unwrap();
        assert_eq!(value["url"], "http://localhost:5173/");
        assert_eq!(supervised.asked(), 0);

        // Supervised: a network card; declining refuses.
        let err = hub
            .act(&supervised, "chat-1", open("https://example.com"))
            .await
            .unwrap_err();
        assert!(err.contains("declined"), "{err}");
        let card = supervised.asked.lock().unwrap()[0].clone();
        assert_eq!(card.kind, ApprovalKind::Network);
        assert!(card.title.contains("https://example.com"));

        // Auto-accept edits asks too; approve-for-session remembers the origin.
        let edits = Chats::new(
            AccessMode::AutoAcceptEdits,
            Some(ApprovalDecision::ApproveForSession),
        );
        hub.act(&edits, "chat-2", open("https://example.com/a"))
            .await
            .unwrap();
        hub.act(
            &edits,
            "chat-2",
            PreviewAction::Navigate {
                url: "https://example.com/b".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(edits.asked(), 1, "the second load reused the approval");
        // ...for that chat only.
        let other = Chats::new(AccessMode::AutoAcceptEdits, Some(ApprovalDecision::Approve));
        hub.act(&other, "chat-3", open("https://example.com"))
            .await
            .unwrap();
        assert_eq!(other.asked(), 1);

        // Auto and Full access load the web without asking.
        for mode in [AccessMode::Auto, AccessMode::FullAccess] {
            let chats = Chats::new(mode, None);
            hub.act(&chats, "chat-4", open("https://example.org"))
                .await
                .unwrap();
            assert_eq!(chats.asked(), 0);
        }

        // No live turn to ask on: refused with the reason.
        let idle = Chats::new(AccessMode::Supervised, None);
        let err = hub
            .act(&idle, "chat-5", open("https://example.net"))
            .await
            .unwrap_err();
        assert!(err.contains("no running turn"), "{err}");
        let seen = seen.lock().unwrap();
        assert!(seen.iter().all(|r| r.chat_id != "chat-5"));
        // The declined load never reached the window.
        assert!(
            !seen
                .iter()
                .any(|r| r.chat_id == "chat-1" && r.action.url() == Some("https://example.com/"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn input_is_paced_and_bursts_are_refused() {
        let hub = PreviewDriverHub::new();
        let chats = Chats::new(AccessMode::FullAccess, None);
        let (_task, _) = fake_ui(&hub, |r| ok(r, json!({})));
        let click = || PreviewAction::Click {
            reference: None,
            selector: Some("button".into()),
        };
        let start = tokio::time::Instant::now();
        for _ in 0..INPUT_BURST {
            hub.act(&chats, "chat-1", click()).await.unwrap();
        }
        assert!(
            start.elapsed() >= INPUT_GAP * (INPUT_BURST as u32 - 1),
            "clicks were not spaced"
        );
        let err = hub.act(&chats, "chat-1", click()).await.unwrap_err();
        assert!(err.contains("Too many"), "{err}");
        // Reads are never limited, and other chats have their own budget.
        hub.act(&chats, "chat-1", PreviewAction::Snapshot)
            .await
            .unwrap();
        hub.act(&chats, "chat-2", click()).await.unwrap();
    }
}
