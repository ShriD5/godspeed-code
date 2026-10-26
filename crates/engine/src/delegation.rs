use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zeron_proto::{Chat, Session, SessionStatus};

use crate::EngineError;
use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

/// Most wake-ups a conductor's chats receive per minute. Past it, notices
/// wait in the outbox and go out on the next retries, so a busy tree of
/// sub-agents can't keep its parents in a tight wake loop.
const CONDUCTOR_WAKES_PER_MINUTE: usize = 20;

/// Per-conductor sliding window over the last minute of wake-ups.
#[derive(Default)]
struct WakeBudget {
    recent: HashMap<String, VecDeque<Instant>>,
}

impl WakeBudget {
    /// Spend one wake for `conductor_id`, or `false` when its minute is full.
    fn try_take(&mut self, conductor_id: &str, now: Instant) -> bool {
        let window = self.recent.entry(conductor_id.to_string()).or_default();
        while window
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            window.pop_front();
        }
        if window.len() >= CONDUCTOR_WAKES_PER_MINUTE {
            return false;
        }
        window.push_back(now);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingNotice {
    event_id: String,
    child_chat_id: String,
    parent_chat_id: String,
    text: String,
}

const PENDING: i64 = 0;
const DELIVERED: i64 = 1;
const DROPPED: i64 = 2;

struct NoticeStore {
    connection: Mutex<Connection>,
}

impl NoticeStore {
    fn open(data_dir: &Path) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        let connection =
            Connection::open(data_dir.join("delegation-outbox.sqlite3")).map_err(store_error)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_notices (
                    event_id TEXT PRIMARY KEY,
                    child_chat_id TEXT NOT NULL,
                    parent_chat_id TEXT NOT NULL,
                    text TEXT NOT NULL,
                    state INTEGER NOT NULL DEFAULT 0
                );",
            )
            .map_err(store_error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn enqueue(&self, notice: &PendingNotice) -> Result<(), EngineError> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .execute(
                "INSERT OR IGNORE INTO pending_notices (event_id, child_chat_id, parent_chat_id, text)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    notice.event_id,
                    notice.child_chat_id,
                    notice.parent_chat_id,
                    notice.text
                ],
            )
            .map_err(store_error)?;
        Ok(())
    }

    fn pending(&self) -> Result<Vec<PendingNotice>, EngineError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut statement = connection
            .prepare(
                "SELECT event_id, child_chat_id, parent_chat_id, text
                 FROM pending_notices WHERE state = ?1 ORDER BY rowid",
            )
            .map_err(store_error)?;
        let rows = statement
            .query_map([PENDING], |row| {
                Ok(PendingNotice {
                    event_id: row.get(0)?,
                    child_chat_id: row.get(1)?,
                    parent_chat_id: row.get(2)?,
                    text: row.get(3)?,
                })
            })
            .map_err(store_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
    }

    fn finish(&self, event_id: &str, state: i64) -> Result<(), EngineError> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .execute(
                "UPDATE pending_notices SET state = ?2 WHERE event_id = ?1",
                params![event_id, state],
            )
            .map_err(store_error)?;
        Ok(())
    }

    fn has_pending(&self) -> Result<bool, EngineError> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_notices WHERE state = ?1)",
                [PENDING],
                |row| row.get(0),
            )
            .map_err(store_error)
    }
}

fn store_error(error: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("delegation outbox: {error}"))
}

