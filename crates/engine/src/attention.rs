//! The "Needs you" inbox: one ordered stream of everything waiting on the
//! user across the chats and always-on agent runs this engine hosts —
//! approvals and questions in live turns, failed, limited and unseen
//! finished chats, unread agent runs, and questions that timed out.
//!
//! - **Derived, not stored.** Every refresh re-reads registry rows, live
//!   session statuses, the open input requests in the waiting chats' docs
//!   and the agent run inbox ([`derive`]). The only state of its own is what
//!   nothing else records: when a request was first seen waiting, and the
//!   questions it timed out (kept until their chat is seen).
//! - **Answers go the normal way.** Approve and decline queue `RespondInput`
//!   through the command ledger, exactly like the composer; a batch is one
//!   such answer per item, and only items that are low-risk approvals at
//!   the moment of answering are touched.
//! - **Question timeout.** Off by default; a device default plus per-chat
//!   overrides. A plain question still unanswered after its timeout gets a
//!   short "use your best judgement or stop" answer and stays in the queue
//!   as timed out. Approvals never time out and are never approved here
//!   without an explicit request.

mod derive;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_doc::SessionCommandPayload;
use zeron_proto::agents::AgentRunFilter;
use zeron_proto::attention::{
    AttentionItem, AttentionKind, AttentionSnapshot, BatchApproval, ChatQuestionTimeout,
    QUESTION_TIMEOUT_ANSWER, QuestionTimeoutSettings,
};
use zeron_proto::{ApprovalDecision, SessionStatus, UserInputAnswer};

use crate::EngineError;
use crate::agents::{Agents, Clock, SystemClock};
use crate::device_settings;
use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

use derive::{PendingInput, Sources, TimedOutQuestion};

const SETTINGS_FILE: &str = "question-timeout.json";
const TIMED_OUT_FILE: &str = "attention-timed-out.json";
/// Longest the refresh loop trusts one sleep; a timeout due meanwhile is
/// answered at most this late (tokio timers stop while a laptop sleeps).
const MAX_SLEEP: Duration = Duration::from_secs(60);
/// Status and registry updates arrive in bursts while turns stream.
const COALESCE: Duration = Duration::from_millis(100);
const RECENT_RUNS: u32 = 200;

#[derive(Clone)]
pub struct AttentionService {
    inner: Arc<Inner>,
}

struct Inner {
    device_id: String,
    workspace: WorkspaceHost,
    doc_host: DocHost,
    sessions: SessionsEngine,
    agents: Agents,
    clock: Arc<dyn Clock>,
    settings_path: PathBuf,
    settings: Mutex<QuestionTimeoutSettings>,
    timed_out_path: PathBuf,
    timed_out: Mutex<Vec<TimedOutQuestion>>,
    /// `(chat, request)` → when it was first seen waiting.
    first_seen: Mutex<HashMap<(String, String), DateTime<Utc>>>,
    /// Requests answered here whose answer the host has not applied yet:
    /// hidden so a second click or tick can't answer them twice.
    answered: Mutex<HashSet<(String, String)>>,
    /// When the next waiting question times out, as of the last refresh.
    next_timeout: Mutex<Option<DateTime<Utc>>>,
    snapshot: watch::Sender<AttentionSnapshot>,
    /// One refresh at a time: they answer timeouts.
    refreshing: Mutex<()>,
    wake: Notify,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

/// The engine services the inbox reads and answers through.
#[derive(Clone)]
pub struct AttentionDeps {
    pub workspace: WorkspaceHost,
    pub doc_host: DocHost,
    pub sessions: SessionsEngine,
    pub agents: Agents,
}

impl AttentionService {
    pub fn open(data_dir: &Path, deps: AttentionDeps) -> Self {
        Self::open_with_clock(data_dir, deps, Arc::new(SystemClock))
    }

