//! Keep-awake RPCs (`crate::keep_awake`): this engine's setting for holding
//! off system sleep while its runs work.

use super::*;
use zeron_proto::KeepAwakeSettings;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_KEEP_AWAKE_SETTINGS | methods::SET_KEEP_AWAKE_SETTINGS
    )
}

impl EngineRpc {
    pub(super) fn keep_awake_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let keep_awake = self
            .keep_awake
            .as_ref()
            .ok_or_else(|| RpcError::Failed("keep awake is unavailable".into()))?;
        match method {
            methods::GET_KEEP_AWAKE_SETTINGS => RpcReply::value(&keep_awake.settings()),
            methods::SET_KEEP_AWAKE_SETTINGS => {
                let settings: KeepAwakeSettings = parse_params(params)?;
                let saved = keep_awake
                    .set_settings(settings)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&saved)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
