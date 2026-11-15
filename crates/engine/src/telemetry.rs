//! Anonymous usage telemetry: at most one small ping per UTC day to
//! `{edge}/telemetry/v1/ping` (payload: [`zeron_proto::TelemetryPing`],
//! docs/PRIVACY.md).
//!
//! - **Identity**: a random install id (UUID v4) in `telemetry.json` in the
//!   data dir, separate from the device id and never tied to an account.
//! - **When**: on launch (after a short delay) if today's ping hasn't gone
//!   out, else at the day's first activity (a turn started, an approval
//!   answered) in an engine that stayed up past midnight. An idle engine
//!   sends nothing.
//! - **What**: the platform and version, and the counts of the most recent
//!   earlier day not yet reported (a day is only complete once it's over).
//!   Turns, harness mix, tokens and spend come from the usage history's
//!   index of Godspeed's own turns ([`crate::UsageIndex::godspeed_between`]);
//!   agent logs from outside Godspeed are never read for it.
//! - **Failure**: retried with backoff, then dropped. Never blocks anything.
//! - **Off**: Settings → General (stored here, `enabled`), or
//!   `GODSPEED_TELEMETRY=0` / `DO_NOT_TRACK=1`. Off means nothing is sent;
//!   nothing remote is deleted.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use zeron_proto::{
    TELEMETRY_PATH, TELEMETRY_VERSION, TelemetryPing, TelemetrySettings, TelemetrySettingsUpdate,
    TelemetryTokens, TelemetryUsage, telemetry_arch, telemetry_env_off, telemetry_harness,
    telemetry_os,
};

use crate::{EngineError, UsageIndex};

/// The state file in the data dir.
pub const FILE_NAME: &str = "telemetry.json";

/// Delay before the launch ping, so a quick open-and-quit costs nothing and
/// boot work goes first.
const LAUNCH_DELAY: Duration = Duration::from_secs(60);
/// A first launch waits longer: time to read the notice and turn it off.
const FIRST_LAUNCH_DELAY: Duration = Duration::from_secs(5 * 60);
/// How often a running engine re-checks for a new day (activity also wakes
/// it at once).
const TICK: Duration = Duration::from_secs(15 * 60);
/// Waits between attempts; after the last one the day's ping is dropped.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(30 * 60),
];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Local day marks kept (only the latest unreported one is ever sent).
const KEEP_DAYS: i64 = 14;

/// What this engine noted about one UTC day.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct DayMark {
    /// A person used Godspeed that day.
    #[serde(default)]
    active: bool,
    #[serde(default)]
    approvals: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    install_id: String,
    first_seen: NaiveDate,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    notice_shown: bool,
    /// The UTC day the last ping went out (or was given up on).
    #[serde(default)]
    last_sent: Option<NaiveDate>,
    /// The latest day whose usage was reported.
    #[serde(default)]
    last_reported: Option<NaiveDate>,
    #[serde(default)]
    days: BTreeMap<NaiveDate, DayMark>,
}

fn default_true() -> bool {
    true
}

impl State {
    fn fresh(today: NaiveDate) -> Self {
        Self {
            install_id: uuid::Uuid::new_v4().to_string(),
            first_seen: today,
            enabled: true,
            notice_shown: false,
            last_sent: None,
            last_reported: None,
            days: BTreeMap::new(),
        }
    }

    /// Today's ping is owed: telemetry is on, the engine ran today and
    /// nothing went out yet.
    fn due(&self, today: NaiveDate) -> bool {
        self.enabled && self.days.contains_key(&today) && self.last_sent.is_none_or(|d| d < today)
    }

    /// The day the next ping reports: the latest earlier day this engine
    /// ran that hasn't been reported.
    fn usage_day(&self, today: NaiveDate) -> Option<NaiveDate> {
        self.days
            .keys()
            .rev()
            .copied()
            .find(|day| *day < today)
            .filter(|day| self.last_reported.is_none_or(|last| *day > last))
    }

