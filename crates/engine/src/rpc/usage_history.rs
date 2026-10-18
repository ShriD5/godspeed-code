//! Usage history RPC (`GetUsageHistory`, `crate::usage_history`). IPC-only:
//! the answer is about this device's turns and agent logs.

use super::*;
use zeron_proto::UsageHistoryRequest;

pub(super) fn handles(method: &str) -> bool {
    method == methods::GET_USAGE_HISTORY
}

impl EngineRpc {
    pub(super) async fn usage_history_rpc(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let request: UsageHistoryRequest = parse_params(params)?;
        let index = self
            .usage_history
            .clone()
            .ok_or_else(|| RpcError::Failed("usage history is unavailable".into()))?;
        let history = index
            .history(request)
            .await
            .map_err(|e| RpcError::Failed(e.to_string()))?;
        RpcReply::value(&history)
    }
}
