//! Shared build slots RPCs (`crate::build_slots`): Settings → General →
//! Builds. IPC only: it's this device's setting.

use super::*;
use zeron_proto::BuildSlotsSettingsUpdate;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_BUILD_SLOTS_SETTINGS | methods::SET_BUILD_SLOTS_SETTINGS
    )
}

impl EngineRpc {
    pub(super) fn build_slots_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let build_slots = self
            .build_slots
            .as_ref()
            .ok_or_else(|| RpcError::Failed("build slots are unavailable".into()))?;
        match method {
            methods::GET_BUILD_SLOTS_SETTINGS => RpcReply::value(&build_slots.settings()),
            methods::SET_BUILD_SLOTS_SETTINGS => {
                let update: BuildSlotsSettingsUpdate = parse_params(params)?;
                let saved = build_slots
                    .update(update)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&saved)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
