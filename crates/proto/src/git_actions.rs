//! Git write actions: commit, push, pull, sync, pull request creation, init
//! and publish. Every call runs on the engine that owns the checkout, so a
//! remote or headless engine answers exactly like the local one.

use serde::{Deserialize, Serialize};

/// The checked-out branch's relationship to its remote, read from local refs
/// only (callers fetch explicitly for fresher counts).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchStatus {
    /// `false` when the folder is not inside a Git work tree; every other
    /// field is then empty.
    pub is_repo: bool,
    /// The checked-out branch, `None` for a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// HEAD exists (the repository has at least one commit).
    #[serde(default)]
    pub has_commits: bool,
    /// Upstream as `remote/branch`, only when it tracks a branch of the same
    /// name. A worktree branch that merely started from `origin/main` has no
    /// push upstream: pushing it there would land feature work on main.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Commits on HEAD that the upstream lacks.
    #[serde(default)]
    pub ahead: usize,
    /// Commits on the upstream that HEAD lacks.
    #[serde(default)]
    pub behind: usize,
    /// The remote a first push would publish to, if any remote exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The remote is hosted on GitHub (pull requests are offered).
    #[serde(default)]
    pub github: bool,
    /// Uncommitted paths (tracked changes plus untracked files).
    #[serde(default)]
    pub changed_files: usize,
    /// A merge, rebase or cherry-pick is in progress or paths are unmerged.
    #[serde(default)]
    pub conflicted: bool,
}

/// `GitCommit` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitResult {
    pub sha: String,
    /// The first line of the recorded message.
    pub subject: String,
    /// The full recorded message (after credit stripping).
    pub message: String,
    /// The message was generated because the request left it empty.
    #[serde(default)]
    pub generated: bool,
    pub status: GitBranchStatus,
}

/// What a `GitPush`, `GitPull` or `GitSync` changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSyncResult {
    /// Commits integrated from the upstream (fast-forward only).
    #[serde(default)]
    pub pulled: usize,
    /// Commits sent to the remote.
    #[serde(default)]
    pub pushed: usize,
    /// This push created the upstream (`--set-upstream`).
    #[serde(default)]
    pub published_branch: bool,
    pub status: GitBranchStatus,
}

/// `GenerateCommitMessage` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedCommitMessage {
    pub message: String,
}

/// `GeneratePullRequest` reply: editable defaults for the create dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedPullRequest {
    pub title: String,
    pub body: String,
    /// The base the text was written against (the repository default unless
    /// the request named one).
    pub base: String,
    /// Repository-relative path of the pull request template that shaped the
    /// body, if one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// `CreatePullRequest` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedPullRequest {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    pub title: String,
    pub base: String,
    pub head: String,
    #[serde(default)]
    pub draft: bool,
    /// The branch was pushed first because the remote lacked its commits.
    #[serde(default)]
    pub pushed: bool,
}

/// `GenerateBranchName` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedBranchName {
    pub name: String,
}

/// `PublishRepository` visibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryVisibility {
    Public,
    #[default]
    Private,
}

/// `PublishRepository` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedRepository {
    pub url: String,
    pub status: GitBranchStatus,
}
