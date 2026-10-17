//! Worktree environments: what lets several worktrees of one project run at
//! the same time (see `docs/godspeed/WORKTREES.md`).
//!
//! A project's environment (saved with its Actions, importable from the
//! `env` section of `godspeed.json`) says which untracked files to copy into
//! new worktrees, how many ports each worktree gets, the commands to run on
//! create, delete and wake, and how to name branches. This service applies
//! it around the plain git operations in [`Repos`]: it reserves ports,
//! copies files, runs setup in the worktree's "(setup)" terminal and
//! teardown before removal, and puts worktrees to sleep and wakes them.

pub mod context;
mod copy;
pub mod ports;
pub mod records;
mod scripts;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zeron_proto::{
    ProjectAction, ProjectActionIcon, ProjectActionRun, WakeWorktreeOutcome, Worktree,
    WorktreeEnvironment, WorktreeEnvironmentStatus,
};

use crate::project_actions::ActionLaunch;
use crate::{EngineError, ProjectActionsStore, Repos, Terminals};
use context::WorktreeContext;
use records::WorktreeRecord;

pub const SETUP_ACTION_ID: &str = "worktree-setup";
pub const WAKE_ACTION_ID: &str = "worktree-wake";

/// The project a worktree belongs to: its id, the path its configuration is
/// stored under, and its canonical root.
#[derive(Debug, Clone)]
pub struct ProjectScope {
    pub space_id: String,
    pub space_path: PathBuf,
    pub root: PathBuf,
}

#[derive(Clone)]
pub struct WorktreeEnvironments {
    repos: Repos,
    terminals: Terminals,
    project_actions: ProjectActionsStore,
}

impl WorktreeEnvironments {
    pub fn new(repos: Repos, terminals: Terminals, project_actions: ProjectActionsStore) -> Self {
        Self {
            repos,
            terminals,
            project_actions,
        }
    }

    fn environment(
        &self,
        scope: &ProjectScope,
    ) -> Result<Option<WorktreeEnvironment>, EngineError> {
        Ok(self
            .project_actions
            .environment(&scope.space_id, &scope.space_path)?
            .filter(|environment| !environment.is_empty()))
    }

    fn record_environment(&self, record: &WorktreeRecord) -> Option<WorktreeEnvironment> {
        let (space_id, space_path) = (record.space_id.as_ref()?, record.space_path.as_ref()?);
        self.project_actions
            .environment(space_id, Path::new(space_path))
            .ok()
            .flatten()
    }

