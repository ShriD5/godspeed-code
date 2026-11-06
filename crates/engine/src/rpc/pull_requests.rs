//! Pull request actions and watches (`PerformChangeRequestAction`,
//! `GetChangeRequestMergeOptions`, `ListPullRequestWatches`,
//! `SetPullRequestWatch`). Device-scoped like the other PR reads: they run
//! on the engine whose `gh` is signed in, forwarded on `targetDeviceId`.

use super::*;
use zeron_proto::pull_request_actions::{
    ChangeRequestAction, ChangeRequestActionResult, SetPullRequestWatch,
};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::PERFORM_CHANGE_REQUEST_ACTION
            | methods::GET_CHANGE_REQUEST_MERGE_OPTIONS
            | methods::LIST_PULL_REQUEST_WATCHES
            | methods::SET_PULL_REQUEST_WATCH
    )
}

#[derive(Debug, Deserialize)]
struct ActionParams {
    url: String,
    action: ChangeRequestAction,
}

#[derive(Debug, Deserialize)]
struct UrlParams {
    url: String,
}

impl EngineRpc {
    pub(super) async fn pull_request_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        match method {
            methods::PERFORM_CHANGE_REQUEST_ACTION => {
                let p: ActionParams = parse_params(params)?;
                p.action.validate().map_err(RpcError::BadParams)?;
                // The words GitHub refused with are the point: the user
                // asked for this and needs to know why it didn't happen.
                let message = self
                    .github
                    .perform_action(&p.url, &p.action)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&ChangeRequestActionResult { message })
            }
            methods::GET_CHANGE_REQUEST_MERGE_OPTIONS => {
                let p: UrlParams = parse_params(params)?;
                let options = self
                    .github
                    .merge_options(&p.url)
                    .await
                    .map_err(change_request_rpc_error)?;
                RpcReply::value(&options)
            }
            methods::LIST_PULL_REQUEST_WATCHES => {
                let watch = self.pr_watch()?;
                RpcReply::value(&watch.list())
            }
            methods::SET_PULL_REQUEST_WATCH => {
                let watch = self.pr_watch()?;
                let p: SetPullRequestWatch = parse_params(params)?;
                let watches = watch
                    .set(&p)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                RpcReply::value(&watches)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }

    fn pr_watch(&self) -> Result<&crate::pr_watch::PrWatchService, RpcError> {
        self.pr_watch
            .as_ref()
            .ok_or_else(|| RpcError::Failed("pull request watching is unavailable".into()))
    }
}
