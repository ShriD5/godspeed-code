//! Message search wire types (`SearchMessages`): full-text search over the
//! transcripts an engine holds, answered from its local message index.
//!
//! Every engine answers for the chats in its own store; a viewport asks its
//! local engine and, through `targetDeviceId`, each remote host for the
//! chats that host owns (`hostedOnly`), then merges by chat.

use serde::{Deserialize, Serialize};

/// Longest query an engine accepts, in characters.
pub const MESSAGE_SEARCH_MAX_QUERY_CHARS: usize = 256;
/// Most hits one request may ask for.
pub const MESSAGE_SEARCH_MAX_LIMIT: u16 = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMessagesRequest {
    /// Words to find, case-insensitively; every word must match, each as a
    /// word prefix. One character is enough.
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
    /// At most this many hits per chat (best first); unlimited when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_chat: Option<u16>,
    /// Only chats whose registry row names this engine's device as host.
    #[serde(default)]
    pub hosted_only: bool,
    /// Only these chats; every indexed chat when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageSearchRole {
    User,
    Assistant,
}

/// Which indexed field the snippet comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageSearchField {
    /// The message's prose.
    Text,
    /// Tool titles and commands (lower weight).
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSearchHit {
    pub chat_id: String,
    /// The transcript entry (continuations joined to their root), the same
    /// id the transcript's rows carry as `entry_id`.
    pub message_id: String,
    pub role: MessageSearchRole,
    /// Epoch millis.
    pub created_at: i64,
    pub field: MessageSearchField,
    /// One line of context around the first match, whitespace collapsed.
    pub snippet: String,
    /// `[start, end)` byte ranges of the matches within `snippet`.
    pub ranges: Vec<(u32, u32)>,
    /// Relevance with recency folded in; higher is better.
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMessagesResult {
    /// Best first.
    pub hits: Vec<MessageSearchHit>,
    /// The index was still loading or catching up; results may be partial.
    pub indexing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_defaults_are_optional_on_the_wire() {
        let request: SearchMessagesRequest =
            serde_json::from_value(serde_json::json!({ "query": "a" })).unwrap();
        assert_eq!(request.query, "a");
        assert_eq!(request.limit, None);
        assert!(!request.hosted_only);
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({ "query": "a", "hostedOnly": false })
        );
    }

    #[test]
    fn hit_round_trips_camel_case() {
        let hit = MessageSearchHit {
            chat_id: "c".into(),
            message_id: "m".into(),
            role: MessageSearchRole::Assistant,
            created_at: 7,
            field: MessageSearchField::Tool,
            snippet: "cargo test".into(),
            ranges: vec![(0, 5)],
            score: 1.5,
        };
        let value = serde_json::to_value(&hit).unwrap();
        assert_eq!(value["chatId"], "c");
        assert_eq!(value["field"], "tool");
        assert_eq!(serde_json::from_value::<MessageSearchHit>(value).unwrap(), hit);
    }
}
