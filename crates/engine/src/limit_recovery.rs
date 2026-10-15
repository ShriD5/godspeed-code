//! Usage-limit recovery: a chat whose turn the provider refused for a usage
//! or rate limit becomes Limited, and resumes the same session on the same
//! account once the window resets — on the headless engine too.
//!
//! - **Marker.** The sessions engine reports a limited turn
//!   ([`LimitSignal`]); this module writes the chat's [`ChatLimit`] onto its
//!   registry row, so the Limited status persists and syncs like the rest of
//!   the row. An unknown reset falls back to the exhausted window of the
//!   account's usage meters (t3code #14169), else the banner asks.
//! - **Holding.** While the marker holds ([`ChatLimit::holds_sends`]), sends
//!   and queued messages wait in the chat's queue instead of running into
//!   the limit (`DocHost::limit_holds`).
//! - **Timer.** One task sleeps until the earliest reset, re-reading the wall
//!   clock at least every [`MAX_SLEEP`] because tokio's monotonic timers stop
//!   while a laptop sleeps. Nothing about the schedule lives in memory: every
//!   tick re-derives it from the markers, so restarts need no restoration.
//! - **Resume.** At the reset a scheduled resume clears the marker and puts
//!   one continuation prompt at the head of the queue — the failed turn's
//!   own message is already in the session and is not sent again — then the
//!   held messages follow it. A resume found long after its reset (the
//!   machine was off) goes back to asking instead of firing stale work.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::{
    ChatLimit, HarnessId, LimitRecovery as Recovery, LimitRecoveryAction, LimitRecoverySettings,
    LimitResetSource,
};

use crate::EngineError;
use crate::agent_accounts::AgentAccounts;
use crate::agents::{Clock, SystemClock};
use crate::doc_host::DocHost;
use crate::workspace_host::WorkspaceHost;

/// Longest the timer trusts one sleep (see the module docs).
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);
/// A scheduled resume this late goes back to asking.
const RESUME_STALE: TimeDelta = TimeDelta::hours(12);
/// A usage window counts as spent at this fraction.
const EXHAUSTED: f32 = 0.995;
const SETTINGS_FILE: &str = "limit-recovery.json";
/// The continuation a resume sends into the same session.
pub const RESUME_PROMPT: &str = "Continue where you left off.";

/// A turn the provider refused for a usage limit, as the sessions engine
/// saw it end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitSignal {
    pub reset_at: Option<DateTime<Utc>>,
    pub window: Option<String>,
    pub harness: HarnessId,
}

/// Which chats a newly Limited marker may arm a resume for automatically
/// (always-on agent runs never do: their next fire replaces the run).
pub type AutoResumeFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct LimitRecoveryService {
    inner: Arc<Inner>,
}

struct Inner {
    settings_path: PathBuf,
    settings: Mutex<LimitRecoverySettings>,
    device_id: String,
    workspace: WorkspaceHost,
    doc_host: DocHost,
    accounts: OnceLock<AgentAccounts>,
    auto_filter: OnceLock<AutoResumeFilter>,
    clock: Arc<dyn Clock>,
    /// Chats whose continuation this service sent: a limit on that turn
    /// keeps the resume the user chose (when it names a fresh reset).
    resumed: Mutex<HashSet<String>>,
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

/// The latest reset among the account's spent windows, when one is ahead.
pub fn exhausted_window_reset(
    windows: &[zeron_proto::AgentUsageWindow],
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    windows
        .iter()
        .filter(|w| w.used_fraction >= EXHAUSTED)
        .filter_map(|w| w.resets_at)
        .filter(|at| *at > now)
        .max()
}

impl LimitRecoveryService {
    pub fn open(data_dir: &Path, workspace: WorkspaceHost, doc_host: DocHost) -> Self {
        Self::open_with_clock(data_dir, workspace, doc_host, Arc::new(SystemClock))
    }

