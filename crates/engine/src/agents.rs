//! Always-on agents: named prompts the engine fires on a cron schedule, an
//! interval, a one-shot time, a webhook or a file change — on the headless
//! engine too, so a VPS keeps them running while the laptop sleeps.
//!
//! - **Runs are normal chats.** Each fire creates a chat in the agent's space
//!   through the same workspace/doc-host calls `Mutate createChat` and
//!   `QueueCommand` use, so transcripts, approvals, diffs and terminals work
//!   unchanged. The run record links agent, trigger and chat.
//! - **Scheduler.** One tokio task sleeps until the earliest next fire
//!   (re-reading the wall clock at least every [`MAX_SLEEP`]). Schedule state
//!   is persisted per trigger, so restarts recompute the next fire; see
//!   [`schedule`] for misfire grace and backlog collapse. A fire is skipped
//!   while the agent's previous run is active, run keys (agent + trigger +
//!   scheduled time) make every fire idempotent, and three consecutive
//!   failures pause the agent with the reason recorded. A run stopped on a
//!   usage limit is Limited, not failed: it never counts toward the pause,
//!   and the agent's fires wait until the limit resets (fires due meanwhile
//!   run at the reset rather than being skipped as misfires).
//! - **Approvals wait.** A run parked on an approval shows `needs-approval`
//!   in the inbox until someone answers in the chat. Nothing here answers or
//!   denies on the user's behalf.
//! - **Memory.** `{data_dir}/agents/{id}/MEMORY.md` is injected into every
//!   run's prompt, and the agent is told it may update the file.
//!
//! Telegram fronts these agents in [`crate::telegram`]. Not yet: GitHub
//! event triggers (use the webhook trigger with a GitHub webhook for now).

mod prompt;
mod schedule;
mod store;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::Digest as _;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionCommandPayload};
use zeron_proto::agents::{
    AgentDraft, AgentRun, AgentRunFilter, AgentRunStatus, AgentSummary, AgentTrigger,
    AgentTriggerKind, AgentsSnapshot,
};
use zeron_proto::{ChatConfig, RunRequest, SessionStatus};
use zeron_rpc::{HttpRequest, HttpResponse};

use crate::doc_host::DocHost;
use crate::fs_watch::{FsWatch, FsWatchHub};
use crate::registry::HarnessRegistry;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id};

use prompt::{RunContext, WebhookPayload};
use schedule::{Due, Schedule, TriggerState};
use store::{AgentStore, Inserted, StoredAgent};

/// Longest the scheduler trusts one timer. Tokio timers run on the monotonic
/// clock, which stops while a laptop sleeps; re-reading the wall clock this
/// often bounds how late a wake-up notices due (or missed) fires.
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);
/// This many failed runs in a row pause the agent.
const AUTO_PAUSE_AFTER: u32 = 3;
/// A queued run that never reaches the harness within this is failed.
const NEVER_STARTED: Duration = Duration::from_secs(10 * 60);
const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);
/// A steady stream of writes still fires at least this often.
const MAX_DEBOUNCE: Duration = Duration::from_secs(30);
const SUMMARY_CHARS: usize = 280;
/// How long a limited agent waits when nothing names the reset.
const LIMIT_WAIT_UNKNOWN: chrono::TimeDelta = chrono::TimeDelta::hours(1);
const BUSY_SKIP: &str = "Skipped: the previous run was still active";
/// Path segments whose churn never wakes a file-watch agent.
const IGNORED_SEGMENTS: [&str; 4] = [".git", "node_modules", "target", ".DS_Store"];

/// Wall-clock source; tests drive the scheduler with a fixed one.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone)]
pub struct Agents {
    inner: Arc<Inner>,
}

