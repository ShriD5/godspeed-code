//! Keep the computer awake while agents work (Settings → General): while any
//! run on this engine works or waits on input, hold an OS sleep inhibitor —
//! `caffeinate -i` on macOS, `systemd-inhibit` on Linux — and let it go a
//! minute after the last one settles, so back-to-back turns share one hold.
//! Closing a laptop lid still sleeps it. The inhibitor watches the engine's
//! pid, so it never outlives a crashed engine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::{KeepAwakeSettings, Session, SessionStatus};

use crate::{EngineError, device_settings};

/// The setting file in the data dir.
pub const FILE_NAME: &str = "keep-awake.json";

/// Bridges the gap between back-to-back runs.
const RELEASE_GRACE: Duration = Duration::from_secs(60);
const RETRY_FIRST: Duration = Duration::from_secs(5);
/// A host without an inhibitor (no logind) retries this rarely.
const RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// Starts one hold; the future resolves, with why, only if the hold ends on
/// its own. Dropping it releases the hold.
pub(crate) type Hold = Arc<dyn Fn() -> BoxFuture<'static, String> + Send + Sync>;

#[derive(Clone)]
pub struct KeepAwakeService {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    enabled: watch::Sender<bool>,
    cancel: CancellationToken,
    tasks: TaskTracker,
    settings: Mutex<KeepAwakeSettings>,
}

impl KeepAwakeService {
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(FILE_NAME);
        let settings: KeepAwakeSettings = device_settings::load(&path);
        Self {
            inner: Arc::new(Inner {
                path,
                enabled: watch::channel(settings.enabled).0,
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
                settings: Mutex::new(settings),
            }),
        }
    }

    pub fn settings(&self) -> KeepAwakeSettings {
        *self
            .inner
            .settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_settings(
        &self,
        settings: KeepAwakeSettings,
    ) -> Result<KeepAwakeSettings, EngineError> {
        device_settings::save(&self.inner.path, &settings)?;
        *self
            .inner
            .settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = settings;
        self.inner.enabled.send_replace(settings.enabled);
        Ok(settings)
    }

    /// Start holding for `sessions`' runs. Never from unit tests: it starts
    /// a process that holds off sleep for the whole machine.
    pub fn start(&self, sessions: watch::Receiver<Vec<Session>>) {
        let Some(hold) = system_hold() else {
            return;
        };
        let enabled = self.inner.enabled.subscribe();
        let cancel = self.inner.cancel.clone();
        self.inner.tasks.spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = run(sessions, enabled, hold, RELEASE_GRACE) => {}
            }
        });
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.tasks.wait()).await;
    }
}

/// A run that keeps the machine busy: working, waiting on an answer that may
/// come from a phone, or settled with background work still live.
fn busy(sessions: &[Session]) -> bool {
    sessions.iter().any(|session| {
        matches!(
            session.status,
            SessionStatus::Working | SessionStatus::AwaitingInput
        ) || session.background_tasks > 0
    })
}

/// Holds while `enabled` and a run is busy, and for `grace` after; a run
/// inside the grace keeps the same hold. A hold that ends on its own is
/// restarted after a growing delay.
pub(crate) async fn run(
    mut sessions: watch::Receiver<Vec<Session>>,
    mut enabled: watch::Receiver<bool>,
    hold: Hold,
    grace: Duration,
) {
    loop {
        while !(*enabled.borrow_and_update() && busy(&sessions.borrow_and_update())) {
            tokio::select! {
                changed = sessions.changed() => if changed.is_err() { return },
                changed = enabled.changed() => if changed.is_err() { return },
            }
        }
        let mut retry = RETRY_FIRST;
        let mut holding = hold();
        let mut release_at: Option<Instant> = None;
        loop {
            let on = *enabled.borrow_and_update();
            if !on {
                break;
            }
            if busy(&sessions.borrow_and_update()) {
                release_at = None;
            } else if release_at.is_none() {
                release_at = Some(Instant::now() + grace);
            }
            let release = async {
                match release_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = release => break,
                changed = sessions.changed() => if changed.is_err() { return },
                changed = enabled.changed() => if changed.is_err() { return },
                reason = &mut holding => {
                    tracing::warn!(%reason, "keep-awake: sleep inhibitor stopped; retrying in {retry:?}");
                    let delay = retry;
                    retry = (retry * 2).min(RETRY_MAX);
                    let hold = hold.clone();
                    holding = Box::pin(async move {
                        tokio::time::sleep(delay).await;
                        hold().await
                    });
                }
            }
        }
    }
}