    fn mark(&mut self, today: NaiveDate) -> &mut DayMark {
        self.days.entry(today).or_default()
    }

    /// Today's ping went out (`reported` = the usage day it carried) or was
    /// given up on (`reported` = `None`).
    fn mark_sent(&mut self, today: NaiveDate, reported: Option<NaiveDate>) {
        self.last_sent = Some(today);
        if let Some(day) = reported {
            self.last_reported = Some(self.last_reported.map_or(day, |last| last.max(day)));
        }
        let oldest = today - chrono::Duration::days(KEEP_DAYS);
        self.days.retain(|day, _| *day >= oldest);
    }
}

/// The counters only the ping itself needs, gathered when it is built.
pub type DayCounter = Arc<dyn Fn(DateTime<Utc>, DateTime<Utc>) -> u64 + Send + Sync>;

/// Where a day's usage comes from.
#[derive(Clone, Default)]
pub struct TelemetrySources {
    /// Godspeed's own turns (turns, harness mix, tokens, spend).
    pub usage: Option<UsageIndex>,
    /// Chats created on this device in `[from, to)`.
    pub chats_started: Option<DayCounter>,
    /// Zoomie runs started in `[from, to)`.
    pub zoomie_runs: Option<DayCounter>,
}

impl TelemetrySources {
    /// One UTC day's counts. Blocking (reads the usage index).
    pub fn collect(&self, day: NaiveDate, approvals: u64) -> TelemetryUsage {
        let (from, to) = utc_day(day);
        let turns = self
            .usage
            .as_ref()
            .and_then(|usage| {
                usage
                    .godspeed_between(from.timestamp_millis(), to.timestamp_millis())
                    .ok()
            })
            .unwrap_or_default();
        let mut harnesses = BTreeMap::new();
        for (harness, count) in &turns.by_harness {
            *harnesses
                .entry(telemetry_harness(harness).to_owned())
                .or_default() += count;
        }
        let count = |counter: &Option<DayCounter>| counter.as_ref().map_or(0, |f| f(from, to));
        TelemetryUsage {
            day,
            chats_started: count(&self.chats_started),
            turns_finished: turns.turns,
            approvals_answered: approvals,
            zoomie_runs: count(&self.zoomie_runs),
            harnesses,
            tokens: TelemetryTokens {
                input: turns.tokens.input,
                output: turns.tokens.output,
                cache_read: turns.tokens.cache_read,
                cache_write: turns.tokens.cache_write,
            },
            // Cents are plenty; four places keep tiny days from rounding to 0.
            spend_usd: (turns.cost_usd * 10_000.0).round() / 10_000.0,
        }
    }
}

/// `[midnight, next midnight)` of a UTC day.
fn utc_day(day: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight"));
    (start, start + chrono::Duration::days(1))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    path: PathBuf,
    state: Mutex<State>,
    /// The env var that turned telemetry off for this process, if any.
    env_off: Option<&'static str>,
    wake: tokio::sync::Notify,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// This install's telemetry: its state file, settings and sender.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

/// How one attempt went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attempt {
    Sent,
    /// Worth retrying (network error, 429, 5xx).
    Retry,
    /// Rejected for good (4xx) or telemetry was turned off meanwhile.
    Stop,
}

/// Run `attempt` until it isn't [`Attempt::Retry`], sleeping `delays`
/// between tries. `true` when it was sent.
pub(crate) async fn send_with_retry<F, Fut>(mut attempt: F, delays: &[Duration]) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Attempt>,
{
    for ix in 0..=delays.len() {
        match attempt().await {
            Attempt::Sent => return true,
            Attempt::Stop => return false,
            Attempt::Retry => {}
        }
        if let Some(delay) = delays.get(ix) {
            tokio::time::sleep(*delay).await;
        }
    }
    false
}

impl Telemetry {
    /// Load (or create) the state in `data_dir`, honoring the environment.
    pub fn open(data_dir: &Path) -> Self {
        Self::open_with_env(data_dir, telemetry_env_off(|key| std::env::var(key).ok()))
    }