    pub fn open_with_clock(
        data_dir: &Path,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let settings_path = data_dir.join(SETTINGS_FILE);
        let settings = std::fs::read(&settings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                settings_path,
                settings: Mutex::new(settings),
                device_id: workspace.device_id().to_string(),
                workspace,
                doc_host,
                accounts: OnceLock::new(),
                auto_filter: OnceLock::new(),
                clock,
                resumed: Mutex::new(HashSet::new()),
                wake: Notify::new(),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// The usage meters an unknown reset falls back to.
    pub fn set_accounts(&self, accounts: AgentAccounts) {
        let _ = self.inner.accounts.set(accounts);
    }

    pub fn set_auto_resume_filter(&self, filter: AutoResumeFilter) {
        let _ = self.inner.auto_filter.set(filter);
    }

    /// Start the timer. Needs a tokio runtime.
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

    pub fn settings(&self) -> LimitRecoverySettings {
        *lock(&self.inner.settings)
    }

    pub fn set_settings(
        &self,
        settings: LimitRecoverySettings,
    ) -> Result<LimitRecoverySettings, EngineError> {
        let bytes = serde_json::to_vec_pretty(&settings)
            .map_err(|err| failed(format!("limit recovery settings: {err}")))?;
        let tmp = self.inner.settings_path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.inner.settings_path)?;
        *lock(&self.inner.settings) = settings;
        Ok(settings)
    }

    /// A turn on `chat_id` ended on a usage limit: mark the chat Limited.
    /// Synchronous so the marker lands before the run's Errored status does
    /// (the always-on agents monitor reads it there).
    pub fn note_limited(&self, chat_id: &str, signal: LimitSignal) {
        let inner = &self.inner;
        let now = inner.clock.now();
        let reset = signal.reset_at.filter(|at| *at > now);
        // A continuation this service sent hit the limit again: keep the
        // resume the user chose, as long as the provider names a reset still
        // ahead (an already-passed window cannot start a retry loop).
        let carried = lock(&inner.resumed).remove(chat_id) && reset.is_some();
        let limit = ChatLimit {
            limited_at: now,
            reset_at: reset,
            reset_source: reset.map(|_| LimitResetSource::Provider),
            window: signal.window,
            recovery: if carried || (reset.is_some() && inner.auto_resume(chat_id)) {
                Recovery::ResumeAtReset
            } else {
                Recovery::Ask
            },
        };
        if let Err(err) = inner.workspace.set_chat_limit(chat_id, Some(&limit)) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: marker not written");
            return;
        }
        tracing::info!(chat = %chat_id, reset = ?limit.reset_at, "chat is usage-limited");
        if reset.is_none() {
            let inner = inner.clone();
            let chat_id = chat_id.to_owned();
            let harness = signal.harness;
            self.inner.tasks.spawn(async move {
                tokio::select! {
                    _ = inner.cancel.cancelled() => {}
                    _ = inner.fill_reset_from_usage(&chat_id, harness, limit) => {}
                }
            });
        }
        inner.wake.notify_one();
    }

    /// A turn started on `chat_id` (any dispatch: an explicit send, the
    /// continuation): whatever it was limited on, it is running again.
    pub fn note_turn_start(&self, chat_id: &str) {
        let limited = self
            .inner
            .workspace
            .chat(chat_id)
            .ok()
            .flatten()
            .is_some_and(|chat| chat.limit.is_some());
        if limited && let Err(err) = self.inner.workspace.set_chat_limit(chat_id, None) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: marker not cleared");
        }
    }

    /// A banner choice. Returns the chat's marker afterwards.
    pub fn set_recovery(
        &self,
        chat_id: &str,
        action: LimitRecoveryAction,
        reset_at: Option<DateTime<Utc>>,
    ) -> Result<Option<ChatLimit>, EngineError> {
        let inner = &self.inner;
        let chat = inner
            .workspace
            .chat(chat_id)?
            .ok_or_else(|| failed(format!("no such chat: {chat_id}")))?;
        let Some(mut limit) = chat.limit else {
            return Ok(None);
        };
        match action {
            LimitRecoveryAction::Dismiss => {
                inner.clear(chat_id);
                return Ok(None);
            }
            LimitRecoveryAction::Ask => limit.recovery = Recovery::Ask,
            LimitRecoveryAction::Resume | LimitRecoveryAction::Snooze => {
                if let Some(at) = reset_at {
                    limit.reset_at = Some(at);
                    limit.reset_source = Some(LimitResetSource::User);
                }
                if limit.reset_at.is_none() {
                    return Err(failed("No reset time is known yet: pick when to resume"));
                }
                limit.recovery = if action == LimitRecoveryAction::Resume {
                    Recovery::ResumeAtReset
                } else {
                    Recovery::Snoozed
                };
            }
        }
        inner.workspace.set_chat_limit(chat_id, Some(&limit))?;
        inner.wake.notify_one();
        Ok(Some(limit))
    }

    /// Act on every due marker at `now`; returns the next reset to wake
    /// for. The timer calls this with the wall clock; tests call it directly.
    #[doc(hidden)]
    pub fn tick_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.inner.tick(now)
    }
}

impl Inner {
    fn auto_resume(&self, chat_id: &str) -> bool {
        lock(&self.settings).auto_resume && self.auto_filter.get().is_none_or(|f| f(chat_id))
    }

