//! Telegram bot RPCs (`crate::telegram`): token, link codes, linked chats
//! and the defaults new chats start with. Device-scoped, so all of them
//! forward on `targetDeviceId` to the engine that runs the bot. None of
//! them ever returns the token.

use super::*;
use zeron_proto::telegram::TelegramDefaults;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::GET_TELEGRAM
            | methods::WATCH_TELEGRAM
            | methods::SET_TELEGRAM_TOKEN
            | methods::CLEAR_TELEGRAM_TOKEN
            | methods::CREATE_TELEGRAM_LINK_CODE
            | methods::UNLINK_TELEGRAM_CHAT
            | methods::SET_TELEGRAM_DEFAULTS
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetTokenParams {
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnlinkParams {
    chat_id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDefaultsParams {
    defaults: TelegramDefaults,
}

impl EngineRpc {
    pub(super) async fn telegram_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let telegram = self
            .telegram
            .as_ref()
            .ok_or_else(|| RpcError::Failed("the Telegram bot is unavailable".into()))?;
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match method {
            methods::GET_TELEGRAM => RpcReply::value(&telegram.status().map_err(failed)?),
            methods::WATCH_TELEGRAM => {
                let stream = futures::stream::unfold(
                    (telegram.watch(), telegram.clone(), false),
                    |(mut rx, telegram, emitted)| async move {
                        if emitted {
                            rx.changed().await.ok()?;
                        }
                        rx.borrow_and_update();
                        let status = match telegram.status() {
                            Ok(status) => serde_json::to_value(status).ok()?,
                            Err(err) => {
                                tracing::warn!(%err, "WatchTelegram: status failed");
                                return None;
                            }
                        };
                        Some((status, (rx, telegram, true)))
                    },
                );
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::SET_TELEGRAM_TOKEN => {
                let p: SetTokenParams = parse_params(params)?;
                RpcReply::value(&telegram.set_token(&p.token).map_err(failed)?)
            }
            methods::CLEAR_TELEGRAM_TOKEN => {
                RpcReply::value(&telegram.clear_token().map_err(failed)?)
            }
            methods::CREATE_TELEGRAM_LINK_CODE => RpcReply::value(&telegram.create_link_code()),
            methods::UNLINK_TELEGRAM_CHAT => {
                let p: UnlinkParams = parse_params(params)?;
                RpcReply::value(&telegram.unlink(p.chat_id).map_err(failed)?)
            }
            methods::SET_TELEGRAM_DEFAULTS => {
                let p: SetDefaultsParams = parse_params(params)?;
                RpcReply::value(&telegram.set_defaults(p.defaults).map_err(failed)?)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
