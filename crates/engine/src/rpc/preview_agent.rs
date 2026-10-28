//! Agent preview RPCs (`crate::preview_driver`): the MCP server's
//! `PreviewAct`, and the desktop window's driver stream and answers. IPC
//! only: the browser is in the window attached to this engine.

use super::*;
use zeron_proto::preview_agent::{PreviewActParams, PreviewDriverAnswer};
use zeron_proto::{AccessMode, ApprovalDecision, ApprovalRequest};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::PREVIEW_ACT | methods::WATCH_PREVIEW_DRIVER | methods::ANSWER_PREVIEW_REQUEST
    )
}

/// The network gate's view of the session engine.
struct Chats<'a>(&'a SessionsEngine);

#[async_trait]
impl crate::preview_driver::PreviewChats for Chats<'_> {
    fn access_mode(&self, chat_id: &str) -> Option<AccessMode> {
        self.0.last_request(chat_id).map(|request| request.access_mode())
    }

    async fn ask(&self, chat_id: &str, request: ApprovalRequest) -> Option<ApprovalDecision> {
        let question = request.clone().into_question(crate::new_id());
        let answers = self.0.ask_input(chat_id, vec![question.clone()])?.await.ok()?;
        // Empty answers: the turn ended (or was parked) before anyone
        // answered, so nobody declined — there was no one to ask.
        if answers.is_empty() {
            return None;
        }
        Some(request.decide(&question.id, &answers))
    }
}

impl EngineRpc {
    pub(super) async fn preview_agent_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        match method {
            methods::WATCH_PREVIEW_DRIVER => Ok(RpcReply::Stream(self.preview_driver.attach())),
            methods::ANSWER_PREVIEW_REQUEST => {
                let answer: PreviewDriverAnswer = parse_params(params)?;
                self.preview_driver.answer(answer);
                RpcReply::value(&serde_json::json!({}))
            }
            methods::PREVIEW_ACT => {
                let p: PreviewActParams = parse_params(params)?;
                let value = self
                    .preview_driver
                    .act(&Chats(&self.sessions), &p.chat_id, p.action)
                    .await
                    .map_err(RpcError::Failed)?;
                Ok(RpcReply::Value(value))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
