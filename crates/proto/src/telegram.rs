//! Telegram bot for always-on agents: wire shape for the `GetTelegram` /
//! `WatchTelegram` / … RPCs. Device-scoped like the agents it fronts: the
//! engine that holds the bot token is the one that polls Telegram.
//!
//! The bot token itself never appears here; `token_set` is all a viewport
//! learns about it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{AccessMode, HarnessId};

/// Provider, model, project and access a Telegram chat starts new threads
/// and schedules with. Every field is optional: `None` means "the engine's
/// pick" (first installed provider, its default model, no project, the
/// provider's default access mode).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A space on the engine's own device; `None` runs in a scratch folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_mode: Option<AccessMode>,
}

/// The poller's state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum TelegramConnection {
    /// No token saved.
    #[default]
    Off,
    Connecting,
    #[serde(rename_all = "camelCase")]
    Connected {
        bot_username: String,
    },
    /// The last attempt failed; the poller retries with backoff unless the
    /// token itself was rejected.
    #[serde(rename_all = "camelCase")]
    Error {
        message: String,
    },
}

/// One-time code a Telegram chat sends as `/link CODE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramLinkCode {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

/// A private chat allowed to drive this engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramLinkedChat {
    pub chat_id: i64,
    /// The user's name or @handle as Telegram reported it at link time.
    pub label: String,
    pub linked_at: DateTime<Utc>,
    /// IANA zone `/schedule` times are read in.
    pub timezone: String,
    pub defaults: TelegramDefaults,
}

/// `GetTelegram` / `WatchTelegram` item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramStatus {
    pub token_set: bool,
    pub connection: TelegramConnection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_code: Option<TelegramLinkCode>,
    #[serde(default)]
    pub linked_chats: Vec<TelegramLinkedChat>,
    /// What a newly linked chat starts with.
    pub defaults: TelegramDefaults,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_wire_shape_is_tagged() {
        assert_eq!(
            serde_json::to_value(TelegramConnection::Connected {
                bot_username: "godspeed_bot".into()
            })
            .unwrap(),
            serde_json::json!({ "state": "connected", "botUsername": "godspeed_bot" })
        );
        assert_eq!(
            serde_json::to_value(TelegramConnection::Off).unwrap(),
            serde_json::json!({ "state": "off" })
        );
    }

    #[test]
    fn defaults_omit_unset_fields() {
        let defaults = TelegramDefaults {
            harness: Some(HarnessId::Codex),
            access_mode: Some(AccessMode::Supervised),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&defaults).unwrap(),
            serde_json::json!({ "harness": "codex", "accessMode": "supervised" })
        );
        let empty: TelegramDefaults = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, TelegramDefaults::default());
    }
}