struct Inner {
    store: AgentStore,
    data_dir: PathBuf,
    device_id: String,
    workspace: WorkspaceHost,
    doc_host: DocHost,
    sessions: SessionsEngine,
    registry: Arc<HarnessRegistry>,
    clock: Arc<dyn Clock>,
    /// Definitions changed: recompute the next fire now.
    wake: Notify,
    /// Bumped on every change the UI should repaint for.
    revision: watch::Sender<u64>,
    /// Serializes the "is a run active?" check with the insert that follows.
    fire_lock: tokio::sync::Mutex<()>,
    watches: Mutex<HashMap<String, FileWatchTask>>,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

struct FileWatchTask {
    roots: Vec<PathBuf>,
    debounce: Duration,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FileWatchTask {
    fn drop(&mut self) {
        // The task owns the FsWatch handles; aborting it releases the watches.
        self.task.abort();
    }
}

/// One fire, from any trigger.
struct Fire {
    agent: StoredAgent,
    kind: AgentTriggerKind,
    run_key: String,
    scheduled_at: DateTime<Utc>,
    collapsed: u32,
    webhook: Option<WebhookPayload>,
    changed_paths: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

/// Stable per-trigger key: an edit that leaves a trigger as it was keeps its
/// schedule history; any change to it starts a fresh count.
fn trigger_key(trigger: &AgentTrigger) -> String {
    let json = serde_json::to_string(trigger).unwrap_or_default();
    let digest = sha2::Sha256::digest(json.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn trigger_kind(trigger: &AgentTrigger) -> AgentTriggerKind {
    match trigger {
        AgentTrigger::Cron { .. } => AgentTriggerKind::Cron,
        AgentTrigger::Interval { .. } => AgentTriggerKind::Interval,
        AgentTrigger::Once { .. } => AgentTriggerKind::Once,
        AgentTrigger::Webhook => AgentTriggerKind::Webhook,
        AgentTrigger::FileWatch { .. } => AgentTriggerKind::FileWatch,
    }
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Compare secrets without an early exit on the first differing byte.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn human_duration(delta: chrono::TimeDelta) -> String {
    let minutes = delta.num_minutes();
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d", minutes / (24 * 60))
    }
}

/// Why every agent of a database copied from another install is paused.
pub const COPIED_INSTALL_REASON: &str =
    "Copied from another Godspeed install. Resume it to run here.";

/// Stamp the database with this install's id (its device id, which lives in
/// its own file). A database stamped by another install was copied here (a
/// dev home seeded from real data, a restored backup): its agents belong to
/// that install and could act on its real workspaces, so each one is paused
/// until someone resumes it on this install.
fn adopt_install(store: &AgentStore, install_id: &str) -> Result<(), EngineError> {
    match store.install_id()? {
        Some(owner) if owner == install_id => return Ok(()),
        Some(owner) => {
            for agent in store.agents()? {
                if agent.paused_reason.is_none() {
                    store.set_paused(&agent.id, Some(COPIED_INSTALL_REASON))?;
                }
            }
            tracing::warn!(%owner, "agents: database copied from another install; agents paused");
        }
        // Unstamped: brand new, or written before install ids. Adopt it.
        None => {}
    }
    store.set_install_id(install_id)
}

impl Agents {
    pub fn open(
        data_dir: &Path,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        sessions: SessionsEngine,
        registry: Arc<HarnessRegistry>,
    ) -> Result<Self, EngineError> {
        Self::open_with_clock(
            data_dir,
            workspace,
            doc_host,
            sessions,
            registry,
            Arc::new(SystemClock),
        )
    }

    pub fn open_with_clock(
        data_dir: &Path,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        sessions: SessionsEngine,
        registry: Arc<HarnessRegistry>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, EngineError> {
        let store = AgentStore::open(data_dir)?;
        adopt_install(&store, workspace.device_id())?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                data_dir: data_dir.to_path_buf(),
                device_id: workspace.device_id().to_string(),
                workspace,
                doc_host,
                sessions,
                registry,
                clock,
                wake: Notify::new(),
                revision: watch::channel(0).0,
                fire_lock: tokio::sync::Mutex::new(()),
                watches: Mutex::new(HashMap::new()),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        })
    }

    /// Settle runs a previous process left active, then start the scheduler
    /// and file watches. Needs a tokio runtime.
    pub fn start(&self) {
        let inner = &self.inner;
        match inner.store.active_runs() {
            Ok(stale) => {
                for mut run in stale {
                    // The harness process died with the old engine; recovery
                    // already stamped its transcript aborted. Not the agent's
                    // fault, so it doesn't count toward auto-pause.
                    run.status = AgentRunStatus::Failed;
                    run.finished_at = Some(inner.clock.now());
                    run.summary =
                        Some("Interrupted: the engine restarted before the run finished".into());
                    if let Err(err) = inner.store.update_run(&run) {
                        tracing::warn!(run = %run.id, %err, "agents: stale run not settled");
                    }
                }
            }
            Err(err) => tracing::warn!(%err, "agents: stale runs not read"),
        }
        let looping = inner.clone();
        inner.tasks.spawn(async move { looping.run_loop().await });
        inner.reconcile_watches();
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        lock(&self.inner.watches).clear();
        self.inner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.tasks.wait()).await;
    }

    /// Revision counter for `WatchAgents`.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.inner.revision.subscribe()
    }

    pub fn list(&self) -> Result<Vec<AgentSummary>, EngineError> {
        let now = self.inner.clock.now();
        self.inner
            .store
            .agents()?
            .iter()
            .map(|agent| self.inner.summary(agent, now))
            .collect()
    }

    pub fn snapshot(&self) -> Result<AgentsSnapshot, EngineError> {
        let (unread_runs, needs_approval_runs) = self.inner.store.inbox_counts()?;
        Ok(AgentsSnapshot {
            agents: self.list()?,
            unread_runs,
            needs_approval_runs,
        })
    }

    pub fn create(&self, draft: AgentDraft) -> Result<AgentSummary, EngineError> {
        let inner = &self.inner;
        let (draft, workspace_path) = inner.validate(draft)?;
        let now = inner.clock.now();
        let agent = StoredAgent {
            id: new_id(),
            draft,
            workspace_path,
            webhook_token: new_token(),
            consecutive_failures: 0,
            paused_reason: None,
            created_at: now,
            updated_at: now,
        };
        inner.store.put_agent(&agent)?;
        inner.init_triggers(&agent, now)?;
        inner.ensure_memory(&agent)?;
        inner.changed();
        inner.summary(&agent, now)
    }

    pub fn update(&self, id: &str, draft: AgentDraft) -> Result<AgentSummary, EngineError> {
        let inner = &self.inner;
        let mut agent = inner.agent(id)?;
        let (draft, workspace_path) = inner.validate(draft)?;
        let re_enabled = draft.enabled && !agent.draft.enabled;
        agent.draft = draft;
        agent.workspace_path = workspace_path;
        agent.updated_at = inner.clock.now();
        if re_enabled {
            agent.paused_reason = None;
            agent.consecutive_failures = 0;
        }
        inner.store.put_agent(&agent)?;
        let keep: Vec<String> = if re_enabled {
            Vec::new()
        } else {
            agent.draft.triggers.iter().map(trigger_key).collect()
        };
        inner.store.retain_triggers(&agent.id, &keep)?;
        inner.init_triggers(&agent, agent.updated_at)?;
        inner.changed();
        inner.summary(&agent, inner.clock.now())
    }

    /// Delete the agent, its schedule state, inbox rows and memory file.
    /// Chats it created stay — they are ordinary chats.
    pub fn delete(&self, id: &str) -> Result<bool, EngineError> {
        let deleted = self.inner.store.delete_agent(id)?;
        if deleted {
            let dir = self.inner.agent_dir(id);
            if let Err(err) = std::fs::remove_dir_all(&dir)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(dir = %dir.display(), %err, "agents: memory dir not removed");
            }
            self.inner.changed();
        }
        Ok(deleted)
    }

