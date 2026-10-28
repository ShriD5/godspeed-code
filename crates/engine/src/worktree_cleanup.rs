//! Worktree cleanup for the worktrees Godspeed created on this device: the
//! storage listing behind Settings → General → Worktrees, "Delete all
//! eligible", and the background sweep that follows the device's policy
//! (delete merged branches, delete worktrees untouched for N days).
//!
//! Listing and deleting reuse the unused-worktree scan and the environment
//! deletion path (`Repos::godspeed_worktrees`,
//! `WorktreeEnvironments::delete_worktree`, which runs teardown commands), so
//! a cleaned worktree goes exactly as one deleted by hand. Every deletion is
//! decided afresh right before it happens, never with `force`: uncommitted
//! changes, unpushed commits and running chats keep a worktree.
//!
//! The sweep, like the thread lifecycle timer, re-reads the wall clock at
//! least every [`MAX_SLEEP`] because tokio's timers stop while a laptop
//! sleeps.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::worktree_cleanup::{
    CleanupMode, DeletedWorktree, FailedWorktree, KeptWorktree, WorktreeFacts,
    decide_worktree_cleanup,
};
use zeron_proto::{
    Chat, SessionStatus, WorktreeCleanupReport, WorktreeCleanupSettings, WorktreeStorageEntry,
};

use crate::EngineError;
use crate::device_settings;
use crate::repos::{Repos, canonical_lenient, dir_size_on_disk};
use crate::workspace_host::WorkspaceHost;
use crate::worktree_env::WorktreeEnvironments;

const SETTINGS_FILE: &str = "worktree-cleanup.json";
/// How often the policy sweeps while on.
const SWEEP_INTERVAL: TimeDelta = TimeDelta::hours(6);
/// The first sweep waits out engine start, so boot stays quick.
const FIRST_SWEEP_DELAY: TimeDelta = TimeDelta::minutes(2);
/// A policy just turned on sweeps this soon.
const ENABLED_SWEEP_DELAY: TimeDelta = TimeDelta::seconds(30);
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);
/// Sizes are measured off the async workers and kept this long.
const SIZE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct WorktreeCleanupService {
    inner: Arc<Inner>,
}