    pub fn open_with_clock(data_dir: &Path, deps: AttentionDeps, clock: Arc<dyn Clock>) -> Self {
        let settings_path = data_dir.join(SETTINGS_FILE);
        let timed_out_path = data_dir.join(TIMED_OUT_FILE);
        let device_id = deps.workspace.device_id().to_string();
        Self {
            inner: Arc::new(Inner {
                snapshot: watch::channel(AttentionSnapshot {
                    device_id: device_id.clone(),
                    items: Vec::new(),
                })
                .0,
                device_id,
                workspace: deps.workspace,
                doc_host: deps.doc_host,
                sessions: deps.sessions,
                agents: deps.agents,
                clock,
                settings: Mutex::new(device_settings::load(&settings_path)),
                settings_path,
                timed_out: Mutex::new(device_settings::load(&timed_out_path)),
                timed_out_path,
                first_seen: Mutex::new(HashMap::new()),
                answered: Mutex::new(HashSet::new()),
                next_timeout: Mutex::new(None),
                refreshing: Mutex::new(()),
                wake: Notify::new(),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// Start following state changes. Needs a tokio runtime.
    pub fn start(&self) {
        let inner = self.inner.clone();
        self.inner
            .tasks
            .spawn(async move { inner.run_loop().await });
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.tasks.wait()).await;
    }

    /// The current queue, then one per change (`WatchAttention`).
    pub fn watch(&self) -> watch::Receiver<AttentionSnapshot> {
        self.inner.snapshot.subscribe()
    }

    /// A freshly derived queue (`GetAttention`).
    pub fn snapshot(&self) -> AttentionSnapshot {
        self.inner.refresh(self.inner.clock.now())
    }

    /// Answer one approval with `decision` through the command ledger.
    pub fn respond(&self, item_id: &str, decision: ApprovalDecision) -> Result<(), EngineError> {
        let snapshot = self.snapshot();
        let item = snapshot
            .items
            .iter()
            .find(|item| item.id == item_id)
            .ok_or_else(|| failed("That request was already answered"))?;
        self.inner.answer_approval(item, decision)?;
        self.inner.refresh(self.inner.clock.now());
        Ok(())
    }

    /// Approve every listed item that is a low-risk approval right now, each
    /// through the normal answer path. Anything else is skipped, never
    /// approved: medium and high risk always take a deliberate answer.
    pub fn approve_low_risk(&self, item_ids: &[String]) -> BatchApproval {
        let snapshot = self.snapshot();
        let mut result = BatchApproval::default();
        for id in item_ids {
            let item = snapshot
                .items
                .iter()
                .find(|item| &item.id == id)
                .filter(|item| item.batch_approvable());
            match item.map(|item| self.inner.answer_approval(item, ApprovalDecision::Approve)) {
                Some(Ok(())) => result.approved.push(id.clone()),
                Some(Err(err)) => {
                    tracing::warn!(item = %id, %err, "attention: batch approval not sent");
                    result.skipped.push(id.clone());
                }
                None => result.skipped.push(id.clone()),
            }
        }
        self.inner.refresh(self.inner.clock.now());
        result
    }

    /// Mark an item seen: its chat (and agent run) read, or a timed-out
    /// question acknowledged. Waiting approvals and questions can't be
    /// dismissed; they need an answer.
    pub fn dismiss(&self, item_id: &str) -> Result<bool, EngineError> {
        let snapshot = self.snapshot();
        let Some(item) = snapshot.items.iter().find(|item| item.id == item_id) else {
            return Ok(false);
        };
        let inner = &self.inner;
        let now = inner.clock.now();
        match item.kind {
            AttentionKind::Approval | AttentionKind::Question | AttentionKind::Limited => {
                return Err(failed("This needs an answer in its chat"));
            }
            AttentionKind::TimedOut => {
                let mut timed_out = lock(&inner.timed_out);
                timed_out.retain(|t| {
                    Some(&t.chat_id) != item.chat_id.as_ref()
                        || Some(&t.request_id) != item.request_id.as_ref()
                });
                device_settings::save(&inner.timed_out_path, &*timed_out)?;
            }
            AttentionKind::Failed | AttentionKind::Finished => {}
        }
        if let Some(run) = &item.agent_run {
            inner
                .agents
                .mark_runs(std::slice::from_ref(&run.run_id), Some(true), None)?;
        }
        if let Some(chat_id) = &item.chat_id
            && item.kind != AttentionKind::TimedOut
        {
            inner.workspace.mark_chat_seen(chat_id, now)?;
        }
        inner.refresh(now);
        Ok(true)
    }

    pub fn timeout_settings(&self) -> QuestionTimeoutSettings {
        lock(&self.inner.settings).clone()
    }

    /// Replace the device default (per-chat overrides are kept as given).
    pub fn set_timeout_settings(
        &self,
        settings: QuestionTimeoutSettings,
    ) -> Result<QuestionTimeoutSettings, EngineError> {
        self.inner.save_settings(settings)
    }

    /// Set (`Some`) or clear (`None`, back to the default) one chat's timeout.
    pub fn set_chat_timeout(
        &self,
        chat_id: &str,
        timeout: Option<ChatQuestionTimeout>,
    ) -> Result<QuestionTimeoutSettings, EngineError> {
        let mut settings = self.timeout_settings();
        match timeout {
            Some(timeout) => settings.chats.insert(chat_id.to_string(), timeout),
            None => settings.chats.remove(chat_id),
        };
        self.inner.save_settings(settings)
    }

    /// Derive at `now`, answering timeouts due by then; returns the next
    /// deadline. The loop calls this with the wall clock; tests call it.
    #[doc(hidden)]
    pub fn tick_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.inner.refresh(now);
        *lock(&self.inner.next_timeout)
    }
}

impl Inner {
    async fn run_loop(self: Arc<Self>) {
        let mut sessions = self.sessions.watch_sessions();
        let mut chats = self.workspace.watch_chats();
        let mut agents = self.agents.watch();
        loop {
            self.refresh(self.clock.now());
            let next_timeout = *lock(&self.next_timeout);
            let sleep = next_timeout
                .map(|at| (at - self.clock.now()).to_std().unwrap_or(Duration::ZERO))
                .unwrap_or(MAX_SLEEP)
                .min(MAX_SLEEP);
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                changed = sessions.changed() => if changed.is_err() { return },
                changed = chats.changed() => if changed.is_err() { return },
                changed = agents.changed() => if changed.is_err() { return },
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(sleep) => {}
            }
            tokio::time::sleep(COALESCE).await;
        }
    }

    fn save_settings(
        &self,
        settings: QuestionTimeoutSettings,
    ) -> Result<QuestionTimeoutSettings, EngineError> {
        device_settings::save(&self.settings_path, &settings)?;
        *lock(&self.settings) = settings.clone();
        self.wake.notify_one();
        Ok(settings)
    }

    /// The input requests waiting in this engine's live turns, minus the
    /// ones answered here and not yet applied.
    fn pending_inputs(&self, now: DateTime<Utc>) -> Vec<PendingInput> {
        let sessions = self.sessions.watch_sessions().borrow().clone();
        let mut open = Vec::new();
        for session in sessions.iter().filter(|s| {
            s.status == SessionStatus::AwaitingInput && self.sessions.turn_in_flight(&s.chat_id)
        }) {
            let Ok(entries) = self
                .doc_host
                .open(&session.chat_id)
                .and_then(|handle| Ok(handle.doc().read_entries()?))
            else {
                continue;
            };
            for (request_id, questions) in derive::open_inputs(&entries) {
                open.push((session.chat_id.clone(), request_id, questions));
            }
        }
        let keys: HashSet<(String, String)> = open
            .iter()
            .map(|(chat_id, request_id, _)| (chat_id.clone(), request_id.clone()))
            .collect();
        let mut answered = lock(&self.answered);
        answered.retain(|key| keys.contains(key));
        let mut first_seen = lock(&self.first_seen);
        first_seen.retain(|key, _| keys.contains(key) && !answered.contains(key));
        open.into_iter()
            .filter_map(|(chat_id, request_id, questions)| {
                let key = (chat_id, request_id);
                if answered.contains(&key) {
                    return None;
                }
                let since = *first_seen.entry(key.clone()).or_insert(now);
                Some(PendingInput {
                    chat_id: key.0,
                    request_id: key.1,
                    questions,
                    since,
                })
            })
            .collect()
    }

    fn refresh(&self, now: DateTime<Utc>) -> AttentionSnapshot {
        let _refreshing = lock(&self.refreshing);
        let mut pending = self.pending_inputs(now);
        self.time_out_questions(&mut pending, now);
        let chats = self.workspace.read_chats().unwrap_or_default();
        let spaces = self.workspace.read_spaces().unwrap_or_default();
        let sessions = self.sessions.watch_sessions().borrow().clone();
        let runs = self
            .agents
            .list_runs(&AgentRunFilter {
                limit: Some(RECENT_RUNS),
                ..Default::default()
            })
            .unwrap_or_default();
        self.prune_timed_out(&chats);
        let timed_out = lock(&self.timed_out).clone();
        let items = derive::derive(
            &Sources {
                device_id: &self.device_id,
                chats: &chats,
                spaces: &spaces,
                sessions: &sessions,
                pending: &pending,
                runs: &runs,
                timed_out: &timed_out,
            },
            now,
        );
        let snapshot = AttentionSnapshot {
            device_id: self.device_id.clone(),
            items,
        };
        self.snapshot.send_if_modified(|current| {
            let changed = *current != snapshot;
            if changed {
                *current = snapshot.clone();
            }
            changed
        });
        snapshot
    }

    /// Answer every plain question whose timeout passed, park it as timed
    /// out, and drop it from `pending`.
    fn time_out_questions(&self, pending: &mut Vec<PendingInput>, now: DateTime<Utc>) {
        let settings = lock(&self.settings).clone();
        let mut parked = Vec::new();
        let mut next_timeout: Option<DateTime<Utc>> = None;
        pending.retain(|input| {
            let Some(minutes) = settings.minutes_for(&input.chat_id) else {
                return true;
            };
            if !input.can_time_out() {
                return true;
            }
            let due = input.since + minutes_delta(minutes);
            if due > now {
                next_timeout = Some(next_timeout.map_or(due, |at| at.min(due)));
                return true;
            }
            let answers = input
                .questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec![QUESTION_TIMEOUT_ANSWER.to_string()],
                })
                .collect();
            if let Err(err) = self.queue_answer(&input.chat_id, &input.request_id, answers) {
                tracing::warn!(chat = %input.chat_id, %err, "attention: timeout answer not sent");
                return true;
            }
            parked.push(TimedOutQuestion {
                chat_id: input.chat_id.clone(),
                request_id: input.request_id.clone(),
                question: derive::question_summary(&input.questions),
                minutes,
                timed_out_at: now,
            });
            false
        });
        *lock(&self.next_timeout) = next_timeout;
        if parked.is_empty() {
            return;
        }
        let mut timed_out = lock(&self.timed_out);
        timed_out.extend(parked);
        if let Err(err) = device_settings::save(&self.timed_out_path, &*timed_out) {
            tracing::warn!(%err, "attention: timed-out questions not saved");
        }
    }

