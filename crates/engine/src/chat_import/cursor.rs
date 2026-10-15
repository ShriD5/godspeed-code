//! Cursor Agent CLI reader: `<cursor home>/chats/<md5(workspace)>/<chat-id>/store.db`.
//!
//! Each chat is a small content-addressed SQLite store: `meta` holds one
//! hex-encoded JSON row (`agentId`, `name`, `createdAt`, `latestRootBlobId`),
//! and `blobs` holds the messages as JSON (AI SDK shape: `{role, content}`
//! with `text` / `reasoning` / `tool-call` / `tool-result` parts) plus
//! protobuf tree nodes. The latest root node lists the conversation's
//! message blob ids in order (field 1) and the workspace URI (field 9).
//!
//! Messages carry no timestamps; they take the chat's creation time, and the
//! store's modification time stands in for its last activity.
//!
//! These chats can't be resumed here: Godspeed's Cursor harness drives the
//! `@cursor/sdk` local runtime, whose agent store lives under
//! `~/.cursor/projects/…`, not the CLI's `~/.cursor/chats`, so `Agent.resume`
//! can't find a CLI chat id. They import as history on the Cursor harness.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use zeron_proto::{HarnessId, ToolCall};

use super::model::{
    SourceChat, SourceMessage, TranscriptBuilder, basename, project_root_for, title_from_prompt,
};
use super::snapshot::SqliteCopy;
use crate::EngineError;

/// Every chat store under the chats root.
pub(crate) fn store_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(workspaces) = std::fs::read_dir(root) else {
        return files;
    };
    for workspace in workspaces.flatten() {
        let Ok(chats) = std::fs::read_dir(workspace.path()) else {
            continue;
        };
        for chat in chats.flatten() {
            let store = chat.path().join("store.db");
            if store.is_file() {
                files.push(store);
            }
        }
    }
    files.sort();
    files
}

/// One chat store read in full (stores are tiny).
struct Store {
    meta: Value,
    messages: Vec<Value>,
    workspace: Option<String>,
}

fn sql_err(err: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("Cursor chat store: {err}"))
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The length-delimited fields of a protobuf message, in order. Stops at the
/// first wire type it doesn't need (none appear in tree nodes today).
fn proto_fields(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(key) = varint(bytes, &mut at) else {
            break;
        };
        match key & 7 {
            0 => {
                if varint(bytes, &mut at).is_none() {
                    break;
                }
            }
            2 => {
                let Some(len) = varint(bytes, &mut at) else {
                    break;
                };
                let end = at.saturating_add(len as usize);
                let Some(value) = bytes.get(at..end) else {
                    break;
                };
                fields.push((key >> 3, value));
                at = end;
            }
            _ => break,
        }
    }
    fields
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `file:///a/b%20c` → `/a/b c`.
fn file_uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = rest
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn read_store(path: &Path) -> Result<Option<Store>, EngineError> {
    let db = SqliteCopy::open(path)?;
    let conn = &db.conn;
    let meta_rows: Vec<String> = conn
        .prepare("SELECT value FROM meta")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<_, _>>()
        })
        .map_err(sql_err)?;
    let Some(meta) = meta_rows.iter().find_map(|row| {
        let bytes = unhex(row)?;
        let meta: Value = serde_json::from_slice(&bytes).ok()?;
        meta.get("agentId").is_some().then_some(meta)
    }) else {
        return Ok(None);
    };
    let Some(root_id) = meta.get("latestRootBlobId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let blob = |id: &str| -> Result<Option<Vec<u8>>, EngineError> {
        conn.query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            err => Err(sql_err(err)),
        })
    };
    let Some(root) = blob(root_id)? else {
        return Ok(None);
    };
    let mut messages = Vec::new();
    let mut workspace = None;
    for (field, value) in proto_fields(&root) {
        match field {
            1 if value.len() == 32 => {
                if let Some(data) = blob(&hex(value))?
                    && let Ok(message) = serde_json::from_slice::<Value>(&data)
                {
                    messages.push(message);
                }
            }
            9 => {
                workspace = std::str::from_utf8(value).ok().and_then(file_uri_path);
            }
            _ => {}
        }
    }
    Ok(Some(Store {
        meta,
        messages,
        workspace,
    }))
}

