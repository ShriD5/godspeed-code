//! Usage-limit recovery RPCs (`crate::limit_recovery`): the banner's
//! choices and the device default, plus continuing a chat on another
//! account (`crate::account_handoff`). Forwarded on `targetDeviceId` to the
//! engine that hosts the chat, whose timer and agent logins act on them.

use super::*;
use zeron_proto::{LimitRecoveryAction, LimitRecoverySettings};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_LIMIT_RECOVERY_SETTINGS
            | methods::SET_LIMIT_RECOVERY_SETTINGS
            | methods::SET_CHAT_LIMIT_RECOVERY
            | methods::CONTINUE_CHAT_ON_ACCOUNT
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetChatLimitRecoveryParams {
    chat_id: String,
    action: LimitRecoveryAction,
    #[serde(default)]
    reset_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl EngineRpc {
    pub(super) async fn limit_recovery_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        if method == methods::CONTINUE_CHAT_ON_ACCOUNT {
            let request: zeron_proto::ContinueOnAccountRequest = parse_params(params)?;
            let handoff = crate::account_handoff::continue_on_account(
                &self.workspace,
                &self.doc_host,
                &self.agent_accounts,
                request,
            )
            .await
            .map_err(|e| RpcError::Failed(e.to_string()))?;
            // Same as ActivateAgentAccount: live voice calls hold the old login.
            if handoff.switched {
                self.voice.retire();
            }
            return RpcReply::value(&handoff.reply);
        }
        let recovery = self
            .limit_recovery
            .as_ref()
            .ok_or_else(|| RpcError::Failed("usage-limit recovery is unavailable".into()))?;
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match method {
            methods::GET_LIMIT_RECOVERY_SETTINGS => RpcReply::value(&recovery.settings()),
            methods::SET_LIMIT_RECOVERY_SETTINGS => {
                let settings: LimitRecoverySettings = parse_params(params)?;
                RpcReply::value(&recovery.set_settings(settings).map_err(failed)?)
            }
            methods::SET_CHAT_LIMIT_RECOVERY => {
                let p: SetChatLimitRecoveryParams = parse_params(params)?;
                let limit = recovery
                    .set_recovery(&p.chat_id, p.action, p.reset_at)
                    .await
                    .map_err(failed)?;
                RpcReply::value(&serde_json::json!({ "limit": limit }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