    /// Forget timed-out questions whose chat is gone or was seen since.
    fn prune_timed_out(&self, chats: &[zeron_proto::Chat]) {
        let mut timed_out = lock(&self.timed_out);
        let before = timed_out.len();
        timed_out.retain(|t| {
            chats.iter().any(|chat| {
                chat.id == t.chat_id
                    && !chat.archived
                    && chat.last_seen_at.is_none_or(|seen| seen < t.timed_out_at)
            })
        });
        if timed_out.len() != before
            && let Err(err) = device_settings::save(&self.timed_out_path, &*timed_out)
        {
            tracing::warn!(%err, "attention: timed-out questions not saved");
        }
    }

    fn answer_approval(
        &self,
        item: &AttentionItem,
        decision: ApprovalDecision,
    ) -> Result<(), EngineError> {
        let (Some(chat_id), Some(request_id), Some(approval)) =
            (&item.chat_id, &item.request_id, &item.approval)
        else {
            return Err(failed("Only approvals can be approved or declined here"));
        };
        let decision = if decision.approves() {
            decision
        } else {
            approval.decline()
        };
        let option = approval
            .decisions
            .iter()
            .find(|option| option.decision == decision)
            .ok_or_else(|| {
                failed(format!(
                    "This request can't be answered with {}",
                    decision.wire_name()
                ))
            })?;
        let question_id = self
            .question_id(chat_id, request_id)
            .ok_or_else(|| failed("That request was already answered"))?;
        self.queue_answer(
            chat_id,
            request_id,
            vec![UserInputAnswer {
                question_id,
                labels: vec![option.label.clone()],
            }],
        )
    }

    fn question_id(&self, chat_id: &str, request_id: &str) -> Option<String> {
        let entries = self
            .doc_host
            .open(chat_id)
            .ok()?
            .doc()
            .read_entries()
            .ok()?;
        derive::open_inputs(&entries)
            .into_iter()
            .find(|(id, _)| id == request_id)
            .and_then(|(_, questions)| questions.first().map(|q| q.id.clone()))
    }

    fn queue_answer(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<(), EngineError> {
        self.doc_host.queue_command(
            chat_id,
            SessionCommandPayload::RespondInput {
                request_id: request_id.to_string(),
                answers,
            },
        )?;
        lock(&self.answered).insert((chat_id.to_string(), request_id.to_string()));
        Ok(())
    }
}

fn minutes_delta(minutes: u32) -> TimeDelta {
    TimeDelta::minutes(i64::from(minutes))
}
