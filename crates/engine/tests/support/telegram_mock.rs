//! A local stand-in for the Telegram Bot API: the methods the bot uses,
//! over plain HTTP/1.1 on 127.0.0.1. Tests queue user updates and inspect
//! every call the bot made. Nothing here talks to the real Telegram.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub body: Value,
}

#[derive(Default)]
struct State {
    updates: Vec<Value>,
    next_update_id: i64,
    next_bot_message: i64,
    next_user_message: i64,
    calls: Vec<Call>,
}

pub struct MockBotApi {
    pub base: String,
    state: Arc<Mutex<State>>,
    updated: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockBotApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub const BOT_USERNAME: &str = "godspeed_test_bot";
/// Longest a mocked long poll holds the connection.
const MAX_POLL: Duration = Duration::from_secs(1);

impl MockBotApi {
    pub async fn start(token: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            next_update_id: 1,
            next_bot_message: 5000,
            next_user_message: 100,
            ..Default::default()
        }));
        let updated = Arc::new(Notify::new());
        let (serve_state, serve_updated, serve_token) =
            (state.clone(), updated.clone(), token.to_string());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (state, updated, token) = (
                    serve_state.clone(),
                    serve_updated.clone(),
                    serve_token.clone(),
                );
                tokio::spawn(async move {
                    let _ = serve(stream, &token, &state, &updated).await;
                });
            }
        });
        Self {
            base,
            state,
            updated,
            task,
        }
    }

    fn push_update(&self, mut update: Value) {
        let mut state = self.state.lock().unwrap();
        update["update_id"] = json!(state.next_update_id);
        state.next_update_id += 1;
        state.updates.push(update);
        drop(state);
        self.updated.notify_waiters();
    }

    fn message(&self, chat: i64, kind: &str, text: &str, reply_to: Option<i64>) -> i64 {
        let id = {
            let mut state = self.state.lock().unwrap();
            state.next_user_message += 1;
            state.next_user_message
        };
        let mut message = json!({
            "message_id": id,
            "date": 0,
            "chat": { "id": chat, "type": kind },
            "from": { "id": chat, "is_bot": false, "first_name": "Shri", "username": "shri" },
            "text": text,
        });
        if let Some(reply_to) = reply_to {
            message["reply_to_message"] = json!({
                "message_id": reply_to,
                "date": 0,
                "chat": { "id": chat, "type": kind },
                "text": "",
            });
        }
        self.push_update(json!({ "message": message }));
        id
    }

    /// A private-chat text message from user `chat`; returns its id.
    pub fn send(&self, chat: i64, text: &str) -> i64 {
        self.message(chat, "private", text, None)
    }

    pub fn reply(&self, chat: i64, text: &str, reply_to: i64) -> i64 {
        self.message(chat, "private", text, Some(reply_to))
    }

    pub fn send_in_group(&self, chat: i64, text: &str) -> i64 {
        self.message(chat, "group", text, None)
    }

    /// The user taps an inline button on bot message `message_id`.
    pub fn tap(&self, chat: i64, message_id: i64, data: &str) {
        self.push_update(json!({
            "callback_query": {
                "id": format!("cb-{message_id}-{data}"),
                "from": { "id": chat, "is_bot": false, "first_name": "Shri" },
                "chat_instance": "x",
                "data": data,
                "message": {
                    "message_id": message_id,
                    "date": 0,
                    "chat": { "id": chat, "type": "private" },
                    "text": "",
                },
            }
        }));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn calls_to(&self, method: &str) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == method)
            .map(|c| c.body)
            .collect()
    }

    /// Wait for the first `method` call that satisfies `pred`.
    pub async fn wait_for(&self, method: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(found) = self.calls_to(method).into_iter().find(|b| pred(b)) {
                return found;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no {method} call matched; calls: {:#?}",
                self.calls()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The next message the bot sends to `chat` whose text contains
    /// `needle`, with the id the mock gave it.
    pub async fn wait_for_text(&self, chat: i64, needle: &str) -> (i64, Value) {
        let body = self
            .wait_for("sendMessage", |b| {
                b["chat_id"] == json!(chat)
                    && b["text"].as_str().is_some_and(|t| t.contains(needle))
            })
            .await;
        (body["__message_id"].as_i64().unwrap(), body)
    }

    pub fn texts_to(&self, chat: i64) -> Vec<String> {
        self.calls_to("sendMessage")
            .into_iter()
            .filter(|b| b["chat_id"] == json!(chat))
            .filter_map(|b| b["text"].as_str().map(str::to_owned))
            .collect()
    }
}

async fn serve(
    mut stream: TcpStream,
    token: &str,
    state: &Mutex<State>,
    updated: &Notify,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body: Value =
        serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap_or(json!({}));
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let reply = match path
        .strip_prefix("/bot")
        .and_then(|rest| rest.split_once('/'))
    {
        Some((given, method)) if given == token => handle(method, body, state, updated).await,
        _ => json!({ "ok": false, "error_code": 401, "description": "Unauthorized" }),
    };
    let payload = reply.to_string();
    let status = match reply["error_code"].as_u64() {
        None => "200 OK",
        Some(401) => "401 Unauthorized",
        Some(_) => "400 Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn handle(method: &str, body: Value, state: &Mutex<State>, updated: &Notify) -> Value {
    if method == "getUpdates" {
        let offset = body["offset"].as_i64().unwrap_or(0);
        let wait = Duration::from_secs(body["timeout"].as_u64().unwrap_or(0)).min(MAX_POLL);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = updated.notified();
            {
                let mut state = state.lock().unwrap();
                state
                    .updates
                    .retain(|u| u["update_id"].as_i64().unwrap_or(0) >= offset);
                if !state.updates.is_empty() || tokio::time::Instant::now() >= deadline {
                    return json!({ "ok": true, "result": state.updates.clone() });
                }
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }
    let mut state = state.lock().unwrap();
    let mut recorded = body.clone();
    let result = match method {
        "getMe" => json!({
            "id": 4242, "is_bot": true, "first_name": "Godspeed", "username": BOT_USERNAME,
        }),
        "sendMessage" => {
            state.next_bot_message += 1;
            let id = state.next_bot_message;
            recorded["__message_id"] = json!(id);
            json!({
                "message_id": id,
                "date": 0,
                "chat": { "id": body["chat_id"], "type": "private" },
                "text": body["text"],
            })
        }
        "editMessageText" | "answerCallbackQuery" | "sendChatAction" => json!(true),
        other => {
            return json!({ "ok": false, "error_code": 400, "description": format!("mock has no {other}") });
        }
    };
    state.calls.push(Call {
        method: method.to_string(),
        body: recorded,
    });
    json!({ "ok": true, "result": result })
}