    /// Create a worktree of `repo_path` off `base`. With a project whose
    /// environment is set, the branch follows its template, ports are
    /// reserved and files copied; if any of that fails the new worktree is
    /// removed again and the error says so.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        base: &str,
        scope: Option<&ProjectScope>,
    ) -> Result<Worktree, EngineError> {
        let environment = match scope {
            Some(scope) => self.environment(scope)?,
            None => None,
        };
        let template = environment.as_ref().and_then(|env| env.branch.as_deref());
        let worktree = self
            .repos
            .create_worktree_with_branch(repo_path, base, template)
            .await?;
        let (Some(scope), Some(environment)) = (scope, environment) else {
            return Ok(worktree);
        };
        let Err(error) = self.prepare(scope, &environment, &worktree).await else {
            return Ok(worktree);
        };
        let path = Path::new(&worktree.path);
        let cleanup = self.repos.delete_worktree(repo_path, path, true).await;
        self.forget(path);
        Err(EngineError::Other(match cleanup {
            Ok(()) => format!("Couldn't prepare the new worktree, so it was removed: {error}"),
            Err(cleanup) => format!(
                "Couldn't prepare the new worktree at {}: {error}. Removing it also failed: {cleanup}",
                worktree.path
            ),
        }))
    }

    async fn prepare(
        &self,
        scope: &ProjectScope,
        environment: &WorktreeEnvironment,
        worktree: &Worktree,
    ) -> Result<(), String> {
        let path = records::canonical(Path::new(&worktree.path));
        let record = WorktreeRecord {
            path: path.to_string_lossy().into_owned(),
            repo_path: scope.root.to_string_lossy().into_owned(),
            space_id: Some(scope.space_id.clone()),
            space_path: Some(scope.space_path.to_string_lossy().into_owned()),
            slug: worktree.name.clone(),
            branch: worktree.branch.clone(),
            ports: None,
            sleeping: false,
            setup_steps: Vec::new(),
        };
        let records = self.repos.worktree_records();
        let record = records
            .insert(record, environment.ports.as_ref())
            .map_err(|error| error.to_string())?;
        let steps = self.context(&record).render_all(&environment.setup)?;
        records
            .update(&path, |record| record.setup_steps = steps)
            .map_err(|error| error.to_string())?;
        let (main, rules) = (scope.root.clone(), environment.copy.clone());
        tokio::task::spawn_blocking(move || copy::copy_untracked(&main, &path, &rules))
            .await
            .map_err(|error| format!("copying files stopped: {error}"))??;
        Ok(())
    }

    fn context(&self, record: &WorktreeRecord) -> WorktreeContext {
        let root = PathBuf::from(&record.repo_path);
        WorktreeContext {
            worktree: PathBuf::from(&record.path),
            project: root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            root,
            branch: record.branch.clone(),
            slug: record.slug.clone(),
            ports: record.ports.clone(),
        }
    }

    /// Start the new worktree's setup in one "(setup)" terminal: the
    /// environment's setup steps, then the project's setup Action. `None`
    /// when the project has neither.
    pub fn launch_setup(
        &self,
        scope: &ProjectScope,
        worktree: &Worktree,
        cols: u16,
        rows: u16,
    ) -> Result<Option<ProjectActionRun>, EngineError> {
        let action = self
            .project_actions
            .setup_action(&scope.space_id, &scope.space_path)?;
        let worktree_path = Path::new(&worktree.path);
        let record = self.repos.worktree_records().get(worktree_path);
        let environment = record
            .as_ref()
            .map(|record| self.context(record).environment())
            .unwrap_or_default();
        let launch =
            ActionLaunch::new(&scope.root, worktree_path, cols, rows).with_environment(environment);
        let Some(record) = record.filter(|record| !record.setup_steps.is_empty()) else {
            return action
                .map(|action| launch.run_setup(&self.terminals, &action))
                .transpose();
        };
        let command = self.setup_command(&record, &record.setup_steps, action.as_ref())?;
        let combined = ProjectAction {
            id: action
                .as_ref()
                .map_or_else(|| SETUP_ACTION_ID.to_string(), |action| action.id.clone()),
            name: action
                .as_ref()
                .map_or_else(|| "Environment".to_string(), |action| action.name.clone()),
            command,
            icon: ProjectActionIcon::Configure,
            run_on_worktree_create: true,
        };
        launch.run_setup(&self.terminals, &combined).map(Some)
    }

    #[cfg(unix)]
    fn setup_command(
        &self,
        record: &WorktreeRecord,
        steps: &[String],
        action: Option<&ProjectAction>,
    ) -> Result<String, EngineError> {
        let folder = self.state_dir(record);
        std::fs::create_dir_all(&folder)?;
        let status = folder.join("setup-status");
        let _ = std::fs::remove_file(&status);
        let script = folder.join("setup.sh");
        std::fs::write(&script, scripts::setup_script(steps, &status))?;
        let run = format!("/bin/sh {}", scripts::sh_quote(&script.to_string_lossy()));
        Ok(match action {
            Some(action) if !action.command.contains('\n') => {
                format!("{run} && {}", action.command)
            }
            Some(action) => format!("{run}\n{}", action.command),
            None => run,
        })
    }

    #[cfg(windows)]
    fn setup_command(
        &self,
        _record: &WorktreeRecord,
        steps: &[String],
        action: Option<&ProjectAction>,
    ) -> Result<String, EngineError> {
        let mut commands = steps.to_vec();
        commands.extend(action.map(|action| action.command.clone()));
        Ok(commands.join("\r\n"))
    }

    fn state_dir(&self, record: &WorktreeRecord) -> PathBuf {
        let digest = Sha256::digest(record.path.as_bytes());
        let id: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.repos.data_dir().join("worktree-env").join(id)
    }

    /// Remove a worktree: stop its terminals, run the environment's teardown,
    /// remove it, release its ports. A failed teardown doesn't keep the
    /// worktree; its error is returned so the caller can show it.
    pub async fn delete_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        force: bool,
    ) -> Result<Option<String>, EngineError> {
        self.repos.ensure_removable(worktree_path, force).await?;
        self.terminals.stop_within(worktree_path).await;
        let record = self.repos.worktree_records().get(worktree_path);
        let teardown_error = match &record {
            Some(record) if worktree_path.exists() => self.teardown(record).await.err(),
            _ => None,
        };
        self.repos
            .delete_worktree(repo_path, worktree_path, force)
            .await?;
        self.forget(worktree_path);
        if let Some(error) = &teardown_error {
            tracing::warn!(worktree = %worktree_path.display(), %error, "worktree teardown failed");
        }
        Ok(teardown_error)
    }

    async fn teardown(&self, record: &WorktreeRecord) -> Result<(), String> {
        let Some(environment) = self.record_environment(record) else {
            return Ok(());
        };
        if environment.teardown.is_empty() {
            return Ok(());
        }
        let mut context = self.context(record);
        if let Ok(branch) = self.repos.current_branch(Path::new(&record.path)).await {
            context.branch = branch;
        }
        let commands = context.render_all(&environment.teardown)?;
        scripts::run_headless(
            "Teardown",
            &commands,
            Path::new(&record.path),
            &context.environment(),
            scripts::TEARDOWN_STEP_TIMEOUT,
        )
        .await
    }

    fn forget(&self, worktree_path: &Path) {
        match self.repos.worktree_records().remove(worktree_path) {
            Ok(Some(record)) => {
                let _ = std::fs::remove_dir_all(self.state_dir(&record));
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(worktree = %worktree_path.display(), %error, "worktree record not removed");
            }
        }
    }

    /// Variables for processes started in `cwd`: empty outside a worktree
    /// Godspeed set up.
    pub fn environment_for(&self, cwd: &Path) -> BTreeMap<String, String> {
        self.repos
            .worktree_records()
            .containing(cwd)
            .map(|record| self.context(&record).environment())
            .unwrap_or_default()
    }

    pub fn status(&self, path: &Path) -> Option<WorktreeEnvironmentStatus> {
        let record = self.repos.worktree_records().containing(path)?;
        Some(self.status_of(&record))
    }

    fn status_of(&self, record: &WorktreeRecord) -> WorktreeEnvironmentStatus {
        let setup = (!record.setup_steps.is_empty())
            .then(|| {
                scripts::read_setup_state(
                    &self.state_dir(record).join("setup-status"),
                    &record.setup_steps,
                )
            })
            .flatten();
        WorktreeEnvironmentStatus {
            path: record.path.clone(),
            ports: record.ports.clone(),
            sleeping: record.sleeping,
            setup,
        }
    }

    pub fn is_sleeping(&self, path: &Path) -> bool {
        self.repos
            .worktree_records()
            .containing(path)
            .is_some_and(|record| record.sleeping)
    }

    /// Stop the terminals and dev servers Godspeed started in the worktree
    /// containing `path`, freeing their ports, and mark it asleep. Its port
    /// reservation stays, so waking brings back the same ports.
    pub async fn sleep(&self, path: &Path) -> Result<WorktreeEnvironmentStatus, EngineError> {
        let record = self.linked_record(path).await?;
        let root = PathBuf::from(&record.path);
        self.terminals.stop_within(&root).await;
        let record = self
            .repos
            .worktree_records()
            .update(&root, |record| record.sleeping = true)?
            .unwrap_or(record);
        Ok(self.status_of(&record))
    }

    /// Mark the worktree containing `path` awake and start its wake commands,
    /// if the project has any, in a "(wake)" terminal. Waking an awake
    /// worktree changes nothing.
    pub fn wake(
        &self,
        path: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<Option<WakeWorktreeOutcome>, EngineError> {
        let records = self.repos.worktree_records();
        let Some(record) = records.containing(path) else {
            return Ok(None);
        };
        if !record.sleeping {
            return Ok(Some(WakeWorktreeOutcome {
                status: self.status_of(&record),
                wake_action: None,
            }));
        }
        let root = PathBuf::from(&record.path);
        let record = records
            .update(&root, |record| record.sleeping = false)?
            .unwrap_or(record);
        let commands = self
            .record_environment(&record)
            .map(|environment| environment.wake)
            .unwrap_or_default();
        let wake_action = if commands.is_empty() || !root.is_dir() {
            None
        } else {
            let context = self.context(&record);
            let command = context
                .render_all(&commands)
                .map_err(EngineError::Other)?
                .join("\n");
            let action = ProjectAction {
                id: WAKE_ACTION_ID.into(),
                name: "Wake".into(),
                command,
                icon: ProjectActionIcon::Play,
                run_on_worktree_create: false,
            };
            Some(
                ActionLaunch::new(&context.root, &root, cols, rows)
                    .with_environment(context.environment())
                    .run(&self.terminals, &action)?,
            )
        };
        Ok(Some(WakeWorktreeOutcome {
            status: self.status_of(&record),
            wake_action,
        }))
    }

    /// The record of the linked worktree containing `path`, created bare
    /// (no ports) for worktrees made before their project had an environment.
    async fn linked_record(&self, path: &Path) -> Result<WorktreeRecord, EngineError> {
        let not_a_worktree = || EngineError::Other("Only a linked worktree can sleep".into());
        let identity = self.repos.checkout_identity(path).await?;
        let main = self
            .repos
            .main_checkout(&identity.root)
            .await
            .ok_or_else(not_a_worktree)?;
        let main = records::canonical(&main);
        if main == identity.root {
            return Err(not_a_worktree());
        }
        let records = self.repos.worktree_records();
        if let Some(record) = records.get(&identity.root) {
            return Ok(record);
        }
        let branch = self
            .repos
            .current_branch(&identity.root)
            .await
            .unwrap_or_default();
        let record = WorktreeRecord {
            path: identity.root.to_string_lossy().into_owned(),
            repo_path: main.to_string_lossy().into_owned(),
            space_id: None,
            space_path: None,
            slug: identity
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            branch,
            ports: None,
            sleeping: false,
            setup_steps: Vec::new(),
        };
        records.insert(record, None)
    }
}