pub struct DelegationNotifications {
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DelegationNotifications {
    pub fn start(
        data_dir: &Path,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        sessions: SessionsEngine,
        device_id: String,
    ) -> Result<Self, EngineError> {
        let store = Arc::new(NoticeStore::open(data_dir)?);
        let mut statuses = sessions.watch_sessions();
        let worker = tokio::spawn(async move {
            let mut current: HashMap<String, Session> = workspace
                .read_sessions()
                .unwrap_or_default()
                .into_iter()
                .map(|session| (session.chat_id.clone(), session))
                .collect();
            merge_sessions(&mut current, statuses.borrow().clone());
            let mut seen = HashMap::new();
            let mut retry_observation = HashMap::new();
            let initial: Vec<_> = current.values().cloned().collect();
            if let Err(error) = observe_statuses(
                &store,
                &workspace,
                &doc_host,
                &device_id,
                initial,
                &mut seen,
                &mut retry_observation,
            )
            .await
            {
                tracing::warn!(%error, "delegation notification recovery scan failed");
            }
            let mut retry_after = Duration::from_secs(1);
            let mut wakes = WakeBudget::default();
            let _ = retry_pending(
                &store, &workspace, &doc_host, &sessions, &device_id, &current, &mut wakes,
            )
            .await;
            loop {
                let should_retry =
                    store.has_pending().unwrap_or(true) || !retry_observation.is_empty();
                let changed = if should_retry {
                    tokio::select! {
                        changed = statuses.changed() => changed,
                        _ = tokio::time::sleep(retry_after) => {
                            let waiting: Vec<_> = retry_observation.values().cloned().collect();
                            if let Err(error) = observe_statuses(
                                &store, &workspace, &doc_host, &device_id, waiting,
                                &mut seen, &mut retry_observation,
                            ).await {
                                tracing::warn!(%error, "delegation notice retry scan failed");
                            }
                            if let Err(error) = retry_pending(
                                &store, &workspace, &doc_host, &sessions, &device_id, &current,
                                &mut wakes,
                            ).await {
                                tracing::warn!(%error, "delegation outbox retry failed");
                            }
                            retry_after = (retry_after * 2).min(Duration::from_secs(30));
                            continue;
                        }
                    }
                } else {
                    statuses.changed().await
                };
                if changed.is_err() {
                    break;
                }
                let snapshot = statuses.borrow().clone();
                merge_sessions(&mut current, snapshot.clone());
                if let Err(error) = observe_statuses(
                    &store,
                    &workspace,
                    &doc_host,
                    &device_id,
                    snapshot,
                    &mut seen,
                    &mut retry_observation,
                )
                .await
                {
                    tracing::warn!(%error, "delegation notification transition failed");
                }
                if let Err(error) = retry_pending(
                    &store, &workspace, &doc_host, &sessions, &device_id, &current, &mut wakes,
                )
                .await
                {
                    tracing::warn!(%error, "delegation outbox delivery failed");
                }
                retry_after = Duration::from_secs(1);
            }
        });
        Ok(Self {
            worker: Mutex::new(Some(worker)),
        })
    }

    pub async fn shutdown(&self) {
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusSignature {
    status: SessionStatus,
    last_completed_turn: Option<String>,
    awaiting_approval: bool,
    background_tasks: u32,
    running_subagents: u32,
    event_updated_at: Option<i64>,
}

fn status_signature(session: &Session) -> StatusSignature {
    StatusSignature {
        status: session.status,
        last_completed_turn: session.last_completed_turn.clone(),
        awaiting_approval: session.awaiting_approval,
        background_tasks: session.background_tasks,
        running_subagents: session.running_subagents,
        event_updated_at: matches!(
            session.status,
            SessionStatus::AwaitingInput | SessionStatus::Errored
        )
        .then(|| session.updated_at.timestamp_millis()),
    }
}

fn merge_sessions(current: &mut HashMap<String, Session>, sessions: Vec<Session>) {
    for session in sessions {
        current.insert(session.chat_id.clone(), session);
    }
}

async fn observe_statuses(
    store: &NoticeStore,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
    device_id: &str,
    incoming: Vec<Session>,
    seen: &mut HashMap<String, StatusSignature>,
    retry: &mut HashMap<String, Session>,
) -> Result<(), EngineError> {
    for session in incoming {
        if seen.get(&session.chat_id) == Some(&status_signature(&session)) {
            retry.remove(&session.chat_id);
            continue;
        }
        match observe_one(store, workspace, doc_host, device_id, &session).await {
            Ok(true) => {
                seen.insert(session.chat_id.clone(), status_signature(&session));
                retry.remove(&session.chat_id);
            }
            Ok(false) | Err(_) => {
                retry.insert(session.chat_id.clone(), session);
            }
        }
    }
    Ok(())
}

async fn observe_one(
    store: &NoticeStore,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
    device_id: &str,
    session: &Session,
) -> Result<bool, EngineError> {
    if session.device_id != device_id {
        return Ok(true);
    }
    if !has_notifiable_status(session) {
        return Ok(true);
    }
    let child = match workspace.chat(&session.chat_id)? {
        Some(child) if workspace.chat_notifies_parent(&child.id)? => child,
        _ => return Ok(true),
    };
    let Some(parent_id) = child.parent_chat_id.as_deref() else {
        return Ok(true);
    };
    let Some(parent) = workspace.chat(parent_id)? else {
        return Ok(true);
    };
    if child.id == parent.id || !parent_accepts(workspace, &parent, &child, device_id)? {
        return Ok(true);
    }
    let handle = match doc_host.open(&child.id) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::debug!(chat = %child.id, %error, "delegated child transcript is not ready");
            return Ok(false);
        }
    };
    let entries = match handle.doc().read_entries() {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(chat = %child.id, %error, "delegated child transcript read failed");
            return Ok(false);
        }
    };
    if let Some(notice) = notice_for(session, &entries, parent_id) {
        store.enqueue(&notice)?;
    } else if transcript_notice_pending(session, &entries) {
        return Ok(false);
    }
    Ok(true)
}

