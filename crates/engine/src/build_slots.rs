//! Shared build slots: one jobserver for every agent and terminal this
//! engine starts (Settings → General → Builds).
//!
//! Without it, four agents building one Rust repo at once each start a rustc
//! per core — 30+ compilers at 1–2 GB each on a 16 GB laptop. With it they
//! draw from one pool of N slots (default: the core count, what one cargo
//! build uses alone), so a lone build runs exactly as before and parallel
//! builds share the machine instead of swapping it to a crawl.
//!
//! - **Pool**: [`pool::SlotPool`], a GNU make 4.4 style FIFO in an
//!   owner-only directory under the temp dir, one per data dir (the temp dir
//!   is what agent sandboxes usually leave writable). Unix only; on Windows
//!   the setting reports unsupported.
//! - **Injection**: published through [`zeron_harness::build_env`], which
//!   every agent spawn and terminal reads. It yields to any parallelism the
//!   user picked (`CARGO_BUILD_JOBS`, `MAKEFLAGS`, cargo config `build.jobs`).
//! - **Leaks**: a client killed while holding a slot never returns it (make
//!   semantics). Every [`TICK`] the engine counts the free slots; when some
//!   are missing for two ticks in a row while no build tool runs anywhere,
//!   it puts them back. Surplus (a top-up that raced a build) is drained the
//!   same way. A FIFO deleted from under it (temp-dir sweeps) is recreated.
//! - **sccache**: when on and installed, `RUSTC_WRAPPER` for runs in Rust
//!   projects, so worktrees of one repo share compiled dependencies.

#[cfg(unix)]
mod pool;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeron_harness::build_env::{self, BuildEnv, JOBS_OVERRIDES, WRAPPER_OVERRIDES};
use zeron_proto::{BuildSlotsSettings, BuildSlotsSettingsUpdate, MAX_BUILD_SLOTS};

use crate::EngineError;

/// The setting file in the data dir.
pub const FILE_NAME: &str = "build-slots.json";

/// How often the pool is checked for leaks and deletion.
const TICK: Duration = Duration::from_secs(30);

/// Process names that mean a build may be holding slots.
#[cfg_attr(not(unix), allow(dead_code))]
const BUILD_TOOLS: &[&str] = &["cargo", "rustc", "make", "gmake", "ninja", "cmake"];

/// What the user chose (the rest of [`BuildSlotsSettings`] is found).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Stored {
    #[serde(default = "yes")]
    pub(crate) share_slots: bool,
    /// 0 = automatic.
    #[serde(default)]
    pub(crate) slots: u32,
    #[serde(default = "yes")]
    pub(crate) use_sccache: bool,
}

fn yes() -> bool {
    true
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            share_slots: true,
            slots: 0,
            use_sccache: true,
        }
    }
}

impl Stored {
    pub(crate) fn apply(&mut self, update: &BuildSlotsSettingsUpdate) {
        if let Some(share) = update.share_slots {
            self.share_slots = share;
        }
        if let Some(slots) = update.slots {
            self.slots = slots.min(MAX_BUILD_SLOTS);
        }
        if let Some(use_sccache) = update.use_sccache {
            self.use_sccache = use_sccache;
        }
    }
}

/// The automatic slot count: available parallelism — cores on Apple
/// Silicon, hardware threads elsewhere — which is also cargo's own `-j`.
pub(crate) fn default_slots() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
        .clamp(1, MAX_BUILD_SLOTS)
}

/// What the host found on its PATH and in the user's environment.
#[derive(Debug, Clone, Default)]
struct Probe {
    make_reads_fifo: bool,
    sccache: Option<PathBuf>,
    paused_by: Option<&'static str>,
    sccache_paused_by: Option<&'static str>,
}

impl Probe {
    /// Blocking: runs `make --version` and may wait on the login-shell
    /// snapshot.
    fn run() -> Self {
        Self {
            make_reads_fifo: build_env::make_reads_fifo(),
            sccache: build_env::find_tool(if cfg!(windows) {
                "sccache.exe"
            } else {
                "sccache"
            }),
            paused_by: build_env::first_ambient(JOBS_OVERRIDES),
            sccache_paused_by: build_env::first_ambient(WRAPPER_OVERRIDES),
        }
    }
}

/// One leak-check step's verdict.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebalance {
    Nothing,
    /// Put this many leaked slots back.
    Refill(usize),
    /// Take this many surplus slots out.
    Drain(usize),
}

/// Decide a leak-check step. `quiet` counts the consecutive ticks the pool
/// has been off its size with no build tool running; it acts on the second,
/// so a build that just exited (or is about to start) gets a full tick.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn rebalance(
    available: usize,
    tokens: usize,
    builds_running: bool,
    quiet: &mut u32,
) -> Rebalance {
    if available == tokens || builds_running {
        *quiet = 0;
        return Rebalance::Nothing;
    }
    *quiet += 1;
    if *quiet < 2 {
        return Rebalance::Nothing;
    }
    *quiet = 0;
    if available < tokens {
        Rebalance::Refill(tokens - available)
    } else {
        Rebalance::Drain(available - tokens)
    }
}

