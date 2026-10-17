//! Telegram bot for Godspeed's always-on agents, run by the engine so a
//! headless VPS serves it too.
//!
//! - **Linking.** Settings shows a one-time code; `/link CODE` from a
//!   private chat links it. Only linked private chats are served.
//! - **Threads.** Each linked chat has a current Godspeed chat. Text
//!   continues it (the first message starts one with the chat's defaults),
//!   a reply to one of the bot's answers continues that answer's chat, and
//!   `/new` starts over. Sends go through the same doc-host paths as the
//!   composer: a Run command when idle, the chat's queue while a turn runs.
//! - **Schedules.** `/schedule …` creates an ordinary always-on agent with
//!   the chat's defaults, in the chat's time zone; its run results come back
//!   to the chat.
//! - **Approvals.** A Telegram thread or an agent run that stops for an
//!   approval or a question gets a card with the exact command, paths or
//!   diff summary and one button per decision. The answer goes through the
//!   command ledger (`RespondInput`) like any other client's; nothing is
//!   ever answered on the user's behalf. More than three in a minute from
//!   agents fold into one digest with a button row per item.
//! - **One poller.** The token lives in an owner-only file and never leaves
//!   the engine. A single long-poll loop per engine restarts on token
//!   changes and backs off on errors.

mod api;
mod bot;
mod callback;
mod command;
mod delivery;
mod digest;
mod link;
mod pickers;
mod poller;
mod prompts;
mod render;
mod schedule_syntax;
mod schedules;
mod store;
mod threads;
mod token;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::telegram::{
    TelegramConnection, TelegramDefaults, TelegramLinkCode, TelegramLinkedChat, TelegramStatus,
};

use crate::EngineError;
use crate::agents::{Agents, Clock, SystemClock};
use crate::doc_host::DocHost;
use crate::registry::HarnessRegistry;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

use api::BotApi;
use link::{Claim, LinkCodes};
use store::TelegramStore;
use token::{BotToken, TokenFile};

/// Overrides the Bot API base URL (a self-hosted Bot API server).
const API_BASE_ENV: &str = "GODSPEED_TELEGRAM_API_BASE";
const POLL_TIMEOUT: Duration = Duration::from_secs(25);

/// The engine services the bot drives.
#[derive(Clone)]
pub struct TelegramDeps {
    pub workspace: WorkspaceHost,
    pub doc_host: DocHost,
    pub sessions: SessionsEngine,
    pub registry: Arc<HarnessRegistry>,
    pub agents: Agents,
}

pub struct TelegramOptions {
    pub api_base: String,
    /// How long one `getUpdates` long poll waits.
    pub poll_timeout: Duration,
    pub clock: Arc<dyn Clock>,
}

impl Default for TelegramOptions {
    fn default() -> Self {
        Self {
            api_base: std::env::var(API_BASE_ENV)
                .ok()
                .filter(|base| !base.trim().is_empty())
                .unwrap_or_else(|| api::DEFAULT_API_BASE.to_string()),
            poll_timeout: POLL_TIMEOUT,
            clock: Arc::new(SystemClock),
        }
    }
}

#[derive(Clone)]
pub struct Telegram {
    inner: Arc<Inner>,
}

struct Inner {
    store: TelegramStore,
    token_file: TokenFile,
    options: TelegramOptions,
    api_base: Mutex<String>,
    deps: TelegramDeps,
    /// The engine's device data dir (model catalogs cache under it).
    data_dir: PathBuf,
    device_id: String,
    link_codes: Mutex<LinkCodes>,
    token: Mutex<Option<BotToken>>,
    /// The client while connected; deliveries go out through it.
    api: Mutex<Option<BotApi>>,
    connection: Mutex<TelegramConnection>,
    /// Bumped on every change the Settings section repaints for.
    revision: watch::Sender<u64>,
    token_changed: Notify,
    wake_delivery: Notify,
    /// Last typing indicator per Telegram chat.
    typing: Mutex<HashMap<i64, tokio::time::Instant>>,
    /// When recent cards and digests went out, per Telegram chat.
    card_sends: Mutex<HashMap<i64, Vec<chrono::DateTime<chrono::Utc>>>>,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

/// This host's IANA zone, the default for `/schedule` times.
fn host_timezone() -> String {
    std::env::var("TZ")
        .ok()
        .or_else(|| {
            let target = std::fs::read_link("/etc/localtime").ok()?;
            let target = target.to_string_lossy();
            target
                .split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_string())
        })
        .filter(|zone| zone.parse::<chrono_tz::Tz>().is_ok())
        .unwrap_or_else(|| "UTC".into())
}

impl Telegram {
    pub fn open(data_dir: &Path, deps: TelegramDeps) -> Result<Self, EngineError> {
        Self::open_with(data_dir, deps, TelegramOptions::default())
    }

