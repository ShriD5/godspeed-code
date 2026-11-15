//! Git write actions over RPC (`GitCommit`, `GitPush`, `GitPull`, `GitSync`,
//! `GitInit`, `GitBranchStatus`, generation, `CreatePullRequest`,
//! `PublishRepository`). All are relay-forwardable: the checkout's own
//! engine runs them. Like the pull request reads, a caller may only address
//! a known chat or space checkout on this device — never arbitrary folders.

use super::*;
use crate::git_actions::{GitActionError, GitActions};
use crate::text_generation::TextGenerator;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GIT_BRANCH_STATUS
            | methods::GIT_COMMIT
            | methods::GIT_PUSH
            | methods::GIT_PULL
            | methods::GIT_SYNC
            | methods::GIT_INIT
            | methods::GENERATE_COMMIT_MESSAGE
            | methods::GENERATE_PULL_REQUEST
            | methods::GENERATE_BRANCH_NAME
            | methods::CREATE_PULL_REQUEST
            | methods::PUBLISH_REPOSITORY
    )
}

/// Relay deadline: network writes and model runs outlast the 30 s default.
pub(super) fn deadline(method: &str) -> Duration {
    match method {
        methods::GIT_BRANCH_STATUS | methods::GIT_INIT => Duration::from_secs(30),
        methods::PUBLISH_REPOSITORY => Duration::from_secs(11 * 60),
        _ => Duration::from_secs(6 * 60),
    }
}

