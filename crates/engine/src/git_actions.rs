//! Git write actions — commit, push, pull, sync, pull request creation, init
//! and publish — for one checkout on this device (T3 parity §12).
//!
//! Like the rest of the repository plumbing these shell out to `git` (never
//! libgit2) and to `gh` through [`GitHubCli`]. Ground rules:
//! - never force-push, never auto-merge: a pull only fast-forwards and a
//!   divergence is reported with its counts;
//! - a branch's push upstream is only the same-named remote branch. Worktree
//!   branches start from `origin/main` and may track it; pushing one "to its
//!   upstream" would land feature work on main, so such a branch is
//!   published to `<remote>/<branch>` instead;
//! - git runs with `LC_ALL=C` (stable stderr to classify) and
//!   `GIT_TERMINAL_PROMPT=0` (a missing credential fails fast instead of
//!   hanging on an invisible prompt);
//! - every failure maps to a [`GitActionError`] that says what to do next.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use zeron_proto::{
    CreatedPullRequest, GeneratedPullRequest, GitBranchStatus, GitCommitResult, GitSyncResult,
    PublishedRepository, RepositoryVisibility,
};

use crate::source_control::{GitHubCli, GitHubWriteError, PullRequestDraft};
use crate::text_generation::{self, PullRequestPrompt, TextGenerator};

/// Local operations (status, staging, commit hooks).
const LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Network operations (fetch, push).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

/// A failed git write action, phrased for the person who asked for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitActionError {
    #[error("Not a git repository. Initialize one with Init repository first.")]
    NotARepository,
    #[error("This folder is already inside a git repository ({0}).")]
    AlreadyRepository(String),
    #[error("Nothing to commit: the working tree has no changes.")]
    NothingToCommit,
    #[error("The commit message is empty. Write one, or leave it blank to generate one.")]
    EmptyMessage,
    #[error(
        "Couldn't generate a commit message: no text model is available. Write a message, or pick a model in Settings → Thread naming."
    )]
    GenerationUnavailable,
    #[error("HEAD is detached. Check out a branch before pushing or pulling.")]
    DetachedHead,
    #[error("This repository has no commits yet. Make a first commit, then try again.")]
    NoCommits,
    #[error("This repository has no remote. Publish it to GitHub or add a remote first.")]
    NoRemote,
    #[error("This repository already has a remote ({0}); it is already published.")]
    RemoteExists(String),
    #[error("Branch {0} has no upstream yet. Push it first to publish it.")]
    NoUpstream(String),
    #[error(
        "Authentication with the remote failed. Run `gh auth login` (or check your SSH keys / credential helper), then try again."
    )]
    AuthFailed,
    #[error("Couldn't reach the remote: {0}")]
    RemoteUnavailable(String),
    #[error(
        "Push rejected: the remote has commits this branch doesn't. Pull (or Sync) first, then push again."
    )]
    NonFastForward,
    #[error(
        "{branch} and {upstream} have diverged ({ahead} local and {behind} remote commits). Merge or rebase in a terminal; Godspeed only fast-forwards."
    )]
    Diverged {
        branch: String,
        upstream: String,
        ahead: usize,
        behind: usize,
    },
    #[error("Local changes would be overwritten by the pull: {0}. Commit or discard them first.")]
    LocalChangesBlock(String),
    #[error("The repository has unresolved merge conflicts. Resolve them, then try again.")]
    MergeConflict,
    #[error(
        "Git doesn't know who you are. Run `git config --global user.name \"Your Name\"` and `git config --global user.email you@example.com`, then commit again."
    )]
    MissingIdentity,
    #[error("You're on the base branch {0}. Switch to a feature branch to open a pull request.")]
    OnBaseBranch(String),
    #[error("{0}")]
    GitHub(#[from] GitHubWriteError),
    #[error("git: {0}")]
    Git(String),
}

