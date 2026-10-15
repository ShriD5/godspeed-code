//! Thread lifecycle RPCs (`crate::chat_lifecycle`): the device's auto-settle
//! setting. Forwarded on `targetDeviceId` to the engine whose timer acts on it.

use super::*;
use zeron_proto::ChatLifecycleSettings;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_CHAT_LIFECYCLE_SETTINGS | methods::SET_CHAT_LIFECYCLE_SETTINGS
    )
}

impl EngineRpc {
    pub(super) fn chat_lifecycle_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let lifecycle = self
            .chat_lifecycle
            .as_ref()
            .ok_or_else(|| RpcError::Failed("thread lifecycle is unavailable".into()))?;
        match method {
            methods::GET_CHAT_LIFECYCLE_SETTINGS => RpcReply::value(&lifecycle.settings()),
            methods::SET_CHAT_LIFECYCLE_SETTINGS => {
                let settings: ChatLifecycleSettings = parse_params(params)?;
                let saved = lifecycle
                    .set_settings(settings)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&saved)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
