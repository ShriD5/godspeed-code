//! Project transfer RPCs (`crate::project_transfer`): listing and reading
//! ignored files on the device that owns a project, and cloning plus writing
//! relayed files on the device receiving it. All forward on `targetDeviceId`.

use super::*;
use zeron_proto::project_transfer::{
    CancelProjectCloneRequest, CloneProgress, CloneProjectRequest, ListIgnoredFilesRequest,
    ReadProjectFileRequest, WriteProjectFileRequest,
};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_IGNORED_FILES
            | methods::READ_PROJECT_FILE
            | methods::CLONE_PROJECT
            | methods::CANCEL_PROJECT_CLONE
            | methods::WRITE_PROJECT_FILE
    )
}

pub(super) fn is_stream(method: &str) -> bool {
    method == methods::CLONE_PROJECT
}

/// Clone progress as a stream: the current phase, every change, and nothing
/// after the terminal item.
fn clone_stream(rx: watch::Receiver<CloneProgress>) -> BoxStream<'static, serde_json::Value> {
    futures::stream::unfold(Some((rx, false)), |state| async move {
        let (mut rx, emitted) = state?;
        if emitted && rx.changed().await.is_err() {
            // The clone task ended unseen. Nothing terminal went out yet
            // (the stream stops after one), so its last value is due.
            let last = rx.borrow().clone();
            if !last.is_terminal() {
                return None;
            }
            return Some((serde_json::to_value(&last).ok()?, None));
        }
        let value = rx.borrow_and_update().clone();
        let terminal = value.is_terminal();
        let json = serde_json::to_value(&value).ok()?;
        Some((json, (!terminal).then_some((rx, true))))
    })
    .boxed()
}

impl EngineRpc {
    pub(super) async fn project_transfer_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let failed = |error: crate::EngineError| RpcError::Failed(error.to_string());
        match method {
            methods::LIST_IGNORED_FILES => {
                let request: ListIgnoredFilesRequest = parse_params(params)?;
                let space = self.local_project_action_space(&request.space_id)?;
                let listing =
                    crate::project_transfer::list_ignored_files(std::path::Path::new(&space.path))
                        .await
                        .map_err(failed)?;
                RpcReply::value(&listing)
            }
            methods::READ_PROJECT_FILE => {
                let request: ReadProjectFileRequest = parse_params(params)?;
                let space = self.local_project_action_space(&request.space_id)?;
                let chunk = crate::project_transfer::read_file_chunk(
                    std::path::Path::new(&space.path),
                    &request.path,
                    request.offset,
                )
                .await
                .map_err(failed)?;
                RpcReply::value(&chunk)
            }
            methods::WRITE_PROJECT_FILE => {
                let request: WriteProjectFileRequest = parse_params(params)?;
                let space = self.local_project_action_space(&request.space_id)?;
                let outcome = crate::project_transfer::write_file_chunk(
                    std::path::Path::new(&space.path),
                    request,
                )
                .await
                .map_err(failed)?;
                RpcReply::value(&outcome)
            }
            methods::CLONE_PROJECT => {
                let request: CloneProjectRequest = parse_params(params)?;
                let rx = crate::project_transfer::start_clone(
                    self.repos.clone(),
                    self.workspace.clone(),
                    self.doc_host.device_id().to_string(),
                    request,
                )
                .map_err(failed)?;
                Ok(RpcReply::Stream(clone_stream(rx)))
            }
            methods::CANCEL_PROJECT_CLONE => {
                let request: CancelProjectCloneRequest = parse_params(params)?;
                let cancelled = self.repos.project_clones().cancel(&request.clone_id);
                RpcReply::value(&serde_json::json!({ "cancelled": cancelled }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