    pub(crate) fn open_with_env(data_dir: &Path, env_off: Option<&'static str>) -> Self {
        let path = data_dir.join(FILE_NAME);
        let today = Utc::now().date_naive();
        let state = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok())
            .filter(|state| uuid::Uuid::parse_str(&state.install_id).is_ok());
        let telemetry = Self {
            inner: Arc::new(Inner {
                path,
                state: Mutex::new(state.clone().unwrap_or_else(|| State::fresh(today))),
                env_off,
                wake: tokio::sync::Notify::new(),
                task: Mutex::new(None),
            }),
        };
        if state.is_none() {
            telemetry.save();
        }
        telemetry
    }

    fn save(&self) {
        let state = lock(&self.inner.state).clone();
        if let Err(err) = crate::device_settings::save(&self.inner.path, &state) {
            tracing::debug!(error = %err, "telemetry state not saved");
        }
    }

    pub fn settings(&self) -> TelemetrySettings {
        let state = lock(&self.inner.state);
        TelemetrySettings {
            enabled: state.enabled,
            disabled_by_env: self.inner.env_off.map(str::to_owned),
            notice_pending: !state.notice_shown,
        }
    }

    pub fn update(
        &self,
        update: TelemetrySettingsUpdate,
    ) -> Result<TelemetrySettings, EngineError> {
        {
            let mut state = lock(&self.inner.state);
            if let Some(enabled) = update.enabled {
                state.enabled = enabled;
            }
            if let Some(shown) = update.notice_shown {
                state.notice_shown = shown;
            }
        }
        let state = lock(&self.inner.state).clone();
        crate::device_settings::save(&self.inner.path, &state)?;
        if update.enabled == Some(true) {
            // Turning it on is use: today's ping may go out now.
            self.note_activity(false);
        }
        Ok(self.settings())
    }

    /// Pings are actually sent.
    pub fn sending(&self) -> bool {
        self.settings().sending()
    }

    /// The install id (tests; never shown in the UI or logs).
    #[cfg(test)]
    pub(crate) fn install_id(&self) -> String {
        lock(&self.inner.state).install_id.clone()
    }

    /// A person did something today (`approval` = answered an approval or a
    /// question): mark the day and let the sender check for a new day.
    pub fn note_activity(&self, approval: bool) {
        if !self.sending() {
            return;
        }
        {
            let mut state = lock(&self.inner.state);
            let mark = state.mark(Utc::now().date_naive());
            mark.active = true;
            if approval {
                mark.approvals += 1;
            }
        }
        self.save();
        self.inner.wake.notify_one();
    }

    /// Note that the engine ran today (`active`: in the desktop app).
    fn mark_today(&self, today: NaiveDate, active: bool) {
        if !self.sending() {
            return;
        }
        {
            let mut state = lock(&self.inner.state);
            let mark = state.mark(today);
            mark.active |= active;
        }
        self.save();
    }

    /// The ping owed for `today`, with the usage it reports. `None` when
    /// nothing is due.
    fn prepare(
        &self,
        today: NaiveDate,
        interactive: bool,
        sources: &TelemetrySources,
    ) -> Option<(TelemetryPing, Option<NaiveDate>)> {
        let (state, usage_day) = {
            let state = lock(&self.inner.state);
            if !state.due(today) || self.inner.env_off.is_some() {
                return None;
            }
            let usage_day = state.usage_day(today);
            (state.clone(), usage_day)
        };
        let usage = usage_day.map(|day| {
            let approvals = state.days.get(&day).map_or(0, |mark| mark.approvals);
            sources.collect(day, approvals)
        });
        let active = interactive || state.days.get(&today).is_some_and(|mark| mark.active);
        Some((
            TelemetryPing {
                v: TELEMETRY_VERSION,
                install_id: state.install_id.clone(),
                day: today,
                app_version: env!("CARGO_PKG_VERSION").to_owned(),
                os: telemetry_os().to_owned(),
                arch: telemetry_arch().to_owned(),
                first_seen: state.first_seen.min(today),
                active,
                usage,
            },
            usage_day,
        ))
    }

    fn mark_sent(&self, today: NaiveDate, reported: Option<NaiveDate>) {
        lock(&self.inner.state).mark_sent(today, reported);
        self.save();
    }

    /// Send today's ping if it's owed: build it (blocking reads off the
    /// runtime), then POST with retries via `post`.
    pub(crate) async fn send_due<P, Fut>(
        &self,
        today: NaiveDate,
        interactive: bool,
        sources: &TelemetrySources,
        delays: &[Duration],
        mut post: P,
    ) -> bool
    where
        P: FnMut(TelemetryPing) -> Fut,
        Fut: Future<Output = Attempt>,
    {
        let this = self.clone();
        let built_sources = sources.clone();
        let Ok(Some((ping, usage_day))) =
            tokio::task::spawn_blocking(move || this.prepare(today, interactive, &built_sources))
                .await
        else {
            return false;
        };
        let sent = send_with_retry(
            || {
                let ping = ping.clone();
                let still_on = self.sending();
                let fut = still_on.then(|| post(ping));
                async move {
                    match fut {
                        Some(fut) => fut.await,
                        None => Attempt::Stop,
                    }
                }
            },
            delays,
        )
        .await;
        // Sent: the usage day is reported. Failed: today is still done
        // (one ping a day at most); tomorrow reports the latest day instead.
        self.mark_sent(today, sent.then_some(usage_day).flatten());
        sent
    }

    /// Start the daily sender against `edge_url`. `interactive` = the
    /// desktop app (its launch counts as use); the headless engine passes
    /// `false`. Idempotent.
    pub fn start(&self, edge_url: &str, interactive: bool, sources: TelemetrySources) {
        let mut task = lock(&self.inner.task);
        if task.is_some() {
            return;
        }
        let settings = self.settings();
        if !interactive && settings.sending() && settings.notice_pending {
            tracing::info!(
                "Godspeed sends an anonymous daily usage ping (version, how much you used it, \
                 token totals). No prompts or code. Turn off with GODSPEED_TELEMETRY=0. \
                 Details: {}",
                zeron_proto::PRIVACY_URL
            );
        }
        let url = format!("{}{}", edge_url.trim_end_matches('/'), TELEMETRY_PATH);
        let this = self.clone();
        *task = Some(tokio::spawn(async move {
            this.mark_today(Utc::now().date_naive(), interactive);
            let first = this.settings().notice_pending;
            tokio::time::sleep(if first && interactive {
                FIRST_LAUNCH_DELAY
            } else {
                LAUNCH_DELAY
            })
            .await;
            let client = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default();
            loop {
                let today = Utc::now().date_naive();
                let client = client.clone();
                let url = url.clone();
                this.send_due(today, interactive, &sources, &RETRY_DELAYS, move |ping| {
                    post_ping(client.clone(), url.clone(), ping)
                })
                .await;
                tokio::select! {
                    _ = this.inner.wake.notified() => {}
                    _ = tokio::time::sleep(TICK) => {}
                }
            }
        }));
    }

    /// Stop the sender (engine shutdown).
    pub async fn shutdown(&self) {
        let task = lock(&self.inner.task).take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn post_ping(client: reqwest::Client, url: String, ping: TelemetryPing) -> Attempt {
    match client.post(&url).json(&ping).send().await {
        Ok(response) if response.status().is_success() => Attempt::Sent,
        Ok(response)
            if response.status().as_u16() == 429 || response.status().is_server_error() =>
        {
            Attempt::Retry
        }
        Ok(response) => {
            tracing::debug!(status = %response.status(), "telemetry ping rejected");
            Attempt::Stop
        }
        Err(_) => Attempt::Retry,
    }
}

#[cfg(test)]
mod tests;