/// Whether any build tool is running on this machine (any user's process
/// list we can see). Errs on "yes" when `ps` fails: a delayed refill is
/// harmless, an extra slot under a live build isn't.
#[cfg_attr(not(unix), allow(dead_code))]
fn builds_running() -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "comm="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    any_build_tool(&String::from_utf8_lossy(&out.stdout))
}

/// `ps -o comm=` output names a build tool (macOS prints full paths).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn any_build_tool(ps: &str) -> bool {
    ps.lines().any(|line| {
        let name = line.trim().rsplit('/').next().unwrap_or_default();
        BUILD_TOOLS.contains(&name)
    })
}

/// The pool's directory for `data_dir`: owner-only, under the temp dir,
/// named by a stable hash of the data dir so profiles don't share a pool.
pub(crate) fn pool_dir(temp: &Path, data_dir: &Path) -> PathBuf {
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data_dir.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    temp.join(format!("godspeed-build-slots-{hash:016x}"))
}

#[cfg_attr(not(unix), allow(dead_code))]
struct State {
    stored: Stored,
    probe: Option<Probe>,
    #[cfg(unix)]
    pool: Option<pool::SlotPool>,
    quiet: u32,
    /// This service published the process-wide build environment (so only
    /// it clears it again).
    published: bool,
}

#[cfg_attr(not(unix), allow(dead_code))]
struct Inner {
    settings_path: PathBuf,
    pool_dir: PathBuf,
    /// Where the pool goes when the temp dir can't be used.
    fallback_dir: PathBuf,
    state: Mutex<State>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The engine's build-slot service. Cheap to clone.
#[derive(Clone)]
pub struct BuildSlots {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl BuildSlots {
    /// Load the setting from `data_dir`. Nothing runs until [`Self::start`].
    pub fn open(data_dir: &Path) -> Self {
        Self::open_in(data_dir, &std::env::temp_dir())
    }

    pub(crate) fn open_in(data_dir: &Path, temp: &Path) -> Self {
        let settings_path = data_dir.join(FILE_NAME);
        Self {
            inner: Arc::new(Inner {
                pool_dir: pool_dir(temp, data_dir),
                fallback_dir: data_dir.join("build-slots"),
                state: Mutex::new(State {
                    stored: crate::device_settings::load(&settings_path),
                    probe: None,
                    #[cfg(unix)]
                    pool: None,
                    quiet: 0,
                    published: false,
                }),
                settings_path,
                task: Mutex::new(None),
            }),
        }
    }

    /// The setting and what the engine found.
    pub fn settings(&self) -> BuildSlotsSettings {
        let state = lock(&self.inner.state);
        let probe = state.probe.clone().unwrap_or_default();
        BuildSlotsSettings {
            share_slots: state.stored.share_slots,
            slots: state.stored.slots,
            use_sccache: state.stored.use_sccache,
            default_slots: default_slots(),
            supported: cfg!(unix),
            active: self.pool_running(&state),
            paused_by: probe.paused_by.map(str::to_owned),
            sccache_path: probe.sccache.map(|p| p.to_string_lossy().into_owned()),
            sccache_paused_by: probe.sccache_paused_by.map(str::to_owned),
        }
    }

    #[cfg(unix)]
    fn pool_running(&self, state: &State) -> bool {
        state.pool.is_some()
    }

    #[cfg(not(unix))]
    fn pool_running(&self, _: &State) -> bool {
        false
    }

    /// Save a change and apply it to new spawns (running agents keep the
    /// environment they started with).
    pub fn update(
        &self,
        update: BuildSlotsSettingsUpdate,
    ) -> Result<BuildSlotsSettings, EngineError> {
        {
            let mut state = lock(&self.inner.state);
            state.stored.apply(&update);
            crate::device_settings::save(&self.inner.settings_path, &state.stored)?;
            if state.probe.is_some() {
                self.reconcile(&mut state);
            }
        }
        Ok(self.settings())
    }

    /// Probe the host, start the pool and its leak checks. Idempotent; needs
    /// a Tokio runtime.
    pub fn start(&self) {
        let mut task = lock(&self.inner.task);
        if task.is_some() {
            return;
        }
        let this = self.clone();
        *task = Some(tokio::spawn(async move {
            loop {
                // Re-probe now and then: sccache or a newer make may have been
                // installed, the login shell snapshot may have arrived.
                if let Ok(probe) = tokio::task::spawn_blocking(Probe::run).await {
                    let mut state = lock(&this.inner.state);
                    state.probe = Some(probe);
                    this.reconcile(&mut state);
                }
                for _ in 0..20 {
                    tokio::time::sleep(TICK).await;
                    let this = this.clone();
                    let _ = tokio::task::spawn_blocking(move || this.check()).await;
                }
            }
        }));
    }

