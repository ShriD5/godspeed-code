//! The single long-poll loop: connect with the saved token, pull updates,
//! hand each to the bot, and back off on errors. A token change restarts
//! it; a rejected token parks it until the token changes.

use std::sync::Arc;
use std::time::Duration;

use zeron_proto::telegram::TelegramConnection;

use super::api::{ApiError, BotApi};
use super::token::BotToken;
use super::{Inner, lock};

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

impl Inner {
    pub(super) async fn supervise(self: Arc<Self>) {
        loop {
            let token = lock(&self.token).clone();
            // Registered before the select so a change racing it isn't lost.
            let changed = self.token_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match token {
                None => tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    _ = changed => {}
                },
                Some(token) => tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    _ = changed => {}
                    _ = self.clone().run_bot(token) => {}
                },
            }
            *lock(&self.api) = None;
        }
    }

    /// Runs until the token is rejected (then parks forever; the supervisor
    /// replaces it on the next token change).
    async fn run_bot(self: Arc<Self>, token: BotToken) {
        let base = lock(&self.api_base).clone();
        let api = BotApi::new(&base, token);
        let mut backoff = FIRST_BACKOFF;
        loop {
            self.set_connection(TelegramConnection::Connecting);
            let error = match api.get_me().await {
                Ok(me) => {
                    backoff = FIRST_BACKOFF;
                    let username = me.username.unwrap_or_else(|| me.id.to_string());
                    tracing::info!(bot = %username, "telegram: connected");
                    *lock(&self.api) = Some(api.clone());
                    self.set_connection(TelegramConnection::Connected {
                        bot_username: username,
                    });
                    self.wake_delivery.notify_one();
                    let error = self.poll(&api).await;
                    *lock(&self.api) = None;
                    error
                }
                Err(error) => error,
            };
            let message = api.token().redact(&error.to_string());
            tracing::warn!(error = %message, "telegram: poller stopped");
            self.set_connection(TelegramConnection::Error { message });
            let wait = match error {
                ApiError::Unauthorized => {
                    std::future::pending::<()>().await;
                    return;
                }
                ApiError::RateLimited(secs) => Duration::from_secs(secs),
                _ => {
                    let wait = backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    wait
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Pull and handle updates until a call fails. The offset is committed
    /// before an update is handled: a crash mid-handling drops that one
    /// message rather than running an agent prompt twice.
    async fn poll(self: &Arc<Self>, api: &BotApi) -> ApiError {
        loop {
            let offset = match self.store.update_offset() {
                Ok(offset) => offset,
                Err(err) => return ApiError::Transport(err.to_string()),
            };
            let updates = match api.get_updates(offset, self.options.poll_timeout).await {
                Ok(updates) => updates,
                Err(error) => return error,
            };
            for update in updates {
                if let Err(err) = self.store.set_update_offset(update.update_id + 1) {
                    return ApiError::Transport(err.to_string());
                }
                self.handle_update(api, update).await;
            }
        }
    }
}
