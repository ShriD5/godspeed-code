//! Thread lifecycle timers for the chats this engine hosts: a snoozed chat
//! wakes when its snooze ends, and, when the device opts in, an idle chat
//! settles after a number of days without activity.
//!
//! Like usage-limit recovery, nothing about the schedule lives in memory:
//! every tick re-derives it from the registry rows, re-reading the wall clock
//! at least every [`MAX_SLEEP`] because tokio's monotonic timers stop while a
//! laptop sleeps. A restart or a night asleep needs no restoring, and a
//! snooze that ended meanwhile wakes on the first tick. Clients project an
//! ended snooze as active on their own; the wake makes it durable and synced.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::{
    Chat, ChatIndicator, ChatLifecycle, ChatLifecycleSettings, LifecycleState, Session,
};

use crate::EngineError;
use crate::agents::{Clock, SystemClock};
use crate::device_settings;
use crate::workspace_host::WorkspaceHost;

/// Longest the timer trusts one sleep (see the module docs).
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);
const SETTINGS_FILE: &str = "chat-lifecycle.json";

#[derive(Clone)]
pub struct ChatLifecycleService {
    inner: Arc<Inner>,
}

struct Inner {
    settings_path: PathBuf,
    settings: Mutex<ChatLifecycleSettings>,
    device_id: String,
    workspace: WorkspaceHost,
    clock: Arc<dyn Clock>,
    wake: Notify,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ChatLifecycleService {
    pub fn open(data_dir: &Path, workspace: WorkspaceHost) -> Self {
        Self::open_with_clock(data_dir, workspace, Arc::new(SystemClock))
    }

    pub fn open_with_clock(
        data_dir: &Path,
        workspace: WorkspaceHost,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let settings_path = data_dir.join(SETTINGS_FILE);
        Self {
            inner: Arc::new(Inner {
                settings: Mutex::new(device_settings::load(&settings_path)),
                settings_path,
                device_id: workspace.device_id().to_string(),
                workspace,
                clock,
                wake: Notify::new(),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
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

    /// Re-plan now: a lifecycle or setting changed.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    pub fn settings(&self) -> ChatLifecycleSettings {
        *lock(&self.inner.settings)
    }

    pub fn set_settings(
        &self,
        settings: ChatLifecycleSettings,
    ) -> Result<ChatLifecycleSettings, EngineError> {
        device_settings::save(&self.inner.settings_path, &settings)?;
        *lock(&self.inner.settings) = settings;
        self.wake();
        Ok(settings)
    }

    /// Act on everything due at `now`; returns the next deadline to wake
    /// for. The timer calls this with the wall clock; tests call it directly.
    #[doc(hidden)]
    pub fn tick_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.inner.tick(now)
    }
}

impl Inner {
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
                tracing::warn!(%err, "chat lifecycle: chats not read");
                return None;
            }
        };
        let settle_after = lock(&self.settings)
            .auto_settle_after_days
            .map(|days| TimeDelta::days(i64::from(days)));
        let sessions: HashMap<String, Session> = self
            .workspace
            .read_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|session| (session.chat_id.clone(), session))
            .collect();
        let pinned: HashSet<String> = self.workspace.pinned_chat_ids().into_iter().collect();
        let mut earliest: Option<DateTime<Utc>> = None;
        let mut wake_at = |at: DateTime<Utc>| {
            earliest = Some(earliest.map_or(at, |cur: DateTime<Utc>| cur.min(at)));
        };
        let hosted = chats.iter().filter(|chat| {
            chat.device_id == self.device_id && chat.is_top_level() && !chat.archived
        });
        for chat in hosted {
            match chat.lifecycle.state {
                LifecycleState::Snoozed { until } if until <= now => {
                    self.replace(chat, LifecycleState::Active, now);
                }
                LifecycleState::Snoozed { until } => wake_at(until),
                LifecycleState::Active => {
                    let Some(after) = settle_after else {
                        continue;
                    };
                    if pinned.contains(&chat.id) || !idle(chat, sessions.get(&chat.id), now) {
                        continue;
                    }
                    let due = last_activity(chat) + after;
                    if due <= now {
                        self.replace(chat, LifecycleState::Settled, now);
                    } else {
                        wake_at(due);
                    }
                }
                LifecycleState::Settled => {}
            }
        }
        earliest
    }

    /// Move `chat` to `state` unless its lifecycle changed since it was read.
    fn replace(&self, chat: &Chat, state: LifecycleState, now: DateTime<Utc>) {
        let next = ChatLifecycle::new(state, now);
        match self
            .workspace
            .replace_chat_lifecycle(&chat.id, &chat.lifecycle, &next)
        {
            Ok(true) => tracing::debug!(chat = %chat.id, ?state, "chat lifecycle changed"),
            Ok(false) => {}
            Err(err) => tracing::warn!(chat = %chat.id, %err, "chat lifecycle not written"),
        }
    }
}

/// Nothing to look at: not unread, failed, limited, running or waiting.
fn idle(chat: &Chat, session: Option<&Session>, now: DateTime<Utc>) -> bool {
    zeron_proto::view::display_status(chat, session, now) == ChatIndicator::Idle
}

/// The later of the last message (or creation) and the last lifecycle change.
fn last_activity(chat: &Chat) -> DateTime<Utc> {
    let message = chat.last_message_at.unwrap_or(chat.created_at);
    chat.lifecycle
        .changed_at
        .map_or(message, |changed| changed.max(message))
}