/// A user message's typed text: the `<user_query>` body; injected context
/// (`<user_info>`, rules, attached-file dumps) is dropped.
fn user_text(message: &Value) -> Option<String> {
    let texts: Vec<String> = match message.get("content")? {
        Value::String(text) => vec![text.clone()],
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
            .collect(),
        _ => return None,
    };
    let query: Vec<String> = texts
        .iter()
        .filter_map(|text| {
            let start = text.find("<user_query>")? + "<user_query>".len();
            let end = text[start..]
                .find("</user_query>")
                .map_or(text.len(), |e| start + e);
            Some(text[start..end].trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .collect();
    if !query.is_empty() {
        return Some(query.join("\n\n"));
    }
    let plain: Vec<&String> = texts
        .iter()
        .filter(|t| !t.trim_start().starts_with('<') && !t.trim().is_empty())
        .collect();
    (!plain.is_empty()).then(|| {
        plain
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    })
}

/// The CLI's tool names (`Shell`, `StrReplace`) in the SDK vocabulary the
/// Cursor harness decodes.
fn tool_call(name: &str, args: &Value) -> ToolCall {
    let sdk = match name {
        "StrReplace" | "MultiEdit" | "Edit" => "edit".to_string(),
        "WebSearch" => "webSearch".to_string(),
        "WebFetch" => "webFetch".to_string(),
        "TodoWrite" | "UpdateTodos" => "updateTodos".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    };
    zeron_harness::cursor::decode_tool(&sdk, args)
}

/// The workspace a chat ran in: the root node's URI, else the path the CLI
/// wrote into its injected `<user_info>`.
fn workspace_of(store: &Store) -> Option<String> {
    store.workspace.clone().or_else(|| {
        store.messages.iter().find_map(|m| {
            let text = match m.get("content")? {
                Value::String(t) => t.clone(),
                _ => return None,
            };
            text.lines()
                .find_map(|l| l.trim().strip_prefix("Workspace Path:"))
                .map(|p| p.trim().to_string())
        })
    })
}

fn modified(path: &Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

/// Read one chat's metadata. `None` for stores with no conversation.
pub(crate) fn scan_file(path: &Path) -> Result<Option<SourceChat>, EngineError> {
    let Some(store) = read_store(path)? else {
        return Ok(None);
    };
    let Some(agent_id) = store.meta.get("agentId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let first_prompt = store
        .messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .find_map(user_text);
    let Some(first_prompt) = first_prompt else {
        return Ok(None);
    };
    let Some(cwd) = workspace_of(&store) else {
        return Ok(None);
    };
    let project_path = project_root_for(&cwd);
    let name = store
        .meta
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty() && *n != "New Agent");
    let created_at = store
        .meta
        .get("createdAt")
        .and_then(Value::as_i64)
        .and_then(DateTime::<Utc>::from_timestamp_millis);
    Ok(Some(SourceChat {
        key: format!("cursor:{agent_id}"),
        project_title: Some(basename(&project_path)),
        project_path,
        title: name
            .map(str::to_string)
            .or_else(|| title_from_prompt(&first_prompt))
            .unwrap_or_else(|| "Cursor chat".into()),
        cwd,
        created_at,
        updated_at: modified(path).or(created_at),
        branch: None,
        harness: Some(HarnessId::Cursor),
        model: None,
        reasoning: None,
        access_mode: None,
        archived: false,
        pinned: false,
        resume: None,
        native_ids: vec![agent_id.to_string()],
    }))
}

/// The full transcript of one chat store.
pub(crate) fn transcript(path: &Path) -> Result<Vec<SourceMessage>, EngineError> {
    let Some(store) = read_store(path)? else {
        return Ok(Vec::new());
    };
    let agent = store
        .meta
        .get("agentId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let at = store
        .meta
        .get("createdAt")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut builder = TranscriptBuilder::default();
    for (index, message) in store.messages.iter().enumerate() {
        let id = format!("cursor:{agent}:{index}");
        let parts = message
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = user_text(message) {
                    builder.user(id, at, text, Vec::new());
                }
            }
            Some("assistant") => {
                if let Some(text) = message.get("content").and_then(Value::as_str) {
                    builder.text(&id, at, text);
                }
                for part in parts {
                    let text = || part.get("text").and_then(Value::as_str).unwrap_or("");
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => builder.text(&id, at, text()),
                        Some("reasoning") => builder.reasoning(&id, at, text()),
                        Some("tool-call") => {
                            let name = part.get("toolName").and_then(Value::as_str).unwrap_or("");
                            let call_id =
                                part.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                            let args = part.get("args").cloned().unwrap_or(Value::Null);
                            builder.tool_call(&id, at, call_id, tool_call(name, &args));
                        }
                        _ => {}
                    }
                }
            }
            Some("tool") => {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) != Some("tool-result") {
                        continue;
                    }
                    let call_id = part.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                    let failed = part.get("isError").and_then(Value::as_bool) == Some(true)
                        || message
                            .pointer(
                                "/providerOptions/cursor/highLevelToolCallResult/output/failure",
                            )
                            .is_some();
                    builder.tool_result(call_id, failed);
                }
            }
            _ => {}
        }
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protobuf_fields_and_uris_decode() {
        let mut node = vec![0x0a, 2, 0xab, 0xcd];
        let uri = b"file:///Users/me/my%20app";
        node.push((9 << 3) | 2);
        node.push(uri.len() as u8);
        node.extend_from_slice(uri);
        node.extend_from_slice(&[(10 << 3), 1]);
        let fields = proto_fields(&node);
        assert_eq!(fields.len(), 2);
        assert_eq!(hex(fields[0].1), "abcd");
        assert_eq!(
            file_uri_path(std::str::from_utf8(fields[1].1).unwrap()).as_deref(),
            Some("/Users/me/my app")
        );
        assert_eq!(unhex("7b7d").as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn user_query_is_the_typed_text() {
        let context = serde_json::json!({"role": "user", "content": "<user_info>\nWorkspace Path: /x\n</user_info>"});
        assert_eq!(user_text(&context), None);
        let query = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "<user_query>\nOpen prs ?\n</user_query>"}]});
        assert_eq!(user_text(&query).as_deref(), Some("Open prs ?"));
        assert!(matches!(
            tool_call("StrReplace", &serde_json::json!({"path": "a.rs"})),
            ToolCall::EditFile { path, .. } if path == "a.rs"
        ));
        assert!(matches!(
            tool_call("Shell", &serde_json::json!({"command": "ls"})),
            ToolCall::Exec { command } if command == "ls"
        ));
    }
}
