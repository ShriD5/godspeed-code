//! Anonymous telemetry RPCs (`crate::telemetry`): Settings → General's
//! "Share anonymous usage stats" switch and the first-launch notice. IPC
//! only: it's this install's setting.

use super::*;
use zeron_proto::TelemetrySettingsUpdate;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_TELEMETRY_SETTINGS | methods::SET_TELEMETRY_SETTINGS
    )
}

impl EngineRpc {
    pub(super) fn telemetry_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let telemetry = self
            .telemetry
            .as_ref()
            .ok_or_else(|| RpcError::Failed("telemetry is unavailable".into()))?;
        match method {
            methods::GET_TELEMETRY_SETTINGS => RpcReply::value(&telemetry.settings()),
            methods::SET_TELEMETRY_SETTINGS => {
                let update: TelemetrySettingsUpdate = parse_params(params)?;
                let saved = telemetry
                    .update(update)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&saved)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
