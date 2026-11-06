//! Worktree cleanup RPCs (`crate::worktree_cleanup`): the device's cleanup
//! policy, the storage listing, "Delete all eligible", and resolving a
//! checkout by folder or branch (MCP `create_chat` `worktree`). Forwarded on
//! `targetDeviceId` to the engine holding the worktrees.

use super::*;
use zeron_proto::worktree_cleanup::CleanupMode;
use zeron_proto::{
    CleanWorktreesParams, ListWorktreeStorageParams, ResolveWorktreeParams, WorktreeCleanupSettings,
};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_WORKTREE_CLEANUP_SETTINGS
            | methods::SET_WORKTREE_CLEANUP_SETTINGS
            | methods::LIST_WORKTREE_STORAGE
            | methods::CLEAN_WORKTREES
            | methods::RESOLVE_WORKTREE
    )
}

/// Params that may be `null` (no fields) or an object carrying only routing.
fn optional_params<T: serde::de::DeserializeOwned + Default>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    if params.is_null() {
        Ok(T::default())
    } else {
        parse_params(params)
    }
}

impl EngineRpc {
    pub(super) async fn worktree_cleanup_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        if method == methods::RESOLVE_WORKTREE {
            let p: ResolveWorktreeParams = parse_params(params)?;
            let resolved = self
                .repos
                .resolve_worktree(std::path::Path::new(&p.repo_path), &p.target)
                .await
                .map_err(failed)?;
            return RpcReply::value(&resolved);
        }
        let cleanup = self
            .worktree_cleanup
            .as_ref()
            .ok_or_else(|| RpcError::Failed("worktree cleanup is unavailable".into()))?;
        match method {
            methods::GET_WORKTREE_CLEANUP_SETTINGS => RpcReply::value(&cleanup.settings()),
            methods::SET_WORKTREE_CLEANUP_SETTINGS => {
                let settings: WorktreeCleanupSettings = parse_params(params)?;
                let saved = cleanup.set_settings(settings).map_err(failed)?;
                RpcReply::value(&saved)
            }
            methods::LIST_WORKTREE_STORAGE => {
                let p: ListWorktreeStorageParams = optional_params(params)?;
                let entries = cleanup.list(p.repo_path, p.sizes).await.map_err(failed)?;
                RpcReply::value(&entries)
            }
            methods::CLEAN_WORKTREES => {
                let p: CleanWorktreesParams = optional_params(params)?;
                let report = cleanup
                    .clean(&p.paths, CleanupMode::Manual)
                    .await
                    .map_err(failed)?;
                RpcReply::value(&report)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
