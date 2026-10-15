//! T3 Code reader: the server's projection tables in `userdata/state.sqlite`.
//!
//! T3 Code keeps the database open in WAL mode while it runs, so it is read
//! through a private copy ([`SqliteCopy`]), never in place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde_json::Value;
use zeron_proto::{AccessMode, HarnessId, ToolCall};

use super::model::{
    SourceChat, SourceImage, SourceMessage, TranscriptBuilder, basename, harness_for_provider,
    parse_time, reasoning_level,
};
use super::snapshot::SqliteCopy;
use crate::EngineError;

const DB_NAMES: [&str; 2] = ["state.sqlite", "statev2.sqlite"];

/// The T3 Code database located under a T3 home (`~/.t3` / `$T3CODE_HOME`).
pub(crate) fn database_path(home: &Path) -> Option<PathBuf> {
    DB_NAMES
        .iter()
        .map(|name| home.join("userdata").join(name))
        .find(|path| path.is_file())
}

/// An open snapshot of the T3 database.
pub(crate) struct T3Snapshot {
    db: SqliteCopy,
    attachments: PathBuf,
}

fn sql_err(err: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("T3 Code database: {err}"))
}

impl T3Snapshot {
    /// Copy and open the database at `db` read-only.
    pub fn open(db: &Path, attachments: PathBuf) -> Result<Self, EngineError> {
        Ok(Self {
            db: SqliteCopy::open(db)?,
            attachments,
        })
    }

    fn columns(&self, table: &str) -> Result<Vec<String>, EngineError> {
        let mut stmt = self
            .db
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    }

    /// `table.col`, or `NULL` for a column an older T3 build doesn't have.
    fn col(columns: &[String], alias: &str, name: &str) -> String {
        if columns.iter().any(|c| c == name) {
            format!("{alias}.{name}")
        } else {
            "NULL".into()
        }
    }