    /// Enabling lifts an auto-pause and restarts every schedule from now, so
    /// fires missed while it was off are not reported as misfires.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<AgentSummary, EngineError> {
        let inner = &self.inner;
        let mut agent = inner.agent(id)?;
        if enabled {
            agent.paused_reason = None;
            agent.consecutive_failures = 0;
            inner.store.retain_triggers(&agent.id, &[])?;
        }
        agent.draft.enabled = enabled;
        agent.updated_at = inner.clock.now();
        inner.store.put_agent(&agent)?;
        inner.init_triggers(&agent, agent.updated_at)?;
        inner.changed();
        inner.summary(&agent, inner.clock.now())
    }

    /// Fire once now, regardless of schedule, enablement or pause (an explicit
    /// request is the way to test an agent). Still one run at a time.
    pub async fn run_now(&self, id: &str) -> Result<AgentRun, EngineError> {
        let inner = &self.inner;
        let agent = inner.agent(id)?;
        let now = inner.clock.now();
        inner
            .fire(Fire {
                run_key: format!("{}:manual:{}", agent.id, now.timestamp_millis()),
                agent,
                kind: AgentTriggerKind::Manual,
                scheduled_at: now,
                collapsed: 0,
                webhook: None,
                changed_paths: Vec::new(),
            })
            .await
    }

    pub fn rotate_webhook(&self, id: &str) -> Result<AgentSummary, EngineError> {
        let inner = &self.inner;
        let mut agent = inner.agent(id)?;
        agent.webhook_token = new_token();
        agent.updated_at = inner.clock.now();
        inner.store.put_agent(&agent)?;
        inner.changed();
        inner.summary(&agent, inner.clock.now())
    }

    pub fn list_runs(&self, filter: &AgentRunFilter) -> Result<Vec<AgentRun>, EngineError> {
        self.inner.store.list_runs(filter)
    }

    /// [`Self::owns_chat`] through a weak handle, for hooks that must not
    /// keep the agents graph (and the sessions engine it holds) alive.
    pub fn owns_chat_probe(&self) -> Arc<dyn Fn(&str) -> bool + Send + Sync> {
        let weak = Arc::downgrade(&self.inner);
        Arc::new(move |chat_id| {
            weak.upgrade()
                .is_some_and(|inner| Agents { inner }.owns_chat(chat_id))
        })
    }

    /// The chat was created by one of this device's agent runs.
    pub fn owns_chat(&self, chat_id: &str) -> bool {
        self.inner
            .store
            .list_runs(&AgentRunFilter {
                chat_id: Some(chat_id.to_owned()),
                include_archived: true,
                ..Default::default()
            })
            .is_ok_and(|runs| !runs.is_empty())
    }

    pub fn mark_runs(
        &self,
        ids: &[String],
        read: Option<bool>,
        archived: Option<bool>,
    ) -> Result<usize, EngineError> {
        let changed = self.inner.store.mark_runs(ids, read, archived)?;
        if changed > 0 {
            self.inner.revision.send_modify(|rev| *rev += 1);
        }
        Ok(changed)
    }

    /// Evaluate every timer trigger at `now` and fire what is due. The
    /// scheduler loop calls this with the wall clock; tests call it directly.
    #[doc(hidden)]
    pub async fn tick_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.inner.tick(now).await
    }

    /// `POST /hooks/agents/{id}/{token}` on the IPC port. Unknown agents and
    /// wrong tokens both answer 404 so the endpoint doesn't confirm ids.
    pub async fn handle_webhook(&self, request: HttpRequest) -> HttpResponse {
        let inner = &self.inner;
        let not_found = || HttpResponse::json(404, &serde_json::json!({ "error": "not found" }));
        let path = request.path.split('?').next().unwrap_or_default();
        let Some((id, token)) = path
            .strip_prefix("/hooks/agents/")
            .and_then(|rest| rest.split_once('/'))
        else {
            return not_found();
        };
        let agent = match inner.store.agent(id) {
            Ok(Some(agent)) if same_secret(token.trim_end_matches('/'), &agent.webhook_token) => {
                agent
            }
            Ok(_) => return not_found(),
            Err(err) => {
                return HttpResponse::json(500, &serde_json::json!({ "error": err.to_string() }));
            }
        };
        if request.method != "POST" {
            return HttpResponse::json(405, &serde_json::json!({ "error": "use POST" }));
        }
        let conflict =
            |message: String| HttpResponse::json(409, &serde_json::json!({ "error": message }));
        if !agent
            .draft
            .triggers
            .iter()
            .any(|t| matches!(t, AgentTrigger::Webhook))
        {
            return conflict("this agent has no webhook trigger".into());
        }
        if !agent.draft.enabled {
            return conflict("agent is disabled".into());
        }
        if let Some(reason) = &agent.paused_reason {
            return conflict(format!("agent is paused: {reason}"));
        }
        let now = inner.clock.now();
        // Senders that retry (GitHub, most queues) name each delivery; reuse
        // that as the run key so a redelivery never runs the agent twice.
        let delivery = ["x-github-delivery", "idempotency-key", "x-request-id"]
            .iter()
            .find_map(|name| request.header(name))
            .map(str::to_owned)
            .unwrap_or_else(|| now.timestamp_millis().to_string());
        let payload = WebhookPayload {
            headers: request.headers.clone(),
            body: String::from_utf8_lossy(&request.body).into_owned(),
        };
        let fire = Fire {
            run_key: format!("{}:webhook:{delivery}", agent.id),
            agent,
            kind: AgentTriggerKind::Webhook,
            scheduled_at: now,
            collapsed: 0,
            webhook: Some(payload),
            changed_paths: Vec::new(),
        };
        match inner.fire(fire).await {
            Ok(run) => HttpResponse::json(
                202,
                &serde_json::json!({ "runId": run.id, "status": run.status, "chatId": run.chat_id }),
            ),
            Err(err) => HttpResponse::json(500, &serde_json::json!({ "error": err.to_string() })),
        }
    }
}

