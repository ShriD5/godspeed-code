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
//! - **Resume.** At the reset a scheduled resume durably queues one
//!   continuation prompt at the head of the queue — the failed turn's own
//!   message is already in the session and is not sent again — then clears
//!   the marker, and the held messages follow it. A resume goes back to
//!   asking instead when it is found long after its reset (the machine was
//!   off), the chat was archived or stopped, or the harness is signed in to
//!   a different account than the one the limit hit; it never switches
//!   accounts itself.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::{
    AgentUsageWindow, Chat, ChatLimit, HarnessId, LifecycleState, LimitLogin,
    LimitRecovery as Recovery, LimitRecoveryAction, LimitRecoverySettings, LimitResetSource,
};

use crate::EngineError;
use crate::agent_accounts::AgentAccounts;
use crate::agents::{Clock, SystemClock};
use crate::device_settings;
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

/// What a harness is signed in with right now.
#[derive(Debug, Clone, Default)]
pub struct HarnessLogin {
    /// [`login_fingerprint`] of the active account(s); `None` without one.
    pub fingerprint: Option<String>,
    pub usage_windows: Vec<AgentUsageWindow>,
}

/// The source of each harness's active login: the device's agent accounts.
#[async_trait]
pub trait ActiveLogins: Send + Sync {
    async fn active_login(&self, harness: HarnessId) -> Result<HarnessLogin, EngineError>;
}

#[async_trait]
impl ActiveLogins for AgentAccounts {
    async fn active_login(&self, harness: HarnessId) -> Result<HarnessLogin, EngineError> {
        let snapshot = self.list(false).await?;
        let active: Vec<_> = snapshot
            .accounts
            .into_iter()
            .filter(|account| account.harness == harness && account.active)
            .collect();
        Ok(HarnessLogin {
            fingerprint: login_fingerprint(active.iter().map(|account| account.id.as_str())),
            usage_windows: active
                .into_iter()
                .flat_map(|account| account.usage_windows)
                .collect(),
        })
    }
}

/// An opaque digest of a set of account ids, safe to sync on the chat row.
pub fn login_fingerprint<'a>(ids: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut ids: Vec<&str> = ids.collect();
    if ids.is_empty() {
        return None;
    }
    ids.sort_unstable();
    let digest = Sha256::digest(ids.join("\n").as_bytes());
    Some(digest[..12].iter().map(|b| format!("{b:02x}")).collect())
}

/// The queue row id of the continuation that resumes `limit`: one per limit.
pub fn resume_row_id(limit: &ChatLimit) -> String {
    format!("limit-resume-{}", limit.limited_at.timestamp_millis())
}

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
    logins: Mutex<Option<Arc<dyn ActiveLogins>>>,
    auto_filter: OnceLock<AutoResumeFilter>,
    clock: Arc<dyn Clock>,
    /// Chats whose continuation this service sent: a limit on that turn
    /// keeps the resume the user chose (when it names a fresh reset).
    resumed: Mutex<HashSet<String>>,
    /// One tick at a time: two overlapping ticks could both resume a chat.
    ticking: tokio::sync::Mutex<()>,
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
        let settings = device_settings::load(&settings_path);
        Self {
            inner: Arc::new(Inner {
                settings_path,
                settings: Mutex::new(settings),
                device_id: workspace.device_id().to_string(),
                workspace,
                doc_host,
                logins: Mutex::new(None),
                auto_filter: OnceLock::new(),
                clock,
                resumed: Mutex::new(HashSet::new()),
                ticking: tokio::sync::Mutex::new(()),
                wake: Notify::new(),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// The logins limits bind to, and the usage meters an unknown reset
    /// falls back to.
    pub fn set_logins(&self, logins: Arc<dyn ActiveLogins>) {
        *lock(&self.inner.logins) = Some(logins);
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
        device_settings::save(&self.inner.settings_path, &settings)?;
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
            login: None,
        };
        if let Err(err) = inner.workspace.set_chat_limit(chat_id, Some(&limit)) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: marker not written");
            return;
        }
        tracing::info!(chat = %chat_id, reset = ?limit.reset_at, "chat is usage-limited");
        let task = inner.clone();
        let chat_id = chat_id.to_owned();
        self.inner.tasks.spawn(async move {
            tokio::select! {
                _ = task.cancel.cancelled() => {}
                _ = task.bind_login(&chat_id, signal.harness, now) => {}
            }
        });
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

    /// A banner choice. Returns the chat's marker afterwards. Choosing to
    /// resume now (or a marker that never learnt its login) binds the
    /// resume to the login active at this moment: the user has decided.
    pub async fn set_recovery(
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
        let Some(limit) = chat.limit else {
            return Ok(None);
        };
        let recovery = match action {
            LimitRecoveryAction::Dismiss => {
                inner.clear(chat_id);
                inner.follow_limit_snooze(chat_id, &limit, None);
                return Ok(None);
            }
            LimitRecoveryAction::Ask => Recovery::Ask,
            LimitRecoveryAction::Resume => Recovery::ResumeAtReset,
            LimitRecoveryAction::Snooze => Recovery::Snoozed,
        };
        if recovery != Recovery::Ask && reset_at.is_none() && limit.reset_at.is_none() {
            return Err(failed("No reset time is known yet: pick when to resume"));
        }
        let now = inner.clock.now();
        let rebind = recovery == Recovery::ResumeAtReset
            && (limit.login.is_none() || reset_at.is_some_and(|at| at <= now));
        let login = if rebind {
            inner.current_login(chat_id, &limit).await
        } else {
            None
        };
        let updated = inner.workspace.update_chat_limit(chat_id, |current| {
            let mut next = current.clone();
            next.recovery = recovery;
            if recovery != Recovery::Ask
                && let Some(at) = reset_at
            {
                next.reset_at = Some(at);
                next.reset_source = Some(LimitResetSource::User);
            }
            if let Some(login) = &login {
                next.login = Some(login.clone());
            }
            Some(next)
        })?;
        inner.follow_limit_snooze(chat_id, &limit, updated.as_ref());
        inner.wake.notify_one();
        Ok(updated)
    }

    /// Act on every due marker at `now`; returns the next reset to wake
    /// for. The timer calls this with the wall clock; tests call it directly.
    #[doc(hidden)]
    pub async fn tick_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.inner.tick(now).await
    }
}

