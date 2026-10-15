//! Message search RPC (`SearchMessages`, `crate::message_index`). Forwards on
//! `targetDeviceId`: each engine answers from its own index, so a viewport
//! asks remote hosts for the chats they own.

use super::*;
use zeron_proto::{
    MESSAGE_SEARCH_MAX_LIMIT, MESSAGE_SEARCH_MAX_QUERY_CHARS, SearchMessagesRequest,
    SearchMessagesResult,
};

/// Hits returned when the request names no limit.
const DEFAULT_LIMIT: u16 = 50;

pub(super) fn handles(method: &str) -> bool {
    method == methods::SEARCH_MESSAGES
}

impl EngineRpc {
    pub(super) async fn message_search_rpc(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let request: SearchMessagesRequest = parse_params(params)?;
        if request.query.chars().count() > MESSAGE_SEARCH_MAX_QUERY_CHARS {
            return Err(RpcError::BadParams(format!(
                "SearchMessages query must not exceed {MESSAGE_SEARCH_MAX_QUERY_CHARS} characters"
            )));
        }
        let index = self
            .message_index
            .clone()
            .ok_or_else(|| RpcError::Failed("message search is unavailable".into()))?;
        // Hosted-only answers are about this device's registry rows.
        let hosted: Option<HashSet<String>> = if request.hosted_only {
            let device = self.doc_host.device_id().to_owned();
            let chats = self
                .workspace
                .read_chats()
                .map_err(|e| RpcError::Failed(e.to_string()))?;
            Some(
                chats
                    .into_iter()
                    .filter(|chat| chat.device_id == device)
                    .map(|chat| chat.id)
                    .collect(),
            )
        } else {
            None
        };
        let result = tokio::task::spawn_blocking(move || {
            let only: Option<HashSet<String>> =
                request.chat_ids.map(|ids| ids.into_iter().collect());
            let accept = |chat: &str| {
                hosted.as_ref().is_none_or(|ids| ids.contains(chat))
                    && only.as_ref().is_none_or(|ids| ids.contains(chat))
            };
            let filtered = hosted.is_some() || only.is_some();
            let limit = request
                .limit
                .unwrap_or(DEFAULT_LIMIT)
                .min(MESSAGE_SEARCH_MAX_LIMIT) as usize;
            SearchMessagesResult {
                hits: index.search(
                    &request.query,
                    limit,
                    request.per_chat.map(usize::from),
                    filtered.then_some(&accept as &dyn Fn(&str) -> bool),
                ),
                indexing: !index.is_ready(),
            }
        })
        .await
        .map_err(|e| RpcError::Failed(e.to_string()))?;
        RpcReply::value(&result)
    }
}