    pub fn open_with(
        data_dir: &Path,
        deps: TelegramDeps,
        options: TelegramOptions,
    ) -> Result<Self, EngineError> {
        let store = TelegramStore::open(data_dir)?;
        let token_file = TokenFile::new(data_dir);
        let token = token_file.load();
        let connection = if token.is_some() {
            TelegramConnection::Connecting
        } else {
            TelegramConnection::Off
        };
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                token_file,
                api_base: Mutex::new(options.api_base.clone()),
                options,
                device_id: deps.workspace.device_id().to_string(),
                data_dir: data_dir.to_path_buf(),
                deps,
                link_codes: Mutex::new(LinkCodes::default()),
                token: Mutex::new(token),
                api: Mutex::new(None),
                connection: Mutex::new(connection),
                revision: watch::channel(0).0,
                token_changed: Notify::new(),
                wake_delivery: Notify::new(),
                typing: Mutex::new(HashMap::new()),
                card_sends: Mutex::new(HashMap::new()),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        })
    }

    /// Start the poller and the delivery loop. Needs a tokio runtime.
    pub fn start(&self) {
        let inner = &self.inner;
        inner.tasks.spawn(inner.clone().supervise());
        inner.tasks.spawn(inner.clone().deliver_loop());
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.tasks.wait()).await;
    }

    /// Revision counter for `WatchTelegram`.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.inner.revision.subscribe()
    }

    pub fn status(&self) -> Result<TelegramStatus, EngineError> {
        let inner = &self.inner;
        let now = inner.options.clock.now();
        Ok(TelegramStatus {
            token_set: lock(&inner.token).is_some(),
            connection: lock(&inner.connection).clone(),
            link_code: lock(&inner.link_codes).current(now),
            linked_chats: inner
                .store
                .linked_chats()?
                .into_iter()
                .map(|chat| TelegramLinkedChat {
                    chat_id: chat.chat_id,
                    label: chat.label,
                    linked_at: chat.linked_at,
                    timezone: chat.timezone,
                    defaults: chat.defaults,
                })
                .collect(),
            defaults: inner.store.global_defaults()?,
        })
    }

    /// Point the bot at another Bot API server (a self-hosted one, or a
    /// test double); takes effect on the next connect, which this starts.
    pub fn set_api_base(&self, base: &str) {
        *lock(&self.inner.api_base) = base.to_string();
        self.inner.token_changed.notify_one();
    }

    pub fn set_token(&self, raw: &str) -> Result<TelegramStatus, EngineError> {
        let token = BotToken::parse(raw).map_err(failed)?;
        self.inner.token_file.save(&token)?;
        self.inner.replace_token(Some(token));
        self.status()
    }

    pub fn clear_token(&self) -> Result<TelegramStatus, EngineError> {
        self.inner.token_file.clear()?;
        self.inner.replace_token(None);
        self.status()
    }

    pub fn create_link_code(&self) -> TelegramLinkCode {
        let code = lock(&self.inner.link_codes).issue(self.inner.options.clock.now());
        self.inner.changed();
        code
    }

    pub fn unlink(&self, chat_id: i64) -> Result<TelegramStatus, EngineError> {
        if self.inner.store.unlink(chat_id)? {
            self.inner.changed();
        }
        self.status()
    }

    pub fn set_defaults(&self, defaults: TelegramDefaults) -> Result<TelegramStatus, EngineError> {
        let defaults = self.inner.validate_defaults(defaults)?;
        self.inner.store.set_global_defaults(&defaults)?;
        self.inner.changed();
        self.status()
    }
}

impl Inner {
    fn changed(&self) {
        self.revision.send_modify(|rev| *rev += 1);
    }

    fn set_connection(&self, connection: TelegramConnection) {
        let mut current = lock(&self.connection);
        if *current != connection {
            *current = connection;
            drop(current);
            self.changed();
        }
    }

    fn replace_token(&self, token: Option<BotToken>) {
        *lock(&self.api) = None;
        let connection = if token.is_some() {
            TelegramConnection::Connecting
        } else {
            TelegramConnection::Off
        };
        *lock(&self.token) = token;
        self.set_connection(connection);
        self.token_changed.notify_one();
        self.changed();
    }

    fn api(&self) -> Option<BotApi> {
        lock(&self.api).clone()
    }

    fn claim_link_code(&self, code: &str) -> Claim {
        let claim = lock(&self.link_codes).claim(code, self.options.clock.now());
        self.changed();
        claim
    }

    /// Drop defaults that no longer resolve on this device.
    fn validate_defaults(
        &self,
        mut defaults: TelegramDefaults,
    ) -> Result<TelegramDefaults, EngineError> {
        if let Some(harness) = defaults.harness
            && !self.available_harnesses().iter().any(|h| h.id == harness)
        {
            return Err(failed(format!(
                "{harness:?} is not available on this device"
            )));
        }
        if let Some(space_id) = &defaults.space_id
            && self.local_space(space_id).is_none()
        {
            return Err(failed("That project is not on this device"));
        }
        defaults.model = defaults.model.filter(|m| !m.trim().is_empty());
        if let (Some(harness), Some(mode)) = (defaults.harness, defaults.access_mode)
            && !zeron_proto::access_mode_supported(harness, mode)
        {
            return Err(failed(format!("{harness:?} can't run in {}", mode.label())));
        }
        Ok(defaults)
    }
}