fn failed(error: GitActionError) -> RpcError {
    RpcError::Failed(error.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CwdParams {
    cwd: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitParams {
    cwd: String,
    #[serde(default)]
    message: String,
    /// Commit exactly these repository-relative paths; empty = everything.
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateCommitParams {
    cwd: String,
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestParams {
    cwd: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchNameParams {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishParams {
    cwd: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    visibility: zeron_proto::RepositoryVisibility,
}

impl EngineRpc {
    /// Git actions for `root`, generating text with the device's configured
    /// model or else the agent of a chat on that checkout (as titles do).
    async fn git_actions(&self, root: Option<&std::path::Path>) -> GitActions {
        let session_harness = match root {
            Some(root) => self.checkout_harness(root).await,
            None => None,
        };
        GitActions::new(
            self.github.clone(),
            TextGenerator::new(self.registry.clone()).with_session_harness(session_harness),
        )
    }

    /// The agent of the most recently active chat on `root`. A just-created
    /// chat row may still be on its way into the chats watch: wait briefly.
    async fn checkout_harness(&self, root: &std::path::Path) -> Option<HarnessId> {
        let local = self.doc_host.device_id().to_string();
        let find = |chats: &[zeron_proto::Chat]| {
            chats
                .iter()
                .filter(|chat| chat.device_id == local && !chat.archived)
                .filter(|chat| chat.cwd.as_deref().map(std::path::Path::new) == Some(root))
                .max_by_key(|chat| chat.last_message_at)
                .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
        };
        let mut chats_rx = self.workspace.watch_chats();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        loop {
            if let Some(harness) = find(&chats_rx.borrow_and_update()) {
                return Some(harness);
            }
            tokio::select! {
                changed = chats_rx.changed() => if changed.is_err() { return None; },
                _ = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }

    /// A chat or space checkout on this device, including a space folder
    /// that is not a repository yet (the Init action targets those).
    async fn git_action_root(&self, cwd: &str) -> Result<std::path::PathBuf, RpcError> {
        if let Ok(root) = self.change_request_root(cwd).await {
            return Ok(root);
        }
        let requested = std::path::Path::new(cwd);
        let local = self.doc_host.device_id();
        let known = self
            .workspace
            .watch_spaces()
            .borrow()
            .iter()
            .any(|space| space.device_id == local && std::path::Path::new(&space.path) == requested);
        if known {
            Ok(requested.to_path_buf())
        } else {
            Err(RpcError::BadParams(
                "cwd is not a known checkout on this device".into(),
            ))
        }
    }

    /// A chat working in `root` opened `url`: watch it by default.
    fn watch_created_pull_request(&self, root: &std::path::Path, url: &str) {
        let Some(watch) = &self.pr_watch else {
            return;
        };
        let local = self.doc_host.device_id();
        let chat = self
            .workspace
            .watch_chats()
            .borrow()
            .iter()
            .filter(|chat| chat.device_id == local && !chat.archived)
            .filter(|chat| chat.cwd.as_deref().map(std::path::Path::new) == Some(root))
            .max_by_key(|chat| chat.last_message_at)
            .map(|chat| chat.id.clone());
        if let Some(chat_id) = chat {
            watch.register_automatic(url, &chat_id);
        }
    }

    /// After a write: refresh diff watchers (HEAD and refs moved) and the
    /// checkout's pull request badge.
    async fn after_git_write(&self, root: &std::path::Path) {
        self.diff_sync.sync_all();
        if let Ok(identity) = self.repos.checkout_identity(root).await {
            self.change_requests.refresh_checkout(&identity.id);
        }
    }

    pub(super) async fn git_action(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        match method {
            methods::GIT_BRANCH_STATUS => {
                let p: CwdParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(None).await;
                RpcReply::value(&git.status(&root).await.map_err(failed)?)
            }
            methods::GIT_COMMIT => {
                let p: CommitParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(Some(&root)).await;
                let result = git
                    .commit(&root, &p.message, &p.files)
                    .await
                    .map_err(failed)?;
                self.after_git_write(&root).await;
                RpcReply::value(&result)
            }
            methods::GIT_PUSH | methods::GIT_PULL | methods::GIT_SYNC => {
                let p: CwdParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(None).await;
                let result = match method {
                    methods::GIT_PUSH => git.push(&root).await,
                    methods::GIT_PULL => git.pull(&root).await,
                    _ => git.sync(&root).await,
                }
                .map_err(failed)?;
                self.after_git_write(&root).await;
                RpcReply::value(&result)
            }
            methods::GIT_INIT => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = parse_params(params)?;
                let root = self.git_action_root(&p.path).await?;
                let git = self.git_actions(None).await;
                let status = git.init(&root).await.map_err(failed)?;
                self.diff_sync.sync_all();
                RpcReply::value(&status)
            }
            methods::GENERATE_COMMIT_MESSAGE => {
                let p: GenerateCommitParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(Some(&root)).await;
                let message = git
                    .generate_commit_message(&root, &p.files)
                    .await
                    .map_err(failed)?;
                RpcReply::value(&zeron_proto::GeneratedCommitMessage { message })
            }
            methods::GENERATE_PULL_REQUEST => {
                let p: PullRequestParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(Some(&root)).await;
                RpcReply::value(
                    &git.generate_pull_request(&root, p.base.as_deref())
                        .await
                        .map_err(failed)?,
                )
            }
            methods::GENERATE_BRANCH_NAME => {
                let p: BranchNameParams = parse_params(params)?;
                let root = match &p.cwd {
                    Some(cwd) => Some(self.git_action_root(cwd).await?),
                    None => None,
                };
                let git = self.git_actions(root.as_deref()).await;
                let name = git
                    .generate_branch_name(root.as_deref(), &p.description)
                    .await
                    .map_err(failed)?;
                RpcReply::value(&zeron_proto::GeneratedBranchName { name })
            }
            methods::CREATE_PULL_REQUEST => {
                let p: PullRequestParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(Some(&root)).await;
                let created = git
                    .create_pull_request(&root, &p.title, &p.body, p.base.as_deref(), p.draft)
                    .await
                    .map_err(failed)?;
                self.after_git_write(&root).await;
                self.watch_created_pull_request(&root, &created.url);
                RpcReply::value(&created)
            }
            methods::PUBLISH_REPOSITORY => {
                let p: PublishParams = parse_params(params)?;
                let root = self.git_action_root(&p.cwd).await?;
                let git = self.git_actions(None).await;
                let published = git
                    .publish(&root, p.name.as_deref(), p.visibility)
                    .await
                    .map_err(failed)?;
                self.after_git_write(&root).await;
                RpcReply::value(&published)
            }
            other => Err(RpcError::UnknownMethod(other.to_string())),
        }
    }
}
