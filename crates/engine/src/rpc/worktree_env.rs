//! Worktree environment RPCs (`crate::worktree_env`): a project's
//! environment config, and each worktree's ports and sleep state. All of
//! them forward on `targetDeviceId` to the device that owns the project.

use super::*;
use crate::worktree_env::{ProjectScope, WorktreeEnvironments};
use zeron_proto::WorktreeEnvironment;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_WORKTREE_ENVIRONMENT
            | methods::SET_WORKTREE_ENVIRONMENT
            | methods::GET_WORKTREE_STATUS
            | methods::SLEEP_WORKTREE
            | methods::WAKE_WORKTREE
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpaceParams {
    space_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetEnvironmentParams {
    space_id: String,
    environment: WorktreeEnvironment,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreePathParams {
    path: String,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_cols() -> u16 {
    120
}

fn default_rows() -> u16 {
    32
}

impl EngineRpc {
    pub(super) fn worktree_environments(&self) -> WorktreeEnvironments {
        WorktreeEnvironments::new(
            self.repos.clone(),
            self.terminals.clone(),
            self.project_actions.clone(),
        )
    }

    /// The local project `space_id` names, as a scope for its worktrees.
    pub(super) fn project_scope(&self, space_id: &str) -> Result<ProjectScope, RpcError> {
        let space = self.local_project_action_space(space_id)?;
        let root = std::fs::canonicalize(&space.path)
            .map_err(|_| RpcError::Failed("Project root is unavailable".into()))?;
        Ok(ProjectScope {
            space_id: space.id,
            space_path: space.path.into(),
            root,
        })
    }

    pub(super) async fn worktree_env_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let failed = |error: crate::EngineError| RpcError::Failed(error.to_string());
        let environments = self.worktree_environments();
        match method {
            methods::GET_WORKTREE_ENVIRONMENT => {
                let p: SpaceParams = parse_params(params)?;
                let space = self.local_project_action_space(&p.space_id)?;
                let store = self.project_actions.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    store.environment_snapshot(&space.id, std::path::Path::new(&space.path))
                })
                .await
                .map_err(|error| RpcError::Failed(error.to_string()))?
                .map_err(failed)?;
                RpcReply::value(&snapshot)
            }
            methods::SET_WORKTREE_ENVIRONMENT => {
                let p: SetEnvironmentParams = parse_params(params)?;
                let space = self.local_project_action_space(&p.space_id)?;
                let store = self.project_actions.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    store.set_environment(
                        &space.id,
                        std::path::Path::new(&space.path),
                        p.environment,
                    )
                })
                .await
                .map_err(|error| RpcError::Failed(error.to_string()))?
                .map_err(failed)?;
                RpcReply::value(&snapshot)
            }
            methods::GET_WORKTREE_STATUS => {
                let p: WorktreePathParams = parse_params(params)?;
                RpcReply::value(&environments.status(std::path::Path::new(&p.path)))
            }
            methods::SLEEP_WORKTREE => {
                let p: WorktreePathParams = parse_params(params)?;
                let status = environments
                    .sleep(std::path::Path::new(&p.path))
                    .await
                    .map_err(failed)?;
                RpcReply::value(&status)
            }
            methods::WAKE_WORKTREE => {
                let p: WorktreePathParams = parse_params(params)?;
                let outcome = environments
                    .wake(std::path::Path::new(&p.path), p.cols, p.rows)
                    .map_err(failed)?;
                RpcReply::value(&outcome)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