    /// Stop the checks, remove the pool and stop injecting (engine shutdown).
    pub async fn shutdown(&self) {
        let task = lock(&self.inner.task).take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        let mut state = lock(&self.inner.state);
        #[cfg(unix)]
        {
            state.pool = None;
        }
        state.probe = None;
        if std::mem::take(&mut state.published) {
            build_env::install(None);
        }
    }

    /// Bring the pool and the published environment in line with the
    /// setting.
    fn reconcile(&self, state: &mut State) {
        let probe = state.probe.clone().unwrap_or_default();
        let jobserver = self.reconcile_pool(state);
        let sccache = probe.sccache.filter(|_| state.stored.use_sccache);
        let env = BuildEnv {
            jobserver,
            make_reads_fifo: probe.make_reads_fifo,
            sccache,
        };
        let useful = env.jobserver.is_some() || env.sccache.is_some();
        build_env::install(useful.then_some(env));
        state.published = true;
    }

    #[cfg(unix)]
    fn reconcile_pool(&self, state: &mut State) -> Option<String> {
        let tokens = if state.stored.share_slots {
            let slots = if state.stored.slots == 0 {
                default_slots()
            } else {
                state.stored.slots
            };
            Some(slots.saturating_sub(1) as usize)
        } else {
            None
        };
        let keep = match (&state.pool, tokens) {
            (Some(pool), Some(tokens)) => pool.tokens() == tokens && pool.is_intact(),
            _ => false,
        };
        if !keep {
            // Drop first: the new pool takes the same path.
            state.pool = None;
            state.quiet = 0;
            if let Some(tokens) = tokens {
                state.pool = self.create_pool(tokens);
            }
        }
        state.pool.as_ref().map(pool::SlotPool::auth)
    }

    #[cfg(not(unix))]
    fn reconcile_pool(&self, _: &mut State) -> Option<String> {
        None
    }

    #[cfg(unix)]
    fn create_pool(&self, tokens: usize) -> Option<pool::SlotPool> {
        let created = pool::SlotPool::create(&self.inner.pool_dir, tokens).or_else(|err| {
            tracing::warn!(error = %err, dir = %self.inner.pool_dir.display(), "build slots: temp dir unusable, using the data dir");
            pool::SlotPool::create(&self.inner.fallback_dir, tokens)
        });
        match created {
            Ok(pool) => {
                tracing::info!(slots = tokens + 1, auth = %pool.auth(), "build slots shared across agents");
                Some(pool)
            }
            Err(err) => {
                tracing::warn!(error = %err, "build slots: could not create the pool");
                None
            }
        }
    }

    /// One leak/deletion check (blocking: may run `ps`).
    fn check(&self) {
        #[cfg(unix)]
        {
            let intact = {
                let state = lock(&self.inner.state);
                match &state.pool {
                    None => return,
                    Some(pool) => pool.is_intact(),
                }
            };
            if !intact {
                tracing::info!("build slots: the pool's FIFO went away; recreating it");
                let mut state = lock(&self.inner.state);
                self.reconcile(&mut state);
                return;
            }
            let (available, tokens) = {
                let state = lock(&self.inner.state);
                let Some(pool) = &state.pool else { return };
                match pool.available() {
                    Ok(available) => (available, pool.tokens()),
                    Err(err) => {
                        tracing::debug!(error = %err, "build slots: could not count free slots");
                        return;
                    }
                }
            };
            // Only pay for `ps` when the count is off.
            let running = available != tokens && builds_running();
            let mut state = lock(&self.inner.state);
            let mut quiet = state.quiet;
            let verdict = rebalance(available, tokens, running, &mut quiet);
            state.quiet = quiet;
            let Some(pool) = &state.pool else { return };
            match verdict {
                Rebalance::Nothing => {}
                Rebalance::Refill(count) => match pool.add(count) {
                    Ok(()) => tracing::info!(
                        count,
                        "build slots: returned slots a dead build never gave back"
                    ),
                    Err(err) => tracing::warn!(error = %err, "build slots: refill failed"),
                },
                Rebalance::Drain(count) => match pool.take(count) {
                    Ok(taken) => tracing::info!(taken, "build slots: drained surplus slots"),
                    Err(err) => tracing::warn!(error = %err, "build slots: drain failed"),
                },
            }
        }
    }

    /// The running pool's `fifo:` auth (tests).
    #[cfg(all(test, unix))]
    pub(crate) fn auth(&self) -> Option<String> {
        lock(&self.inner.state)
            .pool
            .as_ref()
            .map(pool::SlotPool::auth)
    }

    /// Apply a probe without touching the host (tests).
    #[cfg(test)]
    pub(crate) fn set_probe_for_test(&self, make_reads_fifo: bool, sccache: Option<PathBuf>) {
        let mut state = lock(&self.inner.state);
        state.probe = Some(Probe {
            make_reads_fifo,
            sccache,
            paused_by: None,
            sccache_paused_by: None,
        });
        self.reconcile(&mut state);
    }
}