    /// Every native session id a T3 thread resumes — the Claude and Codex
    /// readers skip those sessions so a conversation lands once, via T3.
    pub fn owned_native_ids(&self) -> Result<Vec<String>, EngineError> {
        if self.columns("provider_session_runtime")?.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .db
            .conn
            .prepare("SELECT resume_cursor_json FROM provider_session_runtime")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Option<String>>(0))
            .map_err(sql_err)?;
        let mut ids = Vec::new();
        for cursor in rows.flatten().flatten() {
            let Ok(cursor) = serde_json::from_str::<Value>(&cursor) else {
                continue;
            };
            for key in ["resume", "threadId", "sessionId"] {
                if let Some(id) = cursor.get(key).and_then(Value::as_str)
                    && !id.is_empty()
                {
                    ids.push(id.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Live (not deleted) threads of live projects.
    pub fn scan(&self, warnings: &mut Vec<String>) -> Result<Vec<SourceChat>, EngineError> {
        let pcols = self.columns("projection_projects")?;
        let tcols = self.columns("projection_threads")?;
        if pcols.is_empty() || tcols.is_empty() {
            return Err(EngineError::Other(
                "T3 Code database has no projection tables".into(),
            ));
        }
        let rcols = self.columns("provider_session_runtime")?;
        let has_runtime = !rcols.is_empty();
        let sql = format!(
            "SELECT t.thread_id, t.title, t.branch, t.worktree_path, t.created_at, t.updated_at,
                    {archived}, {pinned}, {model}, {mode},
                    p.title, p.workspace_root,
                    {provider}, {cursor}
             FROM projection_threads t
             JOIN projection_projects p ON p.project_id = t.project_id
             {join}
             WHERE t.deleted_at IS NULL AND p.deleted_at IS NULL
             ORDER BY t.created_at",
            archived = Self::col(&tcols, "t", "archived_at"),
            pinned = Self::col(&tcols, "t", "pinned_at"),
            model = Self::col(&tcols, "t", "model_selection_json"),
            mode = Self::col(&tcols, "t", "runtime_mode"),
            provider = if has_runtime {
                "r.provider_name"
            } else {
                "NULL"
            },
            cursor = if has_runtime {
                "r.resume_cursor_json"
            } else {
                "NULL"
            },
            join = if has_runtime {
                "LEFT JOIN provider_session_runtime r ON r.thread_id = t.thread_id"
            } else {
                ""
            },
        );
        let mut stmt = self.db.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ThreadRow {
                    thread_id: row.get(0)?,
                    title: row.get(1)?,
                    branch: row.get(2)?,
                    worktree_path: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    archived_at: row.get(6)?,
                    pinned_at: row.get(7)?,
                    model_selection: row.get(8)?,
                    runtime_mode: row.get(9)?,
                    project_title: row.get(10)?,
                    workspace_root: row.get(11)?,
                    provider: row.get(12)?,
                    resume_cursor: row.get(13)?,
                })
            })
            .map_err(sql_err)?;
        let mut chats = Vec::new();
        for row in rows {
            match row {
                Ok(row) => chats.push(row.into_chat()),
                Err(err) => warnings.push(format!("T3 thread row: {err}")),
            }
        }
        Ok(chats)
    }

    /// The thread's messages and tool activity, merged in time order.
    pub fn transcript(&self, thread_id: &str) -> Result<Vec<SourceMessage>, EngineError> {
        let mut items: Vec<(i64, u8, i64, Item)> = Vec::new();
        let millis = |at: &str| parse_time(at).map(|t| t.timestamp_millis()).unwrap_or(0);
        let mut stmt = self
            .db
            .conn
            .prepare(
                "SELECT message_id, role, text, created_at, attachments_json
                 FROM projection_thread_messages WHERE thread_id = ?1
                 ORDER BY created_at, message_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![thread_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(sql_err)?;
        for row in rows {
            let (id, role, text, at, attachments) = row.map_err(sql_err)?;
            let item = match role.as_str() {
                "user" => Item::User {
                    id,
                    text,
                    attachments,
                },
                "assistant" => Item::Assistant { id, text },
                _ => continue,
            };
            items.push((millis(&at), 0, 0, item));
        }
        if !self.columns("projection_thread_activities")?.is_empty() {
            let mut stmt = self
                .db
                .conn
                .prepare(
                    "SELECT activity_id, kind, tone, payload_json, created_at, sequence
                     FROM projection_thread_activities
                     WHERE thread_id = ?1 AND kind IN ('tool.started', 'tool.completed')
                     ORDER BY created_at, sequence",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![thread_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                })
                .map_err(sql_err)?;
            // One chip per tool call: its completion when there is one,
            // else the start (a run that died mid-tool).
            let mut tools: HashMap<String, usize> = HashMap::new();
            for row in rows {
                let (activity_id, kind, tone, payload, at, sequence) = row.map_err(sql_err)?;
                let Ok(payload) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                let call_id = payload
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or(&activity_id)
                    .to_string();
                let completed = kind == "tool.completed";
                let is_error = tone == "error"
                    || payload.get("status").and_then(Value::as_str) == Some("failed");
                let tool = Item::Tool {
                    call_id: call_id.clone(),
                    call: tool_call(&payload),
                    completed,
                    is_error,
                };
                match tools.get(&call_id) {
                    // Keep the start's position (the chip appears where the
                    // call began) but the completion's content.
                    Some(&index) if completed => items[index].3 = tool,
                    Some(_) => {}
                    None => {
                        tools.insert(call_id, items.len());
                        items.push((millis(&at), 1, sequence.unwrap_or(0), tool));
                    }
                }
            }
        }
        // Stable: equal keys keep query order.
        items.sort_by_key(|item| (item.0, item.1, item.2));

        let mut builder = TranscriptBuilder::default();
        for (ms, _, _, item) in items {
            match item {
                Item::User {
                    id,
                    text,
                    attachments,
                } => {
                    let images = attachments
                        .as_deref()
                        .map(|json| self.images(json))
                        .unwrap_or_default();
                    builder.user(format!("t3:{id}"), ms, text, images);
                }
                Item::Assistant { id, text } => builder.text(&format!("t3:{id}"), ms, &text),
                Item::Tool {
                    call_id,
                    call,
                    completed,
                    is_error,
                } => {
                    builder.tool_call(&format!("t3:{call_id}"), ms, &call_id, call);
                    if completed {
                        builder.tool_result(&call_id, is_error);
                    }
                }
            }
        }
        Ok(builder.finish())
    }

    /// Image attachments live as `<attachments>/<id>.<ext>`.
    fn images(&self, json: &str) -> Vec<SourceImage> {
        let Ok(Value::Array(list)) = serde_json::from_str::<Value>(json) else {
            return Vec::new();
        };
        list.iter()
            .filter(|a| a.get("type").and_then(Value::as_str) == Some("image"))
            .filter_map(|a| a.get("id").and_then(Value::as_str))
            .filter_map(|id| self.attachment_file(id))
            .map(|path| SourceImage::File {
                path,
                root: self.attachments.clone(),
            })
            .collect()
    }

    fn attachment_file(&self, id: &str) -> Option<PathBuf> {
        if id.contains(['/', '\\']) || id.starts_with('.') {
            return None;
        }
        ["png", "jpg", "jpeg", "webp", "gif"]
            .iter()
            .map(|ext| self.attachments.join(format!("{id}.{ext}")))
            .find(|p| p.is_file())
    }
}

enum Item {
    User {
        id: String,
        text: String,
        attachments: Option<String>,
    },
    Assistant {
        id: String,
        text: String,
    },
    Tool {
        call_id: String,
        call: ToolCall,
        completed: bool,
        is_error: bool,
    },
}

struct ThreadRow {
    thread_id: String,
    title: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    created_at: String,
    updated_at: String,
    archived_at: Option<String>,
    pinned_at: Option<String>,
    model_selection: Option<String>,
    runtime_mode: Option<String>,
    project_title: String,
    workspace_root: String,
    provider: Option<String>,
    resume_cursor: Option<String>,
}

impl ThreadRow {
    fn into_chat(self) -> SourceChat {
        let selection = self
            .model_selection
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);
        let instance = selection
            .get("instanceId")
            .or_else(|| selection.get("provider"))
            .and_then(Value::as_str);
        // The runtime row names the driver that actually ran; the selection's
        // instance id is the fallback for threads that never started one.
        let harness = self
            .provider
            .as_deref()
            .and_then(harness_for_provider)
            .or_else(|| instance.and_then(harness_for_provider));
        let reasoning = selection
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| {
                options.iter().find_map(|o| {
                    let id = o.get("id").and_then(Value::as_str)?;
                    matches!(id, "effort" | "reasoningEffort" | "reasoning")
                        .then(|| o.get("value").and_then(Value::as_str))
                        .flatten()
                        .and_then(reasoning_level)
                })
            });
        let cursor = self
            .resume_cursor
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);
        let cursor_id = |key: &str| {
            cursor
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        // Only ids whose resume semantics match ours ride along: Claude's
        // session id (`--resume`) and Codex's thread id (`thread/resume`).
        let resume = match harness {
            Some(HarnessId::ClaudeCode) => cursor_id("resume"),
            Some(HarnessId::Codex) => cursor_id("threadId"),
            _ => None,
        };
        // Claude cursors name the T3 thread under `threadId`; Codex cursors
        // name the Codex thread there.
        let native_ids = match harness {
            Some(HarnessId::ClaudeCode) => cursor_id("resume").into_iter().collect(),
            Some(HarnessId::Codex) => cursor_id("threadId").into_iter().collect(),
            _ => ["resume", "threadId", "sessionId"]
                .into_iter()
                .filter_map(cursor_id)
                .filter(|id| id != &self.thread_id)
                .collect(),
        };
        let access_mode = match self.runtime_mode.as_deref() {
            Some("full-access") => Some(AccessMode::FullAccess),
            Some("approval-required") => Some(AccessMode::Supervised),
            Some("auto-accept-edits") => Some(AccessMode::AutoAcceptEdits),
            _ => None,
        };
        let project_path = self.workspace_root.trim_end_matches('/').to_string();
        let cwd = self
            .worktree_path
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| project_path.clone());
        let title = if self.title.trim().is_empty() {
            "Imported thread".to_string()
        } else {
            self.title
        };
        SourceChat {
            key: format!("t3:{}", self.thread_id),
            project_title: Some(if self.project_title.trim().is_empty() {
                basename(&project_path)
            } else {
                self.project_title
            }),
            project_path,
            title,
            cwd,
            created_at: parse_time(&self.created_at),
            updated_at: parse_time(&self.updated_at),
            branch: self.branch.filter(|b| !b.is_empty()),
            harness,
            model: selection
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            reasoning,
            access_mode,
            archived: self.archived_at.is_some(),
            pinned: self.pinned_at.is_some(),
            resume,
            native_ids,
        }
    }
}

/// A T3 tool activity as a typed call. Claude-driven threads carry the raw
/// `tool_use` name + input, decoded exactly as the live Claude harness does;
/// other drivers fall back to the activity's item type and title.
fn tool_call(payload: &Value) -> ToolCall {
    let data = payload.get("data").unwrap_or(&Value::Null);
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Tool")
        .to_string();
    if let Some(name) = data
        .get("toolName")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    {
        let input = data.get("input").cloned().unwrap_or(Value::Null);
        // A started activity's input is still empty (`{}`); the detail line
        // holds the command for shells.
        return zeron_harness::claude::decode_tool_use(name, &input);
    }
    let item = data.get("item").unwrap_or(&Value::Null);
    let item_str = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
    match payload.get("itemType").and_then(Value::as_str) {
        Some("command_execution") => ToolCall::Exec {
            command: item_str("command").unwrap_or(title),
        },
        Some("file_change") => ToolCall::ApplyPatch { path: None },
        Some("web_search") => ToolCall::WebSearch {
            query: item_str("query").unwrap_or(title),
        },
        Some("mcp_tool_call") => ToolCall::Mcp {
            server: item_str("server").unwrap_or_else(|| "mcp".into()),
            tool: item_str("tool").unwrap_or(title),
            input: None,
        },
        _ => ToolCall::Unknown {
            name: title,
            input: None,
        },
    }
}
