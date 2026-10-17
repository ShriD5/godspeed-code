//! "Needs you" inbox RPCs (`crate::attention`): the queue, answers to its
//! approvals, dismissals and the question-timeout settings. Device-scoped,
//! so all of them forward on `targetDeviceId` to the engine hosting the
//! chats.

use super::*;
use zeron_proto::attention::{AttentionResponse, ChatQuestionTimeout, QuestionTimeoutSettings};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_ATTENTION
            | methods::WATCH_ATTENTION
            | methods::RESPOND_ATTENTION
            | methods::APPROVE_LOW_RISK
            | methods::DISMISS_ATTENTION
            | methods::GET_QUESTION_TIMEOUT_SETTINGS
            | methods::SET_QUESTION_TIMEOUT_SETTINGS
            | methods::SET_CHAT_QUESTION_TIMEOUT
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApproveLowRiskParams {
    item_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemParams {
    item_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatTimeoutParams {
    chat_id: String,
    timeout: Option<ChatQuestionTimeout>,
}

impl EngineRpc {
    pub(super) fn attention_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let attention = self
            .attention
            .as_ref()
            .ok_or_else(|| RpcError::Failed("the inbox is unavailable".into()))?;
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match method {
            methods::GET_ATTENTION => RpcReply::value(&attention.snapshot()),
            methods::WATCH_ATTENTION => Ok(RpcReply::Stream(watch_stream(attention.watch()))),
            methods::RESPOND_ATTENTION => {
                let p: AttentionResponse = parse_params(params)?;
                attention.respond(&p.item_id, p.decision).map_err(failed)?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::APPROVE_LOW_RISK => {
                let p: ApproveLowRiskParams = parse_params(params)?;
                RpcReply::value(&attention.approve_low_risk(&p.item_ids))
            }
            methods::DISMISS_ATTENTION => {
                let p: ItemParams = parse_params(params)?;
                let dismissed = attention.dismiss(&p.item_id).map_err(failed)?;
                RpcReply::value(&serde_json::json!({ "dismissed": dismissed }))
            }
            methods::GET_QUESTION_TIMEOUT_SETTINGS => {
                RpcReply::value(&attention.timeout_settings())
            }
            methods::SET_QUESTION_TIMEOUT_SETTINGS => {
                let settings: QuestionTimeoutSettings = parse_params(params)?;
                RpcReply::value(&attention.set_timeout_settings(settings).map_err(failed)?)
            }
            methods::SET_CHAT_QUESTION_TIMEOUT => {
                let p: ChatTimeoutParams = parse_params(params)?;
                let saved = attention
                    .set_chat_timeout(&p.chat_id, p.timeout)
                    .map_err(failed)?;
                RpcReply::value(&saved)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