/// This platform's inhibitor, tied to the engine's pid.
fn system_hold() -> Option<Hold> {
    let pid = std::process::id().to_string();
    let (program, args): (&'static str, Vec<String>) = if cfg!(target_os = "macos") {
        ("/usr/bin/caffeinate", vec!["-i".into(), "-w".into(), pid])
    } else if cfg!(target_os = "linux") {
        (
            "systemd-inhibit",
            vec![
                "--what=idle:sleep".into(),
                "--who=Godspeed".into(),
                "--why=Agents are working".into(),
                "--mode=block".into(),
                "tail".into(),
                format!("--pid={pid}"),
                "-f".into(),
                "/dev/null".into(),
            ],
        )
    } else {
        return None;
    };
    Some(Arc::new(move || {
        let mut command = tokio::process::Command::new(program);
        command
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        Box::pin(async move {
            let child = match command.spawn() {
                Ok(child) => child,
                Err(err) => return format!("{program}: {err}"),
            };
            match child.wait_with_output().await {
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    format!("{program} exited ({}) {}", output.status, stderr.trim())
                }
                Err(err) => format!("{program}: {err}"),
            }
        })
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;

    use super::*;

    const GRACE: Duration = Duration::from_secs(60);

    /// Counts holds started and holds live; `exits` makes each hold end on
    /// its own after that long.
    #[derive(Clone, Default)]
    struct Probe {
        started: Arc<AtomicUsize>,
        live: Arc<AtomicUsize>,
    }

    struct LiveGuard(Arc<AtomicUsize>);

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Probe {
        fn hold(&self, exits: Option<Duration>) -> Hold {
            let probe = self.clone();
            Arc::new(move || {
                probe.started.fetch_add(1, Ordering::SeqCst);
                probe.live.fetch_add(1, Ordering::SeqCst);
                let guard = LiveGuard(probe.live.clone());
                Box::pin(async move {
                    let _guard = guard;
                    match exits {
                        Some(after) => tokio::time::sleep(after).await,
                        None => std::future::pending().await,
                    }
                    "exited".to_string()
                })
            })
        }

        fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }

        fn live(&self) -> usize {
            self.live.load(Ordering::SeqCst)
        }
    }

    fn session(status: SessionStatus) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: "chat".into(),
            device_id: "device".into(),
            status,
            started_at: None,
            updated_at: Utc::now(),
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: false,
        }
    }

    struct Harness {
        sessions: watch::Sender<Vec<Session>>,
        enabled: watch::Sender<bool>,
        probe: Probe,
        _task: tokio::task::JoinHandle<()>,
    }

    fn harness(exits: Option<Duration>) -> Harness {
        let (sessions, sessions_rx) = watch::channel(Vec::new());
        let (enabled, enabled_rx) = watch::channel(true);
        let probe = Probe::default();
        let task = tokio::spawn(run(sessions_rx, enabled_rx, probe.hold(exits), GRACE));
        Harness {
            sessions,
            enabled,
            probe,
            _task: task,
        }
    }

    async fn settle(after: Duration) {
        tokio::time::sleep(after).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_working_run_holds_until_a_grace_after_it_settles() {
        let h = harness(None);
        settle(Duration::from_millis(1)).await;
        assert_eq!(h.probe.started(), 0, "nothing runs: no hold");

        h.sessions
            .send_replace(vec![session(SessionStatus::Working)]);
        settle(Duration::from_millis(1)).await;
        assert_eq!(h.probe.live(), 1);

        h.sessions.send_replace(vec![session(SessionStatus::Idle)]);
        settle(GRACE - Duration::from_secs(1)).await;
        assert_eq!(h.probe.live(), 1, "still inside the grace");
        settle(Duration::from_secs(2)).await;
        assert_eq!(h.probe.live(), 0, "released after the grace");
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_inside_the_grace_keeps_the_same_hold() {
        let h = harness(None);
        h.sessions
            .send_replace(vec![session(SessionStatus::Working)]);
        settle(Duration::from_millis(1)).await;
        h.sessions.send_replace(vec![session(SessionStatus::Idle)]);
        settle(GRACE / 2).await;
        h.sessions
            .send_replace(vec![session(SessionStatus::AwaitingInput)]);
        settle(GRACE * 2).await;
        assert_eq!((h.probe.started(), h.probe.live()), (1, 1));

        let mut background = session(SessionStatus::Idle);
        background.background_tasks = 1;
        h.sessions.send_replace(vec![background]);
        settle(GRACE * 2).await;
        assert_eq!(
            (h.probe.started(), h.probe.live()),
            (1, 1),
            "live background work keeps holding"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn turning_it_off_releases_at_once_and_holds_nothing() {
        let h = harness(None);
        h.sessions
            .send_replace(vec![session(SessionStatus::Working)]);
        settle(Duration::from_millis(1)).await;
        assert_eq!(h.probe.live(), 1);

        h.enabled.send_replace(false);
        settle(Duration::from_millis(1)).await;
        assert_eq!(h.probe.live(), 0);

        h.sessions.send_replace(vec![
            session(SessionStatus::Working),
            session(SessionStatus::Working),
        ]);
        settle(Duration::from_secs(5)).await;
        assert_eq!(h.probe.started(), 1, "off: runs start no hold");

        h.enabled.send_replace(true);
        settle(Duration::from_millis(1)).await;
        assert_eq!(h.probe.live(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_inhibitor_that_exits_is_restarted_after_a_delay() {
        let h = harness(Some(Duration::from_secs(1)));
        h.sessions
            .send_replace(vec![session(SessionStatus::Working)]);
        settle(Duration::from_millis(1)).await;
        assert_eq!(h.probe.started(), 1);

        settle(Duration::from_secs(1)).await;
        assert_eq!(h.probe.live(), 0);
        settle(RETRY_FIRST).await;
        assert_eq!(h.probe.started(), 2, "retried after the first delay");
    }

    #[test]
    fn the_setting_is_on_by_default_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let service = KeepAwakeService::open(dir.path());
        assert!(service.settings().enabled);
        service
            .set_settings(KeepAwakeSettings { enabled: false })
            .unwrap();
        assert!(!KeepAwakeService::open(dir.path()).settings().enabled);
    }
}