/// Whether `parent` takes wake-ups from `child`: an eligible parent (a
/// top-level chat, or a conductor member above the depth cap) on this device,
/// hosting the child too. The wake target stays the direct parent.
fn parent_accepts(
    workspace: &WorkspaceHost,
    parent: &Chat,
    child: &Chat,
    device_id: &str,
) -> Result<bool, EngineError> {
    Ok(workspace.chat_may_parent(parent)?
        && child.device_id == parent.device_id
        && child.device_id == device_id)
}

fn has_notifiable_status(session: &Session) -> bool {
    matches!(
        session.status,
        SessionStatus::AwaitingInput | SessionStatus::Errored
    ) || (session.status == SessionStatus::Idle
        && session.last_completed_turn.is_some()
        && session.background_tasks == 0
        && session.running_subagents == 0)
}

fn transcript_notice_pending(session: &Session, entries: &[SessionMessageEntry]) -> bool {
    match session.status {
        SessionStatus::AwaitingInput => match entries.iter().rev().find_map(|entry| {
            entry.parts.iter().rev().find_map(|part| match part {
                MessagePart::Input {
                    resolved,
                    questions,
                    ..
                } => Some((*resolved, questions.is_empty())),
                _ => None,
            })
        }) {
            None => true,
            Some((true, _)) => false,
            Some((false, no_questions)) => no_questions,
        },
        SessionStatus::Errored => entries
            .iter()
            .rev()
            .find(|entry| entry.role == MessageRole::User)
            .is_none(),
        SessionStatus::Idle => session
            .last_completed_turn
            .as_deref()
            .is_some_and(|turn_id| {
                !entries
                    .iter()
                    .any(|entry| entry.id == turn_id && entry.role == MessageRole::Assistant)
            }),
        SessionStatus::Working => false,
    }
}