struct Inner {
    settings_path: PathBuf,
    settings: Mutex<WorktreeCleanupSettings>,
    repos: Repos,
    workspace: WorkspaceHost,
    environments: WorktreeEnvironments,
    sizes: Mutex<HashMap<PathBuf, (Instant, u64)>>,
    /// One clean (sweep or manual) at a time.
    cleaning: tokio::sync::Mutex<()>,
    schedule: Mutex<Schedule>,
    wake: Notify,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

struct Schedule {
    last_sweep: Option<DateTime<Utc>>,
    first_due: DateTime<Utc>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One worktree with what the decision needs, before the verdict.
struct Gathered {
    entry: WorktreeStorageEntry,
    facts: WorktreeFacts,
}

impl WorktreeCleanupService {
    pub fn open(
        data_dir: &Path,
        repos: Repos,
        workspace: WorkspaceHost,
        environments: WorktreeEnvironments,
    ) -> Self {
        let settings_path = data_dir.join(SETTINGS_FILE);
        let settings: WorktreeCleanupSettings = device_settings::load(&settings_path);
        Self {
            inner: Arc::new(Inner {
                settings: Mutex::new(if settings.check().is_ok() {
                    settings
                } else {
                    WorktreeCleanupSettings::default()
                }),
                settings_path,
                repos,
                workspace,
                environments,
                sizes: Mutex::new(HashMap::new()),
                cleaning: tokio::sync::Mutex::new(()),
                schedule: Mutex::new(Schedule {
                    last_sweep: None,
                    first_due: Utc::now() + FIRST_SWEEP_DELAY,
                }),
                wake: Notify::new(),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// Start the sweep timer. Needs a tokio runtime.
    pub fn start(&self) {
        let service = self.clone();
        self.inner
            .tasks
            .spawn(async move { service.run_loop().await });
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.tasks.wait()).await;
    }

    pub fn settings(&self) -> WorktreeCleanupSettings {
        *lock(&self.inner.settings)
    }

    pub fn set_settings(
        &self,
        settings: WorktreeCleanupSettings,
    ) -> Result<WorktreeCleanupSettings, EngineError> {
        settings.check().map_err(EngineError::Other)?;
        device_settings::save(&self.inner.settings_path, &settings)?;
        let was = std::mem::replace(&mut *lock(&self.inner.settings), settings);
        if settings.enabled() && settings != was {
            let mut schedule = lock(&self.inner.schedule);
            schedule.last_sweep = None;
            schedule.first_due = Utc::now() + ENABLED_SWEEP_DELAY;
        }
        self.inner.wake.notify_one();
        Ok(settings)
    }

    async fn run_loop(&self) {
        loop {
            let now = Utc::now();
            let enabled = self.settings().enabled();
            let due = {
                let schedule = lock(&self.inner.schedule);
                schedule
                    .last_sweep
                    .map_or(schedule.first_due, |at| at + SWEEP_INTERVAL)
            };
            if enabled && now >= due {
                lock(&self.inner.schedule).last_sweep = Some(now);
                match self.clean(&[], CleanupMode::Automatic).await {
                    Ok(report) if !report.deleted.is_empty() || !report.failed.is_empty() => {
                        tracing::info!(
                            deleted = report.deleted.len(),
                            kept = report.kept.len(),
                            failed = report.failed.len(),
                            "worktree cleanup sweep"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(%err, "worktree cleanup sweep failed"),
                }
                continue;
            }
            let sleep = if enabled {
                (due - now)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
                    .min(MAX_SLEEP)
            } else {
                MAX_SLEEP
            };
            tokio::select! {
                _ = self.inner.cancel.cancelled() => return,
                _ = self.inner.wake.notified() => {}
                _ = tokio::time::sleep(sleep) => {}
            }
        }
    }

    /// Every Godspeed worktree of `repo_path` (all of this device's git
    /// projects when `None`), with the verdict "Delete all eligible" would
    /// act on. With `measure`, sizes not cached yet are measured first.
    pub async fn list(
        &self,
        repo_path: Option<String>,
        measure: bool,
    ) -> Result<Vec<WorktreeStorageEntry>, EngineError> {
        let gathered = self.gather(repo_path, measure).await?;
        let settings = self.settings();
        let now = Utc::now();
        Ok(gathered
            .into_iter()
            .map(|g| WorktreeStorageEntry {
                decision: decide_worktree_cleanup(&settings, &g.facts, now, CleanupMode::Manual),
                ..g.entry
            })
            .collect())
    }

    /// Delete every eligible worktree (only those in `paths` when it isn't
    /// empty), deciding each afresh. An automatic clean follows the policy;
    /// a manual one also takes worktrees no chat uses.
    pub async fn clean(
        &self,
        paths: &[String],
        mode: CleanupMode,
    ) -> Result<WorktreeCleanupReport, EngineError> {
        let _one_at_a_time = self.inner.cleaning.lock().await;
        let settings = self.settings();
        let wanted: HashSet<PathBuf> = paths
            .iter()
            .map(|p| canonical_lenient(Path::new(p)))
            .collect();
        let mut report = WorktreeCleanupReport::default();
        for g in self.gather(None, false).await? {
            if !wanted.is_empty() && !wanted.contains(&canonical_lenient(Path::new(&g.entry.path)))
            {
                continue;
            }
            let decision = decide_worktree_cleanup(&settings, &g.facts, Utc::now(), mode);
            let Some(reason) = decision.reason else {
                continue;
            };
            if let Some(kept) = decision.kept {
                report.kept.push(KeptWorktree {
                    path: g.entry.path,
                    reason: kept,
                });
                continue;
            }
            let path = PathBuf::from(&g.entry.path);
            match self
                .inner
                .environments
                .delete_worktree(Path::new(&g.entry.repo_path), &path, false)
                .await
            {
                Ok(teardown_error) => {
                    lock(&self.inner.sizes).remove(&canonical_lenient(&path));
                    report.deleted.push(DeletedWorktree {
                        path: g.entry.path,
                        branch: g.entry.branch,
                        reason,
                        teardown_error,
                    });
                }
                Err(err) => report.failed.push(FailedWorktree {
                    path: g.entry.path,
                    error: err.to_string(),
                }),
            }
        }
        Ok(report)
    }

    async fn gather(
        &self,
        repo_path: Option<String>,
        measure: bool,
    ) -> Result<Vec<Gathered>, EngineError> {
        let inner = &self.inner;
        let chats = inner.workspace.read_chats()?;
        let busy: HashSet<String> = inner
            .workspace
            .read_sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| {
                matches!(
                    s.status,
                    SessionStatus::Working | SessionStatus::AwaitingInput
                )
            })
            .map(|s| s.chat_id)
            .collect();
        let chat_dirs: Vec<(PathBuf, &Chat)> = chats
            .iter()
            .filter_map(|chat| Some((canonical_lenient(Path::new(chat.cwd.as_deref()?)), chat)))
            .collect();
        let repos = match repo_path {
            Some(path) => vec![path],
            None => local_repo_paths(&inner.workspace)?,
        };
        let mut seen = HashSet::new();
        let mut gathered = Vec::new();
        for repo in repos {
            let repo = PathBuf::from(repo);
            let worktrees = match inner.repos.godspeed_worktrees(&repo).await {
                Ok(found) => found,
                Err(err) => {
                    tracing::debug!(repo = %repo.display(), error = %err, "worktree storage scan skipped");
                    continue;
                }
            };
            let defaults = inner.repos.default_branch_refs(&repo).await;
            for worktree in worktrees {
                let path = PathBuf::from(&worktree.path);
                let canonical = canonical_lenient(&path);
                // Two projects of one repository list its worktrees twice.
                if !seen.insert(canonical.clone()) {
                    continue;
                }
                let exists = path.exists();
                let dirty = exists && inner.repos.uncommitted_paths(&path).await != Some(0);
                let (merged, unpushed) = match &worktree.branch {
                    Some(branch) => (
                        inner.repos.branch_merged(&repo, branch, &defaults).await,
                        inner
                            .repos
                            .unpushed_commits(
                                &repo,
                                &format!("refs/heads/{branch}"),
                                &defaults,
                                false,
                            )
                            .await
                            .is_none_or(|n| n > 0),
                    ),
                    // Detached: commits on no branch or remote would be lost.
                    None => (
                        false,
                        exists
                            && inner
                                .repos
                                .unpushed_commits(&path, "HEAD", &defaults, true)
                                .await
                                .is_none_or(|n| n > 0),
                    ),
                };
                let mut users: Vec<&Chat> = chat_dirs
                    .iter()
                    .filter(|(dir, _)| dir.starts_with(&canonical))
                    .map(|(_, chat)| *chat)
                    .collect();
                users.sort_by_key(|chat| {
                    (
                        !chat.archived,
                        chat.last_message_at.unwrap_or(chat.created_at),
                    )
                });
                let owner = users.last().copied();
                let running = users.iter().any(|chat| busy.contains(&chat.id));
                let chat_activity = users
                    .iter()
                    .map(|chat| chat.last_message_at.unwrap_or(chat.created_at))
                    .max();
                let checkout_activity = if exists {
                    inner.repos.checkout_last_activity(&path).await
                } else {
                    None
                };
                let last_activity = checkout_activity.max(chat_activity);
                let size_bytes = if exists {
                    self.size_of(&canonical, measure).await
                } else {
                    Some(0)
                };
                gathered.push(Gathered {
                    facts: WorktreeFacts {
                        merged,
                        dirty,
                        unpushed,
                        running,
                        unused: users.is_empty(),
                        last_activity,
                    },
                    entry: WorktreeStorageEntry {
                        repo_path: repo.to_string_lossy().into_owned(),
                        path: worktree.path,
                        branch: worktree.branch,
                        size_bytes,
                        last_activity,
                        merged,
                        dirty,
                        unpushed,
                        running,
                        chat_id: owner.map(|chat| chat.id.clone()),
                        chat_title: owner.and_then(|chat| chat.title.clone()),
                        decision: Default::default(),
                    },
                });
            }
        }
        Ok(gathered)
    }

    /// The cached size of `path`, measured on a blocking thread when asked
    /// and missing or older than [`SIZE_TTL`].
    async fn size_of(&self, path: &Path, measure: bool) -> Option<u64> {
        let cached = lock(&self.inner.sizes).get(path).copied();
        match cached {
            Some((at, bytes)) if at.elapsed() < SIZE_TTL => return Some(bytes),
            Some((_, bytes)) if !measure => return Some(bytes),
            None if !measure => return None,
            _ => {}
        }
        let root = path.to_path_buf();
        let bytes = tokio::task::spawn_blocking(move || dir_size_on_disk(&root))
            .await
            .ok()?;
        lock(&self.inner.sizes).insert(path.to_path_buf(), (Instant::now(), bytes));
        Some(bytes)
    }
}

/// The git projects on this device, one path per repository folder.
pub(crate) fn local_repo_paths(workspace: &WorkspaceHost) -> Result<Vec<String>, EngineError> {
    let device = workspace.device_id().to_string();
    let mut paths: Vec<String> = workspace
        .read_spaces()?
        .into_iter()
        .filter(|space| space.device_id == device && space.git_detected)
        .map(|space| space.path)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}
