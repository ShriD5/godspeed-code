//! The handful of Telegram Bot API methods the bot uses, over reqwest.
//! Every call is a JSON `POST {base}/bot{token}/{method}`; the base URL is
//! configurable so tests (and self-hosted Bot API servers) can stand in.

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::token::BotToken;

pub(crate) const DEFAULT_API_BASE: &str = "https://api.telegram.org";
/// Headroom over a long poll's own timeout before the HTTP call gives up.
const POLL_SLACK: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct User {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
}

impl User {
    pub fn label(&self) -> String {
        match (&self.username, &self.first_name) {
            (Some(username), _) => format!("@{username}"),
            (None, Some(name)) => name.clone(),
            (None, None) => self.id.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

impl Chat {
    pub fn is_private(&self) -> bool {
        self.kind == "private"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Button {
    pub text: String,
    pub callback_data: String,
}

impl Button {
    pub fn new(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: data.into(),
        }
    }
}

/// Rows of inline buttons.
pub(crate) type Keyboard = Vec<Vec<Button>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Plain,
    Html,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SendOptions {
    pub reply_to: Option<i64>,
    pub keyboard: Option<Keyboard>,
    pub html: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ApiError {
    #[error("Telegram rejected the bot token")]
    Unauthorized,
    #[error("another program is polling this bot (only one may at a time)")]
    Conflict,
    #[error("Telegram asked to slow down for {0}s")]
    RateLimited(u64),
    #[error("Telegram refused {method}: {description}")]
    Rejected { method: String, description: String },
    #[error("couldn't reach Telegram: {0}")]
    Transport(String),
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<u16>,
    #[serde(default)]
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct BotApi {
    http: reqwest::Client,
    base: Arc<str>,
    token: Arc<BotToken>,
}

impl BotApi {
    pub fn new(base: &str, token: BotToken) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').into(),
            token: Arc::new(token),
        }
    }

    pub fn token(&self) -> &BotToken {
        &self.token
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token.expose());
        // reqwest errors name the URL, which carries the token: drop it.
        let transport = |err: reqwest::Error| {
            ApiError::Transport(self.token.redact(&err.without_url().to_string()))
        };
        let response = self
            .http
            .post(url)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        let envelope: Envelope = response.json().await.map_err(transport)?;
        if envelope.ok {
            return serde_json::from_value(envelope.result)
                .map_err(|err| ApiError::Transport(format!("unexpected {method} reply: {err}")));
        }
        let description = self
            .token
            .redact(envelope.description.as_deref().unwrap_or("no description"));
        Err(match envelope.error_code.unwrap_or(status.as_u16()) {
            401 | 404 => ApiError::Unauthorized,
            409 => ApiError::Conflict,
            429 => {
                ApiError::RateLimited(envelope.parameters.and_then(|p| p.retry_after).unwrap_or(5))
            }
            _ => ApiError::Rejected {
                method: method.to_string(),
                description,
            },
        })
    }

    pub async fn get_me(&self) -> Result<User, ApiError> {
        self.call("getMe", json!({}), CALL_TIMEOUT).await
    }

    pub async fn get_updates(&self, offset: i64, poll: Duration) -> Result<Vec<Update>, ApiError> {
        self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": poll.as_secs(),
                "allowed_updates": ["message", "callback_query"],
            }),
            poll + POLL_SLACK,
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        options: &SendOptions,
    ) -> Result<Message, ApiError> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(reply_to) = options.reply_to {
            body["reply_parameters"] =
                json!({ "message_id": reply_to, "allow_sending_without_reply": true });
        }
        if let Some(keyboard) = &options.keyboard {
            body["reply_markup"] = json!({ "inline_keyboard": keyboard });
        }
        if options.html {
            body["parse_mode"] = json!("HTML");
        }
        self.call("sendMessage", body, CALL_TIMEOUT).await
    }

    /// Replace a message's text and buttons (`None` removes the buttons).
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        keyboard: Option<&Keyboard>,
        format: Format,
    ) -> Result<(), ApiError> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
            "reply_markup": { "inline_keyboard": keyboard.cloned().unwrap_or_default() },
        });
        if format == Format::Html {
            body["parse_mode"] = json!("HTML");
        }
        match self
            .call::<Value>("editMessageText", body, CALL_TIMEOUT)
            .await
        {
            Ok(_) => Ok(()),
            Err(ApiError::Rejected { description, .. })
                if description.contains("message is not modified") =>
            {
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub async fn answer_callback_query(
        &self,
        id: &str,
        text: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut body = json!({ "callback_query_id": id });
        if let Some(text) = text {
            body["text"] = json!(text);
        }
        self.call::<Value>("answerCallbackQuery", body, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn send_typing(&self, chat_id: i64) -> Result<(), ApiError> {
        self.call::<Value>(
            "sendChatAction",
            json!({ "chat_id": chat_id, "action": "typing" }),
            CALL_TIMEOUT,
        )
        .await
        .map(|_| ())
    }
}