async fn retry_pending(
    store: &NoticeStore,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
    sessions: &SessionsEngine,
    device_id: &str,
    current: &HashMap<String, Session>,
    wakes: &mut WakeBudget,
) -> Result<(), EngineError> {
    for pending in store.pending()? {
        let Some(session) = current.get(&pending.child_chat_id) else {
            continue;
        };
        let child = match workspace.chat(&pending.child_chat_id)? {
            Some(child)
                if workspace.chat_notifies_parent(&child.id)?
                    && child.parent_chat_id.as_deref() == Some(pending.parent_chat_id.as_str()) =>
            {
                child
            }
            _ => {
                store.finish(&pending.event_id, DROPPED)?;
                continue;
            }
        };
        let Some(parent) = workspace.chat(&pending.parent_chat_id)? else {
            store.finish(&pending.event_id, DROPPED)?;
            continue;
        };
        if !parent_accepts(workspace, &parent, &child, device_id)? {
            store.finish(&pending.event_id, DROPPED)?;
            continue;
        }
        let handle = match doc_host.open(&child.id) {
            Ok(handle) => handle,
            Err(_) => continue,
        };
        let entries = match handle.doc().read_entries() {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let latest = sessions
            .session_status(&child.id)
            .unwrap_or_else(|| session.clone());
        if notice_for(&latest, &entries, &pending.parent_chat_id)
            .is_none_or(|notice| notice.event_id != pending.event_id)
        {
            store.finish(&pending.event_id, DROPPED)?;
            continue;
        }
        if let Some(membership) = workspace.conductor_membership(&parent.id)?
            && !wakes.try_take(&membership.conductor_id, Instant::now())
        {
            continue; // stays pending; the outbox retries it later
        }
        match doc_host
            .queue_delegation_notice(&pending.parent_chat_id, &pending.event_id, &pending.text)
            .await
        {
            Ok(_) => store.finish(&pending.event_id, DELIVERED)?,
            Err(error) => {
                tracing::warn!(event = %pending.event_id, %error, "delegation notice remains pending")
            }
        }
    }
    Ok(())
}

fn notice_for(
    session: &Session,
    entries: &[SessionMessageEntry],
    parent_chat_id: &str,
) -> Option<PendingNotice> {
    let (suffix, text) = match session.status {
        SessionStatus::AwaitingInput => {
            let (request_id, questions, resolved) = entries.iter().rev().find_map(|entry| {
                entry.parts.iter().rev().find_map(|part| match part {
                    MessagePart::Input {
                        request_id,
                        questions,
                        resolved,
                        ..
                    } => Some((request_id.clone(), questions, *resolved)),
                    _ => None,
                })
            })?;
            if resolved {
                return None;
            }
            let question = questions.first()?;
            let title = question
                .approval
                .as_ref()
                .map(|approval| approval.title.clone())
                .unwrap_or_else(|| question.question.clone());
            let is_approval = question.approval.is_some() || session.awaiting_approval;
            let (kind, message) = if is_approval {
                (
                    "approval",
                    format!(
                        "Child chat {} needs approval: {}. Request {} is still pending; open that child chat to review it. No approval was given by this notice.",
                        session.chat_id,
                        concise(&title, 240),
                        request_id
                    ),
                )
            } else {
                (
                    "input",
                    format!(
                        "Child chat {} needs your input: {}. Request {} is still pending; open that child chat to answer it.",
                        session.chat_id,
                        concise(&title, 240),
                        request_id
                    ),
                )
            };
            (format!("{kind}:{request_id}"), message)
        }
        SessionStatus::Errored => {
            let user_index = entries
                .iter()
                .rposition(|entry| entry.role == MessageRole::User)?;
            let user = &entries[user_index];
            let latest = entries[user_index + 1..]
                .iter()
                .rev()
                .find(|entry| entry.role == MessageRole::Assistant);
            let error = latest
                .and_then(|entry| {
                    entry.parts.iter().rev().find_map(|part| match part {
                        MessagePart::Error { message, .. } => Some(message.clone()),
                        _ => None,
                    })
                })
                .unwrap_or_else(|| "The child run failed.".into());
            let identity = latest.map_or_else(|| user.id.clone(), |entry| entry.id.clone());
            (
                format!("failed:{identity}"),
                format!(
                    "Child chat {} failed: {}. Open that child chat for its transcript.",
                    session.chat_id,
                    concise(&error, 400)
                ),
            )
        }
        SessionStatus::Idle
            if session.last_completed_turn.is_some()
                && session.background_tasks == 0
                && session.running_subagents == 0 =>
        {
            let turn_id = session.last_completed_turn.as_deref()?;
            let completed = entries
                .iter()
                .find(|entry| entry.id == turn_id && entry.role == MessageRole::Assistant)?;
            let summary = assistant_text(completed);
            let summary = if summary.is_empty() {
                "The child run completed.".into()
            } else {
                summary
            };
            (
                format!("completed:{turn_id}"),
                format!(
                    "Child chat {} completed: {}. Open that child chat for the full transcript.",
                    session.chat_id,
                    concise(&summary, 400)
                ),
            )
        }
        _ => return None,
    };
    Some(PendingNotice {
        event_id: format!("delegation:{}:{suffix}", session.chat_id),
        child_chat_id: session.chat_id.clone(),
        parent_chat_id: parent_chat_id.to_string(),
        text,
    })
}

fn assistant_text(entry: &SessionMessageEntry) -> String {
    entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn concise(text: &str, limit: usize) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = cleaned.chars();
    let short: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use zeron_doc::MessageStatus;
    use zeron_proto::{ApprovalKind, ApprovalRequest, UserInputQuestion};

    fn session(status: SessionStatus) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: "child-1".into(),
            device_id: "device-1".into(),
            status,
            started_at: None,
            updated_at: Utc.timestamp_millis_opt(42).unwrap(),
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: false,
        }
    }

    fn entry(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 42,
            device_id: "device-1".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
            turn_stats: None,
        }
    }

    fn user_entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: format!("{id}-text"),
                text: text.into(),
            }],
            created_at: 42,
            device_id: "device-1".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
            turn_stats: None,
        }
    }

    #[test]
    fn approval_is_a_distinct_pending_event_then_completion_is_separate() {
        let mut waiting = session(SessionStatus::AwaitingInput);
        waiting.awaiting_approval = true;
        let input = entry(
            "assistant-approval",
            vec![MessagePart::Input {
                id: "input-part".into(),
                request_id: "request-1".into(),
                questions: vec![UserInputQuestion {
                    id: "question-1".into(),
                    header: "Permission".into(),
                    question: "Run make?".into(),
                    options: vec!["Allow".into(), "Deny".into()],
                    multi_select: false,
                    prefill: None,
                    multiline: false,
                    approval: Some(ApprovalRequest::new(ApprovalKind::Command, "Run make")),
                }],
                resolved: false,
            }],
        );
        let approval = notice_for(&waiting, &[input.clone()], "parent-1").unwrap();
        assert!(approval.event_id.ends_with("approval:request-1"));
        assert!(approval.text.contains("No approval was given"));

        let resolved = entry(
            "assistant-approval",
            vec![MessagePart::Input {
                id: "input-part".into(),
                request_id: "request-1".into(),
                questions: vec![],
                resolved: true,
            }],
        );
        assert!(notice_for(&waiting, &[resolved], "parent-1").is_none());

        let mut completed = session(SessionStatus::Idle);
        completed.last_completed_turn = Some("assistant-result".into());
        let result = entry(
            "assistant-result",
            vec![MessagePart::Text {
                id: "text-1".into(),
                text: "Done".into(),
            }],
        );
        let completion = notice_for(&completed, &[result], "parent-1").unwrap();
        assert!(completion.event_id.ends_with("completed:assistant-result"));
        assert_ne!(approval.event_id, completion.event_id);
    }

    #[test]
    fn failed_event_is_stable_and_reports_the_error() {
        let failed = session(SessionStatus::Errored);
        let error = entry(
            "assistant-error",
            vec![MessagePart::Error {
                id: "error-1".into(),
                message: "provider disconnected".into(),
            }],
        );
        let user = user_entry("user-current", "new run");
        let entries = vec![user.clone(), error.clone()];
        let notice = notice_for(&failed, &entries, "parent-1").unwrap();
        assert!(notice.event_id.ends_with("failed:assistant-error"));
        assert!(notice.text.contains("provider disconnected"));
        let mut changed_status = failed.clone();
        changed_status.updated_at += chrono::Duration::seconds(10);
        assert_eq!(
            notice_for(&changed_status, &entries, "parent-1")
                .unwrap()
                .event_id,
            notice.event_id
        );
        let older_error = entry(
            "old-turn",
            vec![MessagePart::Error {
                id: "old-error".into(),
                message: "old failure".into(),
            }],
        );
        let latest = entry(
            "new-turn",
            vec![MessagePart::Text {
                id: "text-2".into(),
                text: "started".into(),
            }],
        );
        let current_failure = notice_for(
            &failed,
            &[
                user_entry("user-old", "old request"),
                older_error,
                user_entry("user-new", "new failed request"),
                latest,
            ],
            "parent-1",
        )
        .unwrap();
        assert!(current_failure.event_id.ends_with("failed:new-turn"));
        assert!(!current_failure.text.contains("old failure"));

        let no_current_assistant = notice_for(
            &failed,
            &[
                user_entry("user-old", "old request"),
                entry(
                    "old-error",
                    vec![MessagePart::Error {
                        id: "old-error-part".into(),
                        message: "old failure".into(),
                    }],
                ),
                user_entry("user-new", "new failed request"),
            ],
            "parent-1",
        )
        .unwrap();
        assert!(no_current_assistant.event_id.ends_with("failed:user-new"));
        assert!(!no_current_assistant.text.contains("old failure"));
    }

    #[test]
    fn event_transcript_races_remain_retryable() {
        let waiting = session(SessionStatus::AwaitingInput);
        assert!(transcript_notice_pending(&waiting, &[]));
        let resolved = entry(
            "assistant-input",
            vec![MessagePart::Input {
                id: "part".into(),
                request_id: "request-1".into(),
                questions: vec![],
                resolved: true,
            }],
        );
        assert!(!transcript_notice_pending(&waiting, &[resolved]));

        let mut completed = session(SessionStatus::Idle);
        completed.last_completed_turn = Some("assistant-result".into());
        assert!(transcript_notice_pending(&completed, &[]));
        let result = entry("assistant-result", vec![]);
        assert!(!transcript_notice_pending(&completed, &[result]));

        let failed = session(SessionStatus::Errored);
        assert!(transcript_notice_pending(&failed, &[]));
    }

    #[test]
    fn outbox_survives_reopen_and_deduplicates_event_keys() {
        let dir = tempfile::tempdir().unwrap();
        let notice = PendingNotice {
            event_id: "delegation:child-1:completed:turn-1".into(),
            child_chat_id: "child-1".into(),
            parent_chat_id: "parent-1".into(),
            text: "Child completed".into(),
        };
        {
            let store = NoticeStore::open(dir.path()).unwrap();
            store.enqueue(&notice).unwrap();
            store.enqueue(&notice).unwrap();
        }
        let store = NoticeStore::open(dir.path()).unwrap();
        assert_eq!(store.pending().unwrap(), vec![notice.clone()]);
        store
            .finish("delegation:child-1:completed:turn-1", DELIVERED)
            .unwrap();
        assert!(store.pending().unwrap().is_empty());
        store.enqueue(&notice).unwrap();
        assert!(store.pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nested_conductor_parents_take_wake_ups_but_plain_children_do_not() {
        use crate::workspace_host::{WorkspaceHost, WorkspaceHostConfig};
        use zeron_proto::CreateConductorParams;
        use zeron_sync::DocsStore;

        let data = tempfile::tempdir().unwrap();
        let ws = WorkspaceHost::open(
            Arc::new(DocsStore::open(data.path()).unwrap()),
            WorkspaceHostConfig {
                device_id: "dev".into(),
                device_name: "Dev".into(),
                platform: "macos".into(),
                org_id: "org".into(),
                user_id: "user".into(),
                edge: None,
                local_only: false,
            },
        )
        .unwrap();
        let folder = tempfile::tempdir().unwrap();
        ws.create_conductor(CreateConductorParams {
            chat_id: "main".into(),
            id: String::new(),
            folder: folder.path().to_string_lossy().into_owned(),
            name: None,
            purpose: None,
            device_id: None,
            config: None,
        })
        .unwrap();
        let mk = |id: &str, parent: Option<&str>| {
            ws.create_chat_with_parent(
                id,
                None,
                Some("dev"),
                None,
                Some("/tmp".into()),
                parent.map(str::to_string),
            )
            .unwrap();
            ws.chat(id).unwrap().unwrap()
        };
        let (mid, leaf) = (mk("mid", Some("main")), mk("leaf", Some("mid")));
        let plain = mk("plain", None);
        let plain_child = mk("plain-child", Some("plain"));
        let plain_grandchild = mk("plain-grandchild", Some("plain-child"));
        // A child of a depth-1 conductor member wakes that direct parent.
        assert!(parent_accepts(&ws, &mid, &leaf, "dev").unwrap());
        // Children of ordinary top-level chats keep working.
        assert!(parent_accepts(&ws, &plain, &plain_child, "dev").unwrap());
        // A child of a non-conductor child still produces no wake-up.
        assert!(!parent_accepts(&ws, &plain_child, &plain_grandchild, "dev").unwrap());
        // Device mismatch still blocks.
        assert!(!parent_accepts(&ws, &mid, &leaf, "other").unwrap());
    }

    #[test]
    fn conductor_wakes_are_capped_per_minute() {
        let mut wakes = WakeBudget::default();
        let start = Instant::now();
        for _ in 0..CONDUCTOR_WAKES_PER_MINUTE {
            assert!(wakes.try_take("c1", start));
        }
        assert!(!wakes.try_take("c1", start + Duration::from_secs(30)));
        // Another conductor has its own budget.
        assert!(wakes.try_take("c2", start));
        // The window slides.
        assert!(wakes.try_take("c1", start + Duration::from_secs(60)));
    }
}