    /// Clear the marker and let anything held go out.
    fn clear(&self, chat_id: &str) {
        if let Err(err) = self.workspace.set_chat_limit(chat_id, None) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: marker not cleared");
            return;
        }
        self.doc_host.release_queue(chat_id);
    }

    async fn fill_reset_from_usage(&self, chat_id: &str, harness: HarnessId, limit: ChatLimit) {
        let Some(accounts) = self.accounts.get() else {
            return;
        };
        let snapshot = match accounts.list(false).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::debug!(chat = %chat_id, %err, "limit recovery: usage meters unavailable");
                return;
            }
        };
        let now = self.clock.now();
        let Some(reset) = snapshot
            .accounts
            .iter()
            .filter(|account| account.harness == harness && account.active)
            .find_map(|account| exhausted_window_reset(&account.usage_windows, now))
        else {
            return;
        };
        // Only the marker this lookup was for: the chat may have resumed,
        // been dismissed or limited again meanwhile.
        let current = self
            .workspace
            .chat(chat_id)
            .ok()
            .flatten()
            .and_then(|c| c.limit);
        if current.as_ref() != Some(&limit) {
            return;
        }
        let mut limit = limit;
        limit.reset_at = Some(reset);
        limit.reset_source = Some(LimitResetSource::UsageWindow);
        if self.auto_resume(chat_id) {
            limit.recovery = Recovery::ResumeAtReset;
        }
        if let Err(err) = self.workspace.set_chat_limit(chat_id, Some(&limit)) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: usage reset not written");
        }
        self.wake.notify_one();
    }

    async fn run_loop(self: Arc<Self>) {
        loop {
            let next = self.tick(self.clock.now());
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

    fn tick(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let chats = match self.workspace.read_chats() {
            Ok(chats) => chats,
            Err(err) => {
                tracing::warn!(%err, "limit recovery: chats not read");
                return None;
            }
        };
        let mut earliest: Option<DateTime<Utc>> = None;
        for chat in chats.into_iter().filter(|c| c.device_id == self.device_id) {
            let Some(limit) = chat.limit else {
                continue;
            };
            let Some(reset) = limit.reset_at else {
                continue;
            };
            if !limit.due(now) {
                earliest = Some(earliest.map_or(reset, |cur| cur.min(reset)));
                continue;
            }
            match limit.recovery {
                Recovery::ResumeAtReset if now - reset > RESUME_STALE => {
                    // Misfire: the machine slept through the reset by hours.
                    // Stale work is not sent on its own; the banner offers it.
                    let mut asking = limit.clone();
                    asking.recovery = Recovery::Ask;
                    tracing::info!(chat = %chat.id, "limit recovery: resume missed by hours; asking");
                    if let Err(err) = self.workspace.set_chat_limit(&chat.id, Some(&asking)) {
                        tracing::warn!(chat = %chat.id, %err, "limit recovery: misfire not recorded");
                    }
                }
                Recovery::ResumeAtReset => self.resume(&chat.id, &limit),
                Recovery::Snoozed => self.clear(&chat.id),
                // Messages the user queued while limited continue the session
                // themselves once the window is back; an agent run's chat has
                // nothing to ask (its agent fires again on its own).
                Recovery::Ask
                    if self.has_queue(&chat.id)
                        || self.auto_filter.get().is_some_and(|f| !f(&chat.id)) =>
                {
                    self.clear(&chat.id)
                }
                Recovery::Ask => {}
            }
        }
        earliest
    }

    fn has_queue(&self, chat_id: &str) -> bool {
        self.doc_host.queue_len(chat_id) > 0
    }

    /// Clear the marker, then queue the continuation ahead of anything held.
    /// The row id is the marker's identity, so a repeat (a redelivered tick,
    /// a crash between the two writes) cannot send it twice.
    fn resume(&self, chat_id: &str, limit: &ChatLimit) {
        if let Err(err) = self.workspace.set_chat_limit(chat_id, None) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: resume not started");
            return;
        }
        lock(&self.resumed).insert(chat_id.to_owned());
        let row_id = format!("limit-resume-{}", limit.limited_at.timestamp_millis());
        match self
            .doc_host
            .queue_limit_resume(chat_id, &row_id, RESUME_PROMPT)
        {
            Ok(true) => tracing::info!(chat = %chat_id, "limit recovery: resuming at reset"),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, %err, "limit recovery: continuation not queued")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn exhausted_windows_name_the_latest_reset_ahead() {
        let now = Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap();
        let window = |used: f32, hours: i64| zeron_proto::AgentUsageWindow {
            label: "w".into(),
            used_fraction: used,
            resets_at: Some(now + TimeDelta::hours(hours)),
        };
        assert_eq!(
            exhausted_window_reset(&[window(1.0, 2), window(0.4, 1), window(1.0, 30)], now),
            Some(now + TimeDelta::hours(30))
        );
        assert_eq!(exhausted_window_reset(&[window(0.9, 2)], now), None);
        assert_eq!(exhausted_window_reset(&[window(1.0, -1)], now), None);
    }
}