/// Raw result of one git invocation.
struct GitOutput {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl GitOutput {
    fn stdout_trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// One git invocation. `index` points `GIT_INDEX_FILE` elsewhere (a
/// throwaway index for previewing what a commit would contain).
async fn run_git(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    index: Option<&Path>,
    timeout: Duration,
) -> Result<GitOutput, GitActionError> {
    let mut cmd = tokio::process::Command::new(crate::git_program::git_program());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x08000000);
        cmd.args(["-c", "core.longpaths=true"]);
    }
    cmd.args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| GitActionError::Git(format!("could not start git: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(OUTPUT_LIMIT as u64);
    let mut stderr = child
        .stderr
        .take()
        .expect("piped stderr")
        .take(OUTPUT_LIMIT as u64);
    let input = child.stdin.take().zip(stdin.map(<[u8]>::to_vec));
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (status, _, _, ()) = tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            async {
                if let Some((mut pipe, bytes)) = input {
                    // A child that exits without reading reports via status.
                    let _ = pipe.write_all(&bytes).await;
                }
                Ok(())
            },
        )?;
        Ok::<_, std::io::Error>((status, out, err))
    };
    let (status, out, err) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| GitActionError::Git(format!("git {} timed out", args.join(" "))))?
        .map_err(|e| GitActionError::Git(format!("git {}: {e}", args.join(" "))))?;
    Ok(GitOutput {
        ok: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

/// `run_git` that must succeed; failures become [`GitActionError::Git`].
async fn git_ok(cwd: &Path, args: &[&str]) -> Result<String, GitActionError> {
    let out = run_git(cwd, args, None, None, LOCAL_TIMEOUT).await?;
    if out.ok {
        Ok(out.stdout_trimmed().to_string())
    } else {
        Err(GitActionError::Git(git_detail(&out.stderr)))
    }
}

/// `run_git` whose failure just means "no".
async fn git_value(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = run_git(cwd, args, None, None, LOCAL_TIMEOUT).await.ok()?;
    let value = out.stdout_trimmed();
    (out.ok && !value.is_empty()).then(|| value.to_string())
}

/// git's explanation without hint lines, bounded for a dialog.
fn git_detail(stderr: &str) -> String {
    let detail = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .map(|line| line.strip_prefix("fatal: ").unwrap_or(line))
        .map(|line| line.strip_prefix("error: ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join(" ");
    let detail: String = detail.chars().take(500).collect();
    if detail.is_empty() {
        "the command failed without an explanation".into()
    } else {
        detail
    }
}

fn is_auth_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied (publickey",
        "terminal prompts disabled",
        "invalid username or password",
        "returned error: 403",
        "returned error: 401",
        "the requested url returned error: 403",
        "support for password authentication was removed",
        "host key verification failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Classify a failed fetch/push/pull network step.
fn remote_failure(stderr: &str) -> GitActionError {
    let lower = stderr.to_ascii_lowercase();
    if is_auth_failure(stderr) {
        GitActionError::AuthFailed
    } else if lower.contains("[rejected]")
        && (lower.contains("non-fast-forward")
            || lower.contains("fetch first")
            || lower.contains("stale info"))
    {
        GitActionError::NonFastForward
    } else if lower.contains("could not resolve host")
        || lower.contains("could not read from remote repository")
        || lower.contains("does not appear to be a git repository")
        || lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("repository not found")
    {
        GitActionError::RemoteUnavailable(git_detail(stderr))
    } else {
        GitActionError::Git(git_detail(stderr))
    }
}

/// Paths git listed under "would be overwritten" (tab-indented lines).
fn overwritten_paths(stderr: &str) -> String {
    let paths: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with('\t'))
        .map(str::trim)
        .take(5)
        .collect();
    if paths.is_empty() {
        "some files".into()
    } else {
        paths.join(", ")
    }
}

/// Git write actions on checkouts of this device.
#[derive(Clone)]
pub struct GitActions {
    github: GitHubCli,
    text: TextGenerator,
}

impl GitActions {
    pub fn new(github: GitHubCli, text: TextGenerator) -> Self {
        Self { github, text }
    }

    // ── status ──────────────────────────────────────────────────────────────

    /// Branch, upstream and remote facts from local refs only.
    pub async fn status(&self, cwd: &Path) -> Result<GitBranchStatus, GitActionError> {
        if git_value(cwd, &["rev-parse", "--is-inside-work-tree"]).await.as_deref() != Some("true")
        {
            return Ok(GitBranchStatus::default());
        }
        let branch = git_value(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
        let has_commits = git_value(cwd, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await
            .is_some();
        let upstream = match &branch {
            Some(branch) => self.push_upstream(cwd, branch).await,
            None => None,
        };
        let (ahead, behind) = match (&upstream, has_commits) {
            (Some(_), true) => counts(cwd, "HEAD...@{u}").await.unwrap_or((0, 0)),
            // No upstream: commits no remote has seen yet.
            (None, true) => {
                let unpublished = git_value(cwd, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
                    .await
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                (unpublished, 0)
            }
            _ => (0, 0),
        };
        let remote = self.push_remote(cwd, branch.as_deref()).await;
        // Any remote name (not just `origin`), and an SSH host alias for
        // GitHub (`git@github-work:…`) counts as GitHub.
        let github = match &remote {
            Some(remote) => match git_value(cwd, &["remote", "get-url", remote]).await {
                Some(url) => crate::source_control::remote_is_github(&url).await,
                None => false,
            },
            None => false,
        };
        let porcelain = git_value(cwd, &["status", "--porcelain=v1"]).await.unwrap_or_default();
        let changed_files = porcelain.lines().filter(|l| !l.trim().is_empty()).count();
        let unmerged = porcelain.lines().any(|line| {
            let code = line.get(..2).unwrap_or("");
            code.contains('U') || code == "AA" || code == "DD"
        });
        let conflicted = unmerged || self.operation_in_progress(cwd).await;
        Ok(GitBranchStatus {
            is_repo: true,
            branch,
            has_commits,
            upstream,
            ahead,
            behind,
            remote,
            github,
            changed_files,
            conflicted,
        })
    }

    /// `remote/branch` when `branch` tracks the same-named branch of a remote
    /// whose tracking ref exists locally.
    async fn push_upstream(&self, cwd: &Path, branch: &str) -> Option<String> {
        let remote = git_value(cwd, &["config", &format!("branch.{branch}.remote")]).await?;
        let merge = git_value(cwd, &["config", &format!("branch.{branch}.merge")]).await?;
        if remote == "." || merge != format!("refs/heads/{branch}") {
            return None;
        }
        let upstream = format!("{remote}/{branch}");
        git_value(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{upstream}"),
            ],
        )
        .await
        .map(|_| upstream)
    }

    /// The remote a push of `branch` goes to: its pushRemote, the repo's
    /// pushDefault, its tracking remote, `origin`, else the only remote.
    async fn push_remote(&self, cwd: &Path, branch: Option<&str>) -> Option<String> {
        let remotes: Vec<String> = git_value(cwd, &["remote"])
            .await
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        if remotes.is_empty() {
            return None;
        }
        let mut candidates = Vec::new();
        if let Some(branch) = branch {
            candidates.push(git_value(cwd, &["config", &format!("branch.{branch}.pushRemote")]).await);
        }
        candidates.push(git_value(cwd, &["config", "remote.pushDefault"]).await);
        if let Some(branch) = branch {
            candidates.push(git_value(cwd, &["config", &format!("branch.{branch}.remote")]).await);
        }
        candidates.push(Some("origin".into()));
        candidates
            .into_iter()
            .flatten()
            .find(|name| remotes.contains(name))
            .or_else(|| (remotes.len() == 1).then(|| remotes[0].clone()))
    }

    async fn operation_in_progress(&self, cwd: &Path) -> bool {
        let Some(git_dir) = git_value(cwd, &["rev-parse", "--absolute-git-dir"]).await else {
            return false;
        };
        let git_dir = PathBuf::from(git_dir);
        ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply"]
            .iter()
            .any(|name| git_dir.join(name).exists())
    }

    async fn require_repo(&self, cwd: &Path) -> Result<GitBranchStatus, GitActionError> {
        let status = self.status(cwd).await?;
        if !status.is_repo {
            return Err(GitActionError::NotARepository);
        }
        Ok(status)
    }

    // ── commit ──────────────────────────────────────────────────────────────

    /// Stage all changes (or exactly `files`) and commit them. An empty
    /// `message` is generated from the staged diff; agent credits are
    /// stripped from any message when the device setting asks for it.
    pub async fn commit(
        &self,
        cwd: &Path,
        message: &str,
        files: &[String],
    ) -> Result<GitCommitResult, GitActionError> {
        let status = self.require_repo(cwd).await?;
        if status.conflicted {
            return Err(GitActionError::MergeConflict);
        }
        let paths = pathspec(files);
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        if paths.is_empty() {
            add.pop();
        }
        git_ok(cwd, &add).await?;

        let mut staged = vec!["diff", "--cached", "--quiet", "--"];
        staged.extend(paths.iter().map(String::as_str));
        let probe = run_git(cwd, &staged, None, None, LOCAL_TIMEOUT).await?;
        match probe.code {
            Some(0) => return Err(GitActionError::NothingToCommit),
            Some(1) => {}
            _ => return Err(GitActionError::Git(git_detail(&probe.stderr))),
        }

        let (message, generated) = if message.trim().is_empty() {
            let diff = self.staged_diff(cwd, &paths, None).await?;
            let subjects = recent_subjects(cwd).await;
            let generated = self
                .text
                .commit_message(&diff, &subjects)
                .await
                .ok_or(GitActionError::GenerationUnavailable)?;
            (generated, true)
        } else {
            (self.text.finish(message), false)
        };
        if message.trim().is_empty() {
            return Err(GitActionError::EmptyMessage);
        }

        let mut commit = vec!["commit", "--quiet", "-F", "-"];
        if !paths.is_empty() {
            // Only the chosen paths, even if others were staged before.
            commit.push("--");
            commit.extend(paths.iter().map(String::as_str));
        }
        let out = run_git(cwd, &commit, Some(message.as_bytes()), None, LOCAL_TIMEOUT).await?;
        if !out.ok {
            let lower = out.stderr.to_ascii_lowercase();
            return Err(
                if lower.contains("please tell me who you are")
                    || lower.contains("unable to auto-detect email")
                    || lower.contains("empty ident name")
                {
                    GitActionError::MissingIdentity
                } else if lower.contains("nothing to commit") || lower.contains("nothing added to commit") {
                    GitActionError::NothingToCommit
                } else {
                    GitActionError::Git(git_detail(&format!("{}\n{}", out.stderr, out.stdout)))
                },
            );
        }
        let sha = git_ok(cwd, &["rev-parse", "HEAD"]).await?;
        let recorded = git_ok(cwd, &["log", "-1", "--format=%B"]).await?;
        Ok(GitCommitResult {
            sha,
            subject: recorded.lines().next().unwrap_or("").to_string(),
            message: recorded,
            generated,
            status: self.status(cwd).await?,
        })
    }

    /// The patch a commit of `paths` (all changes when empty) would record,
    /// computed on a throwaway copy of the index so nothing is staged.
    async fn pending_diff(&self, cwd: &Path, paths: &[String]) -> Result<String, GitActionError> {
        let index_path = git_ok(cwd, &["rev-parse", "--git-path", "index"]).await?;
        let index_path = cwd.join(index_path);
        let scratch = tempfile::tempdir()
            .map_err(|e| GitActionError::Git(format!("temporary index: {e}")))?;
        let temp_index = scratch.path().join("index");
        if index_path.exists() {
            std::fs::copy(&index_path, &temp_index)
                .map_err(|e| GitActionError::Git(format!("temporary index: {e}")))?;
        }
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        if paths.is_empty() {
            add.pop();
        }
        let out = run_git(cwd, &add, None, Some(&temp_index), LOCAL_TIMEOUT).await?;
        if !out.ok {
            return Err(GitActionError::Git(git_detail(&out.stderr)));
        }
        self.staged_diff(cwd, paths, Some(&temp_index)).await
    }

    async fn staged_diff(
        &self,
        cwd: &Path,
        paths: &[String],
        index: Option<&Path>,
    ) -> Result<String, GitActionError> {
        let mut args = vec![
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let out = run_git(cwd, &args, None, index, LOCAL_TIMEOUT).await?;
        if out.ok {
            Ok(out.stdout)
        } else {
            Err(GitActionError::Git(git_detail(&out.stderr)))
        }
    }

    /// Generate a message for what committing `files` (all when empty) would
    /// record. Nothing is staged.
    pub async fn generate_commit_message(
        &self,
        cwd: &Path,
        files: &[String],
    ) -> Result<String, GitActionError> {
        let status = self.require_repo(cwd).await?;
        if status.changed_files == 0 {
            return Err(GitActionError::NothingToCommit);
        }
        let diff = self.pending_diff(cwd, &pathspec(files)).await?;
        if diff.trim().is_empty() {
            return Err(GitActionError::NothingToCommit);
        }
        let subjects = recent_subjects(cwd).await;
        self.text
            .commit_message(&diff, &subjects)
            .await
            .ok_or(GitActionError::GenerationUnavailable)
    }

    // ── push / pull / sync ──────────────────────────────────────────────────

    /// Push the current branch. Without a same-named upstream the branch is
    /// published (`--set-upstream`). Never forces.
    pub async fn push(&self, cwd: &Path) -> Result<GitSyncResult, GitActionError> {
        let status = self.require_repo(cwd).await?;
        let branch = status.branch.clone().ok_or(GitActionError::DetachedHead)?;
        if !status.has_commits {
            return Err(GitActionError::NoCommits);
        }
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let (remote, publish) = match &status.upstream {
            Some(upstream) => (
                upstream
                    .strip_suffix(&format!("/{branch}"))
                    .unwrap_or(upstream)
                    .to_string(),
                false,
            ),
            None => (status.remote.clone().ok_or(GitActionError::NoRemote)?, true),
        };
        let mut args = vec!["push", "--porcelain"];
        if publish {
            args.push("--set-upstream");
        }
        args.push(&remote);
        args.push(&refspec);
        let out = run_git(cwd, &args, None, None, NETWORK_TIMEOUT).await?;
        if !out.ok {
            // `--porcelain` reports rejections on stdout.
            return Err(remote_failure(&format!("{}\n{}", out.stderr, out.stdout)));
        }
        Ok(GitSyncResult {
            pulled: 0,
            pushed: status.ahead,
            published_branch: publish,
            status: self.status(cwd).await?,
        })
    }

    /// Fetch the upstream's remote and fast-forward to it. A divergence or a
    /// blocking local change is an error; nothing is ever merged.
    pub async fn pull(&self, cwd: &Path) -> Result<GitSyncResult, GitActionError> {
        let status = self.require_repo(cwd).await?;
        let branch = status.branch.clone().ok_or(GitActionError::DetachedHead)?;
        if status.conflicted {
            return Err(GitActionError::MergeConflict);
        }
        let upstream = status
            .upstream
            .clone()
            .ok_or_else(|| GitActionError::NoUpstream(branch.clone()))?;
        let remote = upstream
            .strip_suffix(&format!("/{branch}"))
            .unwrap_or(&upstream)
            .to_string();
        let fetch = run_git(cwd, &["fetch", "--quiet", &remote], None, None, NETWORK_TIMEOUT).await?;
        if !fetch.ok {
            return Err(remote_failure(&fetch.stderr));
        }
        let (ahead, behind) = counts(cwd, &format!("HEAD...{upstream}"))
            .await
            .unwrap_or((0, 0));
        if behind == 0 {
            return Ok(GitSyncResult {
                pulled: 0,
                pushed: 0,
                published_branch: false,
                status: self.status(cwd).await?,
            });
        }
        if ahead > 0 {
            return Err(GitActionError::Diverged {
                branch,
                upstream,
                ahead,
                behind,
            });
        }
        let merge = run_git(
            cwd,
            &["merge", "--ff-only", "--quiet", &upstream],
            None,
            None,
            LOCAL_TIMEOUT,
        )
        .await?;
        if !merge.ok {
            let lower = merge.stderr.to_ascii_lowercase();
            return Err(if lower.contains("would be overwritten") {
                GitActionError::LocalChangesBlock(overwritten_paths(&merge.stderr))
            } else if lower.contains("not possible to fast-forward") {
                GitActionError::Diverged {
                    branch,
                    upstream,
                    ahead,
                    behind,
                }
            } else if lower.contains("conflict") || lower.contains("unmerged") {
                GitActionError::MergeConflict
            } else {
                GitActionError::Git(git_detail(&merge.stderr))
            });
        }
        Ok(GitSyncResult {
            pulled: behind,
            pushed: 0,
            published_branch: false,
            status: self.status(cwd).await?,
        })
    }

    /// Pull (fast-forward) then push. A branch without an upstream is
    /// simply published.
    pub async fn sync(&self, cwd: &Path) -> Result<GitSyncResult, GitActionError> {
        let status = self.require_repo(cwd).await?;
        if status.upstream.is_none() {
            return self.push(cwd).await;
        }
        let pulled = self.pull(cwd).await?;
        if pulled.status.ahead == 0 {
            return Ok(pulled);
        }
        let pushed = self.push(cwd).await?;
        Ok(GitSyncResult {
            pulled: pulled.pulled,
            ..pushed
        })
    }

    // ── pull requests ───────────────────────────────────────────────────────

    /// The base a pull request targets by default: the remote's HEAD branch,
    /// then `gh`'s view of the repository, then `main`/`master`.
    async fn default_base(&self, cwd: &Path, remote: &str) -> String {
        if let Some(head) = git_value(
            cwd,
            &["symbolic-ref", "--quiet", "--short", &format!("refs/remotes/{remote}/HEAD")],
        )
        .await
            && let Some(branch) = head.strip_prefix(&format!("{remote}/"))
        {
            return branch.to_string();
        }
        if let Some(branch) = self.github.checkout_default_branch(cwd).await {
            return branch;
        }
        for candidate in ["main", "master"] {
            if git_value(
                cwd,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/{remote}/{candidate}"),
                ],
            )
            .await
            .is_some()
            {
                return candidate.to_string();
            }
        }
        "main".to_string()
    }

    /// Commits and diff of HEAD against `remote/base` (falling back to the
    /// local base branch when the remote ref is missing).
    async fn branch_changes(&self, cwd: &Path, remote: &str, base: &str) -> (Vec<String>, String) {
        let remote_ref = format!("{remote}/{base}");
        let base_ref = if git_value(
            cwd,
            &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{remote_ref}")],
        )
        .await
        .is_some()
        {
            remote_ref
        } else {
            base.to_string()
        };
        let commits = git_value(
            cwd,
            &["log", "--reverse", "--format=%s", &format!("{base_ref}..HEAD")],
        )
        .await
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default();
        let diff = run_git(
            cwd,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--find-renames",
                &format!("{base_ref}...HEAD"),
            ],
            None,
            None,
            LOCAL_TIMEOUT,
        )
        .await
        .ok()
        .filter(|out| out.ok)
        .map(|out| out.stdout)
        .unwrap_or_default();
        (commits, diff)
    }

    /// Title and body for a pull request from the current branch into `base`
    /// (the repository default when `None`), following the repository's PR
    /// template when it has one. Falls back to the commit list when no text
    /// model is available.
    pub async fn generate_pull_request(
        &self,
        cwd: &Path,
        base: Option<&str>,
    ) -> Result<GeneratedPullRequest, GitActionError> {
        let status = self.require_repo(cwd).await?;
        let head = status.branch.clone().ok_or(GitActionError::DetachedHead)?;
        let remote = status.remote.clone().ok_or(GitActionError::NoRemote)?;
        let base = match base.filter(|b| !b.trim().is_empty()) {
            Some(base) => base.to_string(),
            None => self.default_base(cwd, &remote).await,
        };
        let root = git_ok(cwd, &["rev-parse", "--show-toplevel"]).await?;
        let template = text_generation::find_pr_template(Path::new(&root));
        let (commits, diff) = self.branch_changes(cwd, &remote, &base).await;
        let input = PullRequestPrompt {
            base: base.clone(),
            head: head.clone(),
            commits: commits.clone(),
            diff,
            style_subjects: recent_subjects(cwd).await,
            template: template.as_ref().map(|(_, text)| text.clone()),
        };
        let (title, body) = match self.text.pull_request(&input).await {
            Some(generated) => generated,
            None => fallback_pull_request(&head, &commits, template.as_ref().map(|(_, t)| t.as_str())),
        };
        Ok(GeneratedPullRequest {
            title,
            body: self.text.finish(&body),
            base,
            template: template.map(|(path, _)| path),
        })
    }

    /// Push if the remote lacks the branch's commits, then open the pull
    /// request with `gh`. Empty title or body are generated.
    pub async fn create_pull_request(
        &self,
        cwd: &Path,
        title: &str,
        body: &str,
        base: Option<&str>,
        draft: bool,
    ) -> Result<CreatedPullRequest, GitActionError> {
        let status = self.require_repo(cwd).await?;
        let head = status.branch.clone().ok_or(GitActionError::DetachedHead)?;
        let remote = status.remote.clone().ok_or(GitActionError::NoRemote)?;
        if !status.has_commits {
            return Err(GitActionError::NoCommits);
        }
        let base = match base.filter(|b| !b.trim().is_empty()) {
            Some(base) => base.to_string(),
            None => self.default_base(cwd, &remote).await,
        };
        if base == head {
            return Err(GitActionError::OnBaseBranch(base));
        }
        let pushed = if status.upstream.is_none() || status.ahead > 0 {
            self.push(cwd).await?;
            true
        } else {
            false
        };
        let (title, body) = if title.trim().is_empty() || body.trim().is_empty() {
            let generated = self.generate_pull_request(cwd, Some(&base)).await?;
            (
                if title.trim().is_empty() {
                    generated.title
                } else {
                    title.trim().to_string()
                },
                if body.trim().is_empty() {
                    generated.body
                } else {
                    body.to_string()
                },
            )
        } else {
            (title.trim().to_string(), body.to_string())
        };
        let title = self.text.finish(&title);
        let body = self.text.finish(&body);
        let url = self
            .github
            .create_pull_request(
                cwd,
                &PullRequestDraft {
                    title: title.clone(),
                    body,
                    base: base.clone(),
                    head: head.clone(),
                    draft,
                },
            )
            .await?;
        let number = url
            .rsplit('/')
            .next()
            .and_then(|segment| segment.parse().ok());
        Ok(CreatedPullRequest {
            url,
            number,
            title,
            base,
            head,
            draft,
            pushed,
        })
    }

    /// A branch name for work described by `description`, or, when empty,
    /// by the pending changes in `cwd`.
    pub async fn generate_branch_name(
        &self,
        cwd: Option<&Path>,
        description: &str,
    ) -> Result<String, GitActionError> {
        let description = if description.trim().is_empty() {
            let cwd = cwd.ok_or(GitActionError::NothingToCommit)?;
            let diff = self.pending_diff(cwd, &[]).await?;
            text_generation::summarize_diff(&diff, text_generation::DIFF_PROMPT_BUDGET)
        } else {
            description.to_string()
        };
        self.text
            .branch_name(&description)
            .await
            .ok_or(GitActionError::GenerationUnavailable)
    }

    // ── init / publish ──────────────────────────────────────────────────────

    /// `git init` an existing folder that is not inside a repository yet.
    pub async fn init(&self, path: &Path) -> Result<GitBranchStatus, GitActionError> {
        if !path.is_dir() {
            return Err(GitActionError::Git(format!(
                "no such folder: {}",
                path.display()
            )));
        }
        if let Some(root) = git_value(path, &["rev-parse", "--show-toplevel"]).await {
            return Err(GitActionError::AlreadyRepository(root));
        }
        git_ok(path, &["init", "--quiet"]).await?;
        self.status(path).await
    }

    /// Create a GitHub repository from this checkout and push it — only ever
    /// on an explicit request. `name` defaults to the folder name.
    pub async fn publish(
        &self,
        cwd: &Path,
        name: Option<&str>,
        visibility: RepositoryVisibility,
    ) -> Result<PublishedRepository, GitActionError> {
        let status = self.require_repo(cwd).await?;
        if let Some(remote) = &status.remote {
            let url = git_value(cwd, &["remote", "get-url", remote])
                .await
                .unwrap_or_default();
            return Err(GitActionError::RemoteExists(
                format!("{remote} {url}").trim().to_string(),
            ));
        }
        if !status.has_commits {
            return Err(GitActionError::NoCommits);
        }
        let root = git_ok(cwd, &["rev-parse", "--show-toplevel"]).await?;
        let root = PathBuf::from(root);
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                repository_name(
                    &root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            });
        let url = self
            .github
            .create_repository(&root, &name, visibility == RepositoryVisibility::Private)
            .await?;
        Ok(PublishedRepository {
            url,
            status: self.status(cwd).await?,
        })
    }
}

/// `--left-right --count` of a symmetric range: `(ahead, behind)`.
async fn counts(cwd: &Path, range: &str) -> Option<(usize, usize)> {
    let out = git_value(cwd, &["rev-list", "--left-right", "--count", range]).await?;
    let mut parts = out.split_whitespace();
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Recent subjects on HEAD (empty for a fresh repository).
async fn recent_subjects(cwd: &Path) -> Vec<String> {
    git_value(
        cwd,
        &[
            "log",
            &format!("-{}", text_generation::STYLE_SUBJECTS),
            "--no-merges",
            "--format=%s",
        ],
    )
    .await
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default()
}

/// Literal pathspecs: `:(literal)` keeps names with glob characters exact.
fn pathspec(files: &[String]) -> Vec<String> {
    files
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .map(|f| format!(":(literal){f}"))
        .collect()
}

/// A GitHub-safe repository name from a folder name.
fn repository_name(folder: &str) -> String {
    let mut name = String::new();
    for c in folder.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            name.push(c);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches(['-', '.']).to_string();
    if name.is_empty() {
        "repository".into()
    } else {
        name
    }
}

/// Text for a pull request when no model is available: the only commit's
/// subject (or the branch name) as title; the template, else the commits.
fn fallback_pull_request(head: &str, commits: &[String], template: Option<&str>) -> (String, String) {
    let title = match commits {
        [only] => only.clone(),
        _ => {
            let tail = head.rsplit('/').next().unwrap_or(head).replace(['-', '_'], " ");
            let mut chars = tail.trim().chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => head.to_string(),
            }
        }
    };
    let body = match template {
        Some(template) => template.to_string(),
        None => commits
            .iter()
            .map(|subject| format!("- {subject}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_failures_are_classified() {
        assert_eq!(
            remote_failure("fatal: Authentication failed for 'https://github.com/a/b.git/'"),
            GitActionError::AuthFailed
        );
        assert_eq!(
            remote_failure("git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository."),
            GitActionError::AuthFailed
        );
        assert_eq!(
            remote_failure("To /tmp/remote.git\n!\trefs/heads/main:refs/heads/main\t[rejected] (non-fast-forward)\nDone"),
            GitActionError::NonFastForward
        );
        assert!(matches!(
            remote_failure("fatal: '/nope' does not appear to be a git repository"),
            GitActionError::RemoteUnavailable(_)
        ));
    }

    #[test]
    fn names_and_fallbacks() {
        assert_eq!(repository_name("My Project!"), "My-Project");
        assert_eq!(repository_name("..."), "repository");
        assert_eq!(pathspec(&[" a b.txt ".into(), "".into()]), vec![":(literal)a b.txt"]);
        let (title, body) =
            fallback_pull_request("godspeed/fix-login", &["Fix login".into(), "Add test".into()], None);
        assert_eq!(title, "Fix login");
        assert_eq!(body, "- Fix login\n- Add test");
        let (title, body) = fallback_pull_request("x", &["Only one".into()], Some("## T"));
        assert_eq!((title.as_str(), body.as_str()), ("Only one", "## T"));
    }

    #[test]
    fn git_detail_drops_hints_and_prefixes() {
        assert_eq!(
            git_detail("hint: try this\nfatal: bad thing\nerror: worse\n"),
            "bad thing worse"
        );
        assert_eq!(overwritten_paths("error: ...\n\ta.txt\n\tb.txt\nPlease commit"), "a.txt, b.txt");
    }
}