impl Inner {
    fn changed(self: &Arc<Self>) {
        self.revision.send_modify(|rev| *rev += 1);
        self.wake.notify_one();
        self.reconcile_watches();
    }

    fn agent(&self, id: &str) -> Result<StoredAgent, EngineError> {
        self.store
            .agent(id)?
            .ok_or_else(|| failed(format!("no such agent: {id}")))
    }

    /// Anchor every timer trigger without state at `now`, so the schedule
    /// counts from the moment it was saved or re-enabled.
    fn init_triggers(&self, agent: &StoredAgent, now: DateTime<Utc>) -> Result<(), EngineError> {
        for trigger in &agent.draft.triggers {
            let key = trigger_key(trigger);
            if self.store.trigger_state(&agent.id, &key)?.is_none() {
                let state = TriggerState {
                    anchor: now,
                    last_scheduled: None,
                };
                self.store.put_trigger_state(&agent.id, &key, &state)?;
            }
        }
        Ok(())
    }

    fn agent_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("agents").join(id)
    }

    fn memory_path(&self, id: &str) -> PathBuf {
        self.agent_dir(id).join("MEMORY.md")
    }

    fn ensure_memory(&self, agent: &StoredAgent) -> Result<PathBuf, EngineError> {
        let path = self.memory_path(&agent.id);
        if !path.exists() {
            std::fs::create_dir_all(self.agent_dir(&agent.id))?;
            std::fs::write(
                &path,
                format!(
                    "# {} memory\n\nNotes this agent keeps between runs.\n",
                    agent.draft.name.trim()
                ),
            )?;
        }
        Ok(path)
    }

    /// Normalize a draft and resolve its workspace folder. Every message
    /// here is user-facing.
    fn validate(&self, mut draft: AgentDraft) -> Result<(AgentDraft, String), EngineError> {
        draft.name = draft.name.trim().to_string();
        draft.normalize_access();
        let mode = draft.access_mode();
        if !zeron_proto::access_mode_supported(draft.harness, mode) {
            return Err(failed(format!(
                "{:?} can't run in {}; pick another access mode",
                draft.harness,
                mode.label()
            )));
        }
        if draft.name.is_empty() {
            return Err(failed("Give the agent a name"));
        }
        if draft.name.chars().count() > 80 {
            return Err(failed("Agent names are at most 80 characters"));
        }
        if draft.instructions.trim().is_empty() {
            return Err(failed("Write the agent's instructions"));
        }
        draft.model = draft.model.filter(|m| !m.trim().is_empty());
        if !self
            .registry
            .descriptors()
            .iter()
            .any(|d| d.id == draft.harness)
        {
            return Err(failed(format!(
                "{:?} is not available on this device",
                draft.harness
            )));
        }
        let space = self
            .workspace
            .space(&draft.space_id)?
            .ok_or_else(|| failed("Pick a workspace for the agent"))?;
        if space.device_id != self.device_id {
            return Err(failed(
                "The workspace must be on the device that runs the agent",
            ));
        }
        let mut webhooks = 0;
        for trigger in &mut draft.triggers {
            Schedule::parse(trigger).map_err(failed)?;
            match trigger {
                AgentTrigger::Webhook => webhooks += 1,
                AgentTrigger::FileWatch { paths, .. } => {
                    for path in paths.iter_mut() {
                        *path = path.trim().trim_start_matches("./").to_string();
                        if Path::new(path.as_str()).is_absolute()
                            || Path::new(path.as_str())
                                .components()
                                .any(|c| matches!(c, std::path::Component::ParentDir))
                        {
                            return Err(failed(format!(
                                "Watched path {path:?} must be inside the workspace"
                            )));
                        }
                    }
                    paths.retain(|p| !p.is_empty());
                }
                AgentTrigger::Cron { expr, timezone } => {
                    *expr = expr.trim().to_string();
                    *timezone = timezone.trim().to_string();
                }
                AgentTrigger::Interval { .. } | AgentTrigger::Once { .. } => {}
            }
        }
        if webhooks > 1 {
            return Err(failed("An agent has a single webhook URL"));
        }
        Ok((draft, space.path))
    }

    fn summary(
        &self,
        agent: &StoredAgent,
        now: DateTime<Utc>,
    ) -> Result<AgentSummary, EngineError> {
        let schedulable = agent.draft.enabled && agent.paused_reason.is_none();
        let mut next_fire_at: Option<DateTime<Utc>> = None;
        if schedulable {
            for trigger in &agent.draft.triggers {
                let Ok(Some(schedule)) = Schedule::parse(trigger) else {
                    continue;
                };
                let state = self
                    .store
                    .trigger_state(&agent.id, &trigger_key(trigger))?
                    .unwrap_or(TriggerState {
                        anchor: now,
                        last_scheduled: None,
                    });
                if let Some(next) = state.next_fire(&schedule) {
                    next_fire_at = Some(next_fire_at.map_or(next, |cur| cur.min(next)));
                }
            }
        }
        if let Some(until) = self.store.limited_until(&agent.id)?.filter(|u| *u > now) {
            next_fire_at = next_fire_at.map(|next| next.max(until));
        }
        let webhook_path = format!("/hooks/agents/{}/{}", agent.id, agent.webhook_token);
        Ok(AgentSummary {
            id: agent.id.clone(),
            draft: agent.draft.clone(),
            workspace_path: agent.workspace_path.clone(),
            memory_path: self.memory_path(&agent.id).to_string_lossy().into_owned(),
            webhook_url: self
                .sessions
                .ipc_port()
                .map(|port| format!("http://127.0.0.1:{port}{webhook_path}")),
            webhook_path,
            paused_reason: agent.paused_reason.clone(),
            consecutive_failures: agent.consecutive_failures,
            next_fire_at,
            active_run_id: self.store.active_run(&agent.id)?.map(|run| run.id),
            last_run: self.store.latest_run(&agent.id)?,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        })
    }

    // ── scheduler ─────────────────────────────────────────────────────────

    async fn run_loop(self: Arc<Self>) {
        loop {
            let next = self.tick(self.clock.now()).await;
            let sleep = next
                .map(|at| (at - self.clock.now()).to_std().unwrap_or(Duration::ZERO))
                .unwrap_or(MAX_SLEEP)
                .min(MAX_SLEEP);
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(sleep) => {}
            }
        }
    }

    async fn tick(self: &Arc<Self>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let agents = match self.store.agents() {
            Ok(agents) => agents,
            Err(err) => {
                tracing::warn!(%err, "agents: scheduler could not read definitions");
                return None;
            }
        };
        let mut earliest: Option<DateTime<Utc>> = None;
        let mut note = |next: Option<DateTime<Utc>>| {
            if let Some(next) = next {
                earliest = Some(earliest.map_or(next, |cur: DateTime<Utc>| cur.min(next)));
            }
        };
        for agent in agents {
            if !agent.draft.enabled || agent.paused_reason.is_some() {
                continue;
            }
            // Out of usage: nothing fires before the reset.
            let limited_until = self.store.limited_until(&agent.id).ok().flatten();
            if let Some(until) = limited_until.filter(|until| *until > now) {
                note(Some(until));
                continue;
            }
            for trigger in &agent.draft.triggers {
                let Ok(Some(schedule)) = Schedule::parse(trigger) else {
                    continue;
                };
                match self
                    .tick_trigger(&agent, trigger, &schedule, now, limited_until)
                    .await
                {
                    Ok(next) => note(next),
                    Err(err) => {
                        tracing::warn!(agent = %agent.id, %err, "agents: trigger tick failed");
                    }
                }
            }
        }
        earliest
    }

    async fn tick_trigger(
        self: &Arc<Self>,
        agent: &StoredAgent,
        trigger: &AgentTrigger,
        schedule: &Schedule,
        now: DateTime<Utc>,
        limited_until: Option<DateTime<Utc>>,
    ) -> Result<Option<DateTime<Utc>>, EngineError> {
        let key = trigger_key(trigger);
        let mut state = match self.store.trigger_state(&agent.id, &key)? {
            Some(state) => state,
            None => {
                let state = TriggerState {
                    anchor: now,
                    last_scheduled: None,
                };
                self.store.put_trigger_state(&agent.id, &key, &state)?;
                state
            }
        };
        let (scheduled_at, collapsed, missed) = match schedule::evaluate(schedule, &state, now) {
            Due::Idle { next } => return Ok(next),
            Due::Fire {
                scheduled_at,
                collapsed,
            } => (scheduled_at, collapsed, false),
            // A fire that fell due while the agent waited out a usage limit
            // was deferred, not missed: it runs now that the limit reset.
            Due::Missed {
                scheduled_at,
                collapsed,
            } => (
                scheduled_at,
                collapsed,
                !limited_until.is_some_and(|until| scheduled_at <= until),
            ),
        };
        let run_key = format!("{}:{key}:{}", agent.id, scheduled_at.timestamp_millis());
        let kind = trigger_kind(trigger);
        if missed {
            let reason = format!(
                "Skipped: missed by {} (misfire grace is {} minutes)",
                human_duration(now - scheduled_at),
                schedule::MISFIRE_GRACE.num_minutes()
            );
            self.record_skip(agent, kind, &run_key, scheduled_at, collapsed, reason)?;
        } else {
            self.fire(Fire {
                agent: agent.clone(),
                kind,
                run_key,
                scheduled_at,
                collapsed,
                webhook: None,
                changed_paths: Vec::new(),
            })
            .await?;
        }
        // Advanced only after the run row exists: a crash in between re-fires
        // the same run key on restart, which the store ignores.
        state.last_scheduled = Some(scheduled_at);
        self.store.put_trigger_state(&agent.id, &key, &state)?;
        Ok(state.next_fire(schedule))
    }

    fn record_skip(
        self: &Arc<Self>,
        agent: &StoredAgent,
        kind: AgentTriggerKind,
        run_key: &str,
        scheduled_at: DateTime<Utc>,
        collapsed: u32,
        reason: String,
    ) -> Result<AgentRun, EngineError> {
        let now = self.clock.now();
        let run = AgentRun {
            id: new_id(),
            agent_id: agent.id.clone(),
            agent_name: agent.draft.name.clone(),
            trigger: kind,
            scheduled_at,
            started_at: None,
            finished_at: Some(now),
            status: AgentRunStatus::Skipped,
            summary: Some(reason),
            chat_id: None,
            collapsed,
            // Visible in the inbox, but not something to "read".
            read: true,
            archived: false,
            created_at: now,
        };
        let run = match self.store.insert_run(run_key, &run)? {
            Inserted::New(run) | Inserted::Existing(run) => run,
        };
        self.revision.send_modify(|rev| *rev += 1);
        Ok(run)
    }

    async fn fire(self: &Arc<Self>, fire: Fire) -> Result<AgentRun, EngineError> {
        let _serial = self.fire_lock.lock().await;
        let now = self.clock.now();
        if self.store.active_run(&fire.agent.id)?.is_some() {
            // Fold repeated busy skips into one row rather than one per tick
            // while a run waits hours on an approval.
            if let Some(mut last) = self.store.latest_run(&fire.agent.id)?
                && last.status == AgentRunStatus::Skipped
                && last.summary.as_deref() == Some(BUSY_SKIP)
            {
                last.collapsed = last.collapsed.saturating_add(1 + fire.collapsed);
                last.scheduled_at = fire.scheduled_at;
                last.finished_at = Some(now);
                self.store.update_run(&last)?;
                self.revision.send_modify(|rev| *rev += 1);
                return Ok(last);
            }
            return self.record_skip(
                &fire.agent,
                fire.kind,
                &fire.run_key,
                fire.scheduled_at,
                fire.collapsed,
                BUSY_SKIP.into(),
            );
        }
        let run = AgentRun {
            id: new_id(),
            agent_id: fire.agent.id.clone(),
            agent_name: fire.agent.draft.name.clone(),
            trigger: fire.kind,
            scheduled_at: fire.scheduled_at,
            started_at: None,
            finished_at: None,
            status: AgentRunStatus::Queued,
            summary: None,
            chat_id: None,
            collapsed: fire.collapsed,
            read: false,
            archived: false,
            created_at: now,
        };
        let run = match self.store.insert_run(&fire.run_key, &run)? {
            Inserted::Existing(run) => return Ok(run),
            Inserted::New(run) => run,
        };
        // Launch synchronously so the caller (run-now, webhook) gets the chat
        // id back; the monitor that follows the run is a background task.
        let launched = self.launch(&run, &fire);
        let run = match launched {
            Ok(run) => {
                let monitor = self.clone();
                let watched = run.clone();
                self.tasks
                    .spawn(async move { monitor.monitor(watched).await });
                run
            }
            Err(err) => {
                let mut run = run;
                self.finish(&mut run, AgentRunStatus::Failed, Some(err.to_string()));
                run
            }
        };
        self.revision.send_modify(|rev| *rev += 1);
        Ok(run)
    }

    /// Create the run's chat and queue its first turn — the same workspace
    /// and doc-host calls behind `Mutate createChat` + `QueueCommand`.
    fn launch(&self, run: &AgentRun, fire: &Fire) -> Result<AgentRun, EngineError> {
        let agent = &fire.agent;
        let space = match self.workspace.space(&agent.draft.space_id)? {
            Some(space) => space,
            // The space row may have been re-created (profile switch, re-add):
            // fall back to the same folder on this device.
            None => self
                .workspace
                .read_spaces()?
                .into_iter()
                .find(|s| s.device_id == self.device_id && s.path == agent.workspace_path)
                .ok_or_else(|| {
                    failed(format!(
                        "Workspace {} is no longer in Godspeed",
                        agent.workspace_path
                    ))
                })?,
        };
        if space.device_id != self.device_id {
            return Err(failed("The agent's workspace is on another device"));
        }
        let memory_path = self.ensure_memory(agent)?;
        let memory = std::fs::read_to_string(&memory_path).unwrap_or_default();
        let trigger_label = fire.kind.label();
        let prompt = prompt::build_prompt(&RunContext {
            agent_name: &agent.draft.name,
            trigger_label,
            scheduled_at: fire.scheduled_at,
            memory_path: &memory_path,
            memory: &memory,
            instructions: &agent.draft.instructions,
            webhook: fire.webhook.as_ref(),
            changed_paths: &fire.changed_paths,
        });
        // The agent's access mode, with its memory directory writable under
        // the sandboxed modes for drivers that take extra roots.
        let mode = agent.draft.access_mode();
        let (sandbox, auto_approve) = mode.legacy();
        let access = zeron_proto::RunAccess {
            mode,
            writable_roots: vec![self.agent_dir(&agent.id).to_string_lossy().into_owned()],
        };
        let config = ChatConfig {
            harness: agent.draft.harness,
            model: agent.draft.model.clone(),
            reasoning: agent.draft.reasoning,
            model_options: Default::default(),
            sandbox,
            access_mode: Some(mode),
        };
        let chat_id = new_id();
        self.workspace.create_chat(
            &chat_id,
            Some(&space.id),
            Some(&self.device_id),
            Some(config),
            None,
        )?;
        // Titled before the first turn so the auto-titler leaves it alone.
        self.workspace
            .rename_chat(&chat_id, &format!("{} · {trigger_label}", agent.draft.name))?;
        let mut run = run.clone();
        run.chat_id = Some(chat_id.clone());
        run.started_at = Some(self.clock.now());
        self.store.update_run(&run)?;
        self.doc_host.queue_command(
            &chat_id,
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt,
                    harness: Some(agent.draft.harness),
                    model: agent.draft.model.clone(),
                    reasoning: agent.draft.reasoning,
                    model_options: Default::default(),
                    cwd: space.path.clone(),
                    sandbox,
                    auto_approve,
                    resume: None,
                    attachments: Vec::new(),
                    worktree: None,
                    // The host injects Godspeed's MCP server itself.
                    mcp: None,
                    access: Some(access),
                },
                message_id: new_id(),
            },
        )?;
        Ok(run)
    }

    /// Follow the run's chat through the sessions status feed until it ends.
    async fn monitor(self: Arc<Self>, mut run: AgentRun) {
        let Some(chat_id) = run.chat_id.clone() else {
            return;
        };
        let mut statuses = self.sessions.watch_sessions();
        let mut seen_active = false;
        let never_started = tokio::time::Instant::now() + NEVER_STARTED;
        loop {
            let session = statuses
                .borrow_and_update()
                .iter()
                .find(|s| s.chat_id == chat_id)
                .cloned();
            if let Some(session) = session {
                match session.status {
                    SessionStatus::Working => {
                        seen_active = true;
                        self.set_status(&mut run, AgentRunStatus::Running);
                    }
                    SessionStatus::AwaitingInput => {
                        seen_active = true;
                        self.set_status(&mut run, AgentRunStatus::NeedsApproval);
                    }
                    SessionStatus::Errored
                        if let Some(limit) = self
                            .workspace
                            .chat(&chat_id)
                            .ok()
                            .flatten()
                            .and_then(|chat| chat.limit) =>
                    {
                        self.finish_limited(&mut run, &limit);
                        return;
                    }
                    SessionStatus::Idle | SessionStatus::Errored => {
                        let errored = session.status == SessionStatus::Errored;
                        // Watches coalesce, so a fast turn can go straight
                        // from nothing to Idle; the transcript settles it.
                        match self.outcome_from_doc(&chat_id, errored) {
                            Some((status, summary)) => {
                                self.finish(&mut run, status, summary);
                                return;
                            }
                            None if seen_active || errored => {
                                let status = if errored {
                                    AgentRunStatus::Failed
                                } else {
                                    AgentRunStatus::Succeeded
                                };
                                self.finish(&mut run, status, None);
                                return;
                            }
                            None => {}
                        }
                    }
                }
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                changed = statuses.changed() => if changed.is_err() { return },
                _ = tokio::time::sleep_until(never_started), if !seen_active => {
                    self.finish(
                        &mut run,
                        AgentRunStatus::Failed,
                        Some("The run never started (is the agent's harness installed?)".into()),
                    );
                    return;
                }
            }
        }
    }

    /// The finished turn's outcome and summary, once the transcript holds
    /// a settled assistant reply to the run's prompt.
    fn outcome_from_doc(
        &self,
        chat_id: &str,
        errored: bool,
    ) -> Option<(AgentRunStatus, Option<String>)> {
        let entries = self
            .doc_host
            .open(chat_id)
            .ok()?
            .doc()
            .read_entries()
            .ok()?;
        let last_user = entries.iter().rposition(|e| e.role == MessageRole::User);
        let reply: Vec<_> = entries
            .iter()
            .skip(last_user.map_or(0, |i| i + 1))
            .filter(|e| e.role == MessageRole::Assistant)
            .collect();
        let last = reply.last()?;
        if last.status == Some(MessageStatus::Streaming) {
            return None;
        }
        let mut text = String::new();
        let mut error = None;
        for part in reply.iter().flat_map(|e| e.parts.iter()) {
            match part {
                MessagePart::Text { text: chunk, .. } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(chunk);
                }
                MessagePart::Error { message, .. } => error = Some(message.clone()),
                _ => {}
            }
        }
        let aborted = last.status == Some(MessageStatus::Aborted);
        // An error part fails the run even after partial text: the reply
        // stopped short of what was asked, so it never reads as Succeeded.
        if errored || aborted || error.is_some() {
            let reason = error
                .or_else(|| {
                    (!text.trim().is_empty()).then(|| prompt::excerpt(&text, SUMMARY_CHARS))
                })
                .unwrap_or_else(|| "The run was interrupted".into());
            return Some((
                AgentRunStatus::Failed,
                Some(prompt::excerpt(&reason, SUMMARY_CHARS)),
            ));
        }
        let summary = (!text.trim().is_empty()).then(|| prompt::excerpt(&text, SUMMARY_CHARS));
        Some((AgentRunStatus::Succeeded, summary))
    }

    fn set_status(&self, run: &mut AgentRun, status: AgentRunStatus) {
        if run.status == status {
            return;
        }
        run.status = status;
        // A run that needs someone resurfaces as unread.
        if status == AgentRunStatus::NeedsApproval {
            run.read = false;
        }
        if let Err(err) = self.store.update_run(run) {
            tracing::warn!(run = %run.id, %err, "agents: run status not saved");
        }
        self.revision.send_modify(|rev| *rev += 1);
    }

    fn finish(&self, run: &mut AgentRun, status: AgentRunStatus, summary: Option<String>) {
        run.status = status;
        run.summary = summary;
        run.finished_at = Some(self.clock.now());
        run.read = false;
        if let Err(err) = self.store.update_run(run) {
            tracing::warn!(run = %run.id, %err, "agents: run outcome not saved");
        }
        let failed = status == AgentRunStatus::Failed;
        match self.store.record_outcome(&run.agent_id, failed) {
            Ok(streak) if failed && streak >= AUTO_PAUSE_AFTER => {
                let reason = format!(
                    "Paused after {streak} consecutive failures. Last error: {}",
                    run.summary.as_deref().unwrap_or("unknown")
                );
                if let Err(err) = self.store.set_paused(&run.agent_id, Some(&reason)) {
                    tracing::warn!(agent = %run.agent_id, %err, "agents: auto-pause not saved");
                }
                tracing::info!(agent = %run.agent_id, "agents: auto-paused after repeated failures");
                lock(&self.watches)
                    .retain(|key, _| !key.starts_with(&format!("{}:", run.agent_id)));
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(agent = %run.agent_id, %err, "agents: outcome not counted"),
        }
        self.revision.send_modify(|rev| *rev += 1);
        self.wake.notify_one();
    }

    /// The run stopped on a usage limit: recorded as Limited, kept out of
    /// the failure streak, and the agent's fires wait for the reset.
    fn finish_limited(&self, run: &mut AgentRun, limit: &zeron_proto::ChatLimit) {
        let now = self.clock.now();
        let until = limit
            .reset_at
            .filter(|at| *at > now)
            .unwrap_or(now + LIMIT_WAIT_UNKNOWN);
        run.status = AgentRunStatus::Limited;
        run.summary = Some(match limit.reset_at {
            Some(at) => format!(
                "Usage limit reached. The next run waits for the reset at {}",
                at.format("%Y-%m-%d %H:%M UTC")
            ),
            None => format!(
                "Usage limit reached. The next run waits {} (no reset time was given)",
                human_duration(LIMIT_WAIT_UNKNOWN)
            ),
        });
        run.finished_at = Some(now);
        run.read = false;
        if let Err(err) = self.store.update_run(run) {
            tracing::warn!(run = %run.id, %err, "agents: limited run not saved");
        }
        if let Err(err) = self.store.set_limited_until(&run.agent_id, Some(until)) {
            tracing::warn!(agent = %run.agent_id, %err, "agents: limit wait not saved");
        }
        tracing::info!(agent = %run.agent_id, %until, "agents: run usage-limited; fires wait for the reset");
        self.revision.send_modify(|rev| *rev += 1);
        self.wake.notify_one();
    }

    // ── file watch ────────────────────────────────────────────────────────

    /// Bring the file watches in line with the stored agents: one debounced
    /// watch per enabled, unpaused file-watch trigger.
    fn reconcile_watches(self: &Arc<Self>) {
        if self.cancel.is_cancelled() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let agents = match self.store.agents() {
            Ok(agents) => agents,
            Err(err) => {
                tracing::warn!(%err, "agents: file watches not reconciled");
                return;
            }
        };
        let mut wanted: HashMap<String, (String, Vec<PathBuf>, Duration)> = HashMap::new();
        for agent in agents
            .iter()
            .filter(|a| a.draft.enabled && a.paused_reason.is_none())
        {
            let root = self
                .workspace
                .space(&agent.draft.space_id)
                .ok()
                .flatten()
                .map(|s| PathBuf::from(s.path))
                .unwrap_or_else(|| PathBuf::from(&agent.workspace_path));
            for trigger in &agent.draft.triggers {
                let AgentTrigger::FileWatch { paths, debounce_ms } = trigger else {
                    continue;
                };
                let roots = if paths.is_empty() {
                    vec![root.clone()]
                } else {
                    paths.iter().map(|p| root.join(p)).collect()
                };
                let debounce = debounce_ms
                    .map(Duration::from_millis)
                    .unwrap_or(DEFAULT_DEBOUNCE);
                wanted.insert(
                    format!("{}:{}", agent.id, trigger_key(trigger)),
                    (agent.id.clone(), roots, debounce),
                );
            }
        }
        let mut watches = lock(&self.watches);
        watches.retain(|key, task| {
            wanted.get(key).is_some_and(|(_, roots, debounce)| {
                *roots == task.roots && *debounce == task.debounce
            })
        });
        for (key, (agent_id, roots, debounce)) in wanted {
            if watches.contains_key(&key) {
                continue;
            }
            let inner = self.clone();
            let task_roots = roots.clone();
            let trigger = key.clone();
            let task = tokio::spawn(async move {
                inner
                    .watch_files(agent_id, trigger, task_roots, debounce)
                    .await;
            });
            watches.insert(
                key,
                FileWatchTask {
                    roots,
                    debounce,
                    task,
                },
            );
        }
    }

    async fn watch_files(
        self: Arc<Self>,
        agent_id: String,
        watch_key: String,
        roots: Vec<PathBuf>,
        debounce: Duration,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
        let registering = roots.clone();
        // `FsWatchHub::watch` canonicalizes (a blocking stat).
        let handles: Vec<FsWatch> = match tokio::task::spawn_blocking(move || {
            let hub = FsWatchHub::global();
            registering
                .iter()
                .map(|root| {
                    let tx = tx.clone();
                    hub.watch(root, notify::RecursiveMode::Recursive, move |event| {
                        // The hub's go-live rescan is not a change.
                        if event.need_rescan() || matches!(event.kind, notify::EventKind::Access(_))
                        {
                            return;
                        }
                        for path in &event.paths {
                            if !path
                                .components()
                                .any(|c| IGNORED_SEGMENTS.iter().any(|s| c.as_os_str() == *s))
                            {
                                let _ = tx.send(path.clone());
                            }
                        }
                    })
                })
                .collect()
        })
        .await
        {
            Ok(handles) => handles,
            Err(_) => return,
        };
        let _handles = handles;
        loop {
            let Some(first) = rx.recv().await else {
                return;
            };
            let mut changed = BTreeSet::from([first]);
            let burst_deadline = tokio::time::Instant::now() + MAX_DEBOUNCE;
            loop {
                tokio::select! {
                    path = rx.recv() => match path {
                        Some(path) => { changed.insert(path); }
                        None => return,
                    },
                    _ = tokio::time::sleep(debounce) => break,
                    _ = tokio::time::sleep_until(burst_deadline) => break,
                }
            }
            if let Err(err) = self
                .fire_file_change(&agent_id, &watch_key, changed, debounce)
                .await
            {
                tracing::warn!(agent = %agent_id, %err, "agents: file-change fire failed");
            }
        }
    }

    async fn fire_file_change(
        self: &Arc<Self>,
        agent_id: &str,
        watch_key: &str,
        changed: BTreeSet<PathBuf>,
        debounce: Duration,
    ) -> Result<(), EngineError> {
        let Some(agent) = self.store.agent(agent_id)? else {
            return Ok(());
        };
        if !agent.draft.enabled || agent.paused_reason.is_some() {
            return Ok(());
        }
        let now = self.clock.now();
        // Changes during (or just after) the agent's own run are most likely
        // its own edits: drop them silently rather than queue a skip row.
        if self.store.active_run(agent_id)?.is_some() {
            return Ok(());
        }
        let settle =
            chrono::TimeDelta::from_std(debounce + Duration::from_secs(2)).unwrap_or_default();
        if let Some(last) = self.store.latest_run(agent_id)?
            && last.trigger == AgentTriggerKind::FileWatch
            && last.finished_at.is_some_and(|at| now - at < settle)
        {
            return Ok(());
        }
        let root = PathBuf::from(&agent.workspace_path);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let changed_paths = changed
            .iter()
            .take(50)
            .map(|path| {
                path.strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let key = watch_key.split_once(':').map_or(watch_key, |(_, key)| key);
        self.fire(Fire {
            run_key: format!("{agent_id}:{key}:{}", now.timestamp_millis()),
            agent,
            kind: AgentTriggerKind::FileWatch,
            scheduled_at: now,
            collapsed: 0,
            webhook: None,
            changed_paths,
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_keys_track_content() {
        let a = AgentTrigger::Interval { every_secs: 600 };
        let b = AgentTrigger::Interval { every_secs: 900 };
        assert_eq!(trigger_key(&a), trigger_key(&a.clone()));
        assert_ne!(trigger_key(&a), trigger_key(&b));
        assert_eq!(trigger_key(&a).len(), 16);
    }

    #[test]
    fn secrets_compare_exactly() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
    }
}