impl Inner {
    fn auto_resume(&self, chat_id: &str) -> bool {
        lock(&self.settings).auto_resume && self.auto_filter.get().is_none_or(|f| f(chat_id))
    }

    /// Clear the marker and let anything held go out, unless a Stop holds it.
    fn clear(&self, chat_id: &str) {
        self.doc_host.freeze_if_stopped(chat_id);
        if let Err(err) = self.workspace.set_chat_limit(chat_id, None) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: marker not cleared");
            return;
        }
        self.doc_host.release_queue(chat_id);
    }

    /// "Snooze until reset" snoozes the chat itself until that reset; any
    /// other choice afterwards wakes a snooze that was only waiting for it.
    fn follow_limit_snooze(&self, chat_id: &str, before: &ChatLimit, after: Option<&ChatLimit>) {
        let snoozed_until = |limit: &ChatLimit| {
            limit
                .reset_at
                .filter(|_| limit.recovery == Recovery::Snoozed)
        };
        let now = self.clock.now();
        let changed = match (after.and_then(snoozed_until), snoozed_until(before)) {
            (Some(until), _) => self.workspace.change_chat_lifecycle(
                chat_id,
                LifecycleState::Snoozed { until },
                None,
                now,
            ),
            (None, Some(until)) => self.workspace.change_chat_lifecycle(
                chat_id,
                LifecycleState::Active,
                Some(LifecycleState::Snoozed { until }),
                now,
            ),
            (None, None) => return,
        };
        if let Err(err) = changed {
            tracing::debug!(chat = %chat_id, %err, "limit recovery: chat snooze not followed");
        }
    }

    fn logins(&self) -> Option<Arc<dyn ActiveLogins>> {
        lock(&self.logins).clone()
    }

    async fn active_login(&self, harness: HarnessId) -> Result<HarnessLogin, EngineError> {
        match self.logins() {
            Some(logins) => logins.active_login(harness).await,
            None => Ok(HarnessLogin::default()),
        }
    }

    /// The login the chat's harness is on now, when it can be read.
    async fn current_login(&self, chat_id: &str, limit: &ChatLimit) -> Option<LimitLogin> {
        let harness = limit
            .login
            .as_ref()
            .map(|login| login.harness)
            .or_else(|| self.workspace.chat_config(chat_id).map(|c| c.harness))?;
        let login = self.active_login(harness).await.ok()?;
        Some(LimitLogin {
            harness,
            fingerprint: login.fingerprint,
        })
    }

    /// Record which login the limit at `limited_at` hit and, when no reset
    /// is known, fall back to that account's spent usage window.
    async fn bind_login(&self, chat_id: &str, harness: HarnessId, limited_at: DateTime<Utc>) {
        let login = match self.active_login(harness).await {
            Ok(login) => login,
            Err(err) => {
                tracing::debug!(chat = %chat_id, %err, "limit recovery: login unavailable");
                return;
            }
        };
        let usage_reset = exhausted_window_reset(&login.usage_windows, self.clock.now());
        let auto = self.auto_resume(chat_id);
        let updated = self.workspace.update_chat_limit(chat_id, |limit| {
            if limit.limited_at != limited_at {
                return None;
            }
            let mut next = limit.clone();
            next.login.get_or_insert(LimitLogin {
                harness,
                fingerprint: login.fingerprint,
            });
            if next.reset_at.is_none()
                && let Some(reset) = usage_reset
            {
                next.reset_at = Some(reset);
                next.reset_source = Some(LimitResetSource::UsageWindow);
                if auto {
                    next.recovery = Recovery::ResumeAtReset;
                }
            }
            (next != *limit).then_some(next)
        });
        if let Err(err) = updated {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: login not recorded");
        }
        self.wake.notify_one();
    }

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

    async fn tick(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let _ticking = self.ticking.lock().await;
        let chats = match self.workspace.read_chats() {
            Ok(chats) => chats,
            Err(err) => {
                tracing::warn!(%err, "limit recovery: chats not read");
                return None;
            }
        };
        let mut earliest: Option<DateTime<Utc>> = None;
        for chat in chats.into_iter().filter(|c| c.device_id == self.device_id) {
            let Some(limit) = chat.limit.clone() else {
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
                Recovery::ResumeAtReset => self.resume_due(&chat, &limit, now).await,
                _ if chat.archived => {}
                _ => self.release_due(&chat.id, &limit).await,
            }
        }
        earliest
    }

    fn has_queue(&self, chat_id: &str) -> bool {
        self.doc_host.queue_len(chat_id) > 0
    }

    /// A snoozed or asking marker past its reset. Messages the user queued
    /// while limited continue the session themselves once the window is
    /// back, on the same login only; an agent run's chat has nothing to ask
    /// (its agent fires again on its own).
    async fn release_due(&self, chat_id: &str, limit: &ChatLimit) {
        let agent_run = self.auto_filter.get().is_some_and(|f| !f(chat_id));
        if limit.recovery == Recovery::Ask && !agent_run && !self.has_queue(chat_id) {
            return;
        }
        if agent_run || self.same_login(limit).await {
            self.clear(chat_id);
        } else if limit.recovery == Recovery::Snoozed {
            self.ask(chat_id, limit, "the harness is on another login");
        }
    }

    async fn resume_due(&self, chat: &Chat, limit: &ChatLimit, now: DateTime<Utc>) {
        match self.resume_blocker(chat, limit, now).await {
            Some(reason) => self.ask(&chat.id, limit, reason),
            None => self.resume(&chat.id, limit),
        }
    }

    /// Why a due resume must not fire on its own, if anything.
    async fn resume_blocker(
        &self,
        chat: &Chat,
        limit: &ChatLimit,
        now: DateTime<Utc>,
    ) -> Option<&'static str> {
        if limit
            .reset_at
            .is_some_and(|reset| now - reset > RESUME_STALE)
        {
            return Some("the reset passed hours ago");
        }
        if chat.archived {
            return Some("the chat is archived");
        }
        if !self.same_login(limit).await {
            return Some("the harness is on another login");
        }
        None
    }

    async fn same_login(&self, limit: &ChatLimit) -> bool {
        let Some(bound) = &limit.login else {
            return false;
        };
        self.active_login(bound.harness)
            .await
            .is_ok_and(|now| now.fingerprint == bound.fingerprint)
    }

    /// Turn a due resume or snooze back into a question for the banner,
    /// taking back a continuation an interrupted resume left queued.
    fn ask(&self, chat_id: &str, limit: &ChatLimit, reason: &str) {
        tracing::info!(chat = %chat_id, reason, "limit recovery: not resuming on its own; asking");
        if let Err(err) = self
            .doc_host
            .withdraw_limit_resume(chat_id, &resume_row_id(limit))
        {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: continuation not withdrawn");
            return;
        }
        let asking = self.workspace.update_chat_limit(chat_id, |current| {
            (current.limited_at == limit.limited_at && current.recovery != Recovery::Ask).then(
                || ChatLimit {
                    recovery: Recovery::Ask,
                    ..current.clone()
                },
            )
        });
        if let Err(err) = asking {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: asking not recorded");
        }
    }

    /// Queue the continuation ahead of anything held, then clear the marker.
    /// The marker is the only record that a resume is owed, so it outlives
    /// the queue write: a crash or failure in between retries on the next
    /// tick, and the row's receipt keeps the retry from sending it twice.
    fn resume(&self, chat_id: &str, limit: &ChatLimit) {
        if let Err(err) =
            self.doc_host
                .queue_limit_resume(chat_id, &resume_row_id(limit), RESUME_PROMPT)
        {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: continuation not queued");
            return;
        }
        lock(&self.resumed).insert(chat_id.to_owned());
        if let Err(err) = self.workspace.set_chat_limit(chat_id, None) {
            tracing::warn!(chat = %chat_id, %err, "limit recovery: marker not cleared");
            return;
        }
        tracing::info!(chat = %chat_id, "limit recovery: resuming at reset");
        self.doc_host.resume_queue(chat_id);
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
