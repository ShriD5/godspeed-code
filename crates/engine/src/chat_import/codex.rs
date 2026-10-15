//! Codex reader: rollout files under `<codex home>/sessions/YYYY/MM/DD/` (and
//! `archived_sessions/`, imported archived).
//!
//! The first line is `session_meta` (thread id, cwd, git); conversation lives
//! in `response_item` lines (messages, reasoning summaries, tool calls and
//! their outputs). `event_msg` lines repeat the same content for live
//! viewers and are ignored. The thread id is what app-server
//! `thread/resume` takes. Titles come from `session_index.jsonl`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use zeron_proto::{HarnessId, TodoItem, TodoStatus, ToolCall};

use super::model::{
    SourceChat, SourceImage, SourceMessage, TranscriptBuilder, basename, parse_time,
    project_root_for, reasoning_level, title_from_prompt,
};
use crate::EngineError;

/// Rollout files, each paired with whether it is archived.
pub(crate) fn rollout_files(home: &Path) -> Vec<(PathBuf, bool)> {
    fn walk(dir: &Path, archived: bool, out: &mut Vec<(PathBuf, bool)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, archived, out);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push((path, archived));
            }
        }
    }
    let mut files = Vec::new();
    walk(&home.join("sessions"), false, &mut files);
    walk(&home.join("archived_sessions"), true, &mut files);
    files.sort();
    files
}

/// Thread titles the Codex apps saved (`{id, thread_name}` per line, last wins).
pub(crate) fn thread_titles(home: &Path) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let Ok(file) = std::fs::File::open(home.join("session_index.jsonl")) else {
        return titles;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            value.get("id").and_then(Value::as_str),
            value.get("thread_name").and_then(Value::as_str),
        ) && !name.trim().is_empty()
        {
            titles.insert(id.to_string(), name.to_string());
        }
    }
    titles
}

#[derive(Deserialize)]
struct LightLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<LightPayload>,
}

#[derive(Deserialize)]
struct LightPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    role: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// Read one rollout's metadata. `None` for subagent threads and rollouts
/// without a conversation.
pub(crate) fn scan_file(
    path: &Path,
    archived: bool,
    titles: &HashMap<String, String>,
) -> Result<Option<SourceChat>, EngineError> {
    let mut lines = BufReader::new(std::fs::File::open(path)?).lines();
    let Some(first) = lines.next().transpose()? else {
        return Ok(None);
    };
    let Ok(first) = serde_json::from_str::<Value>(&first) else {
        return Ok(None);
    };
    if first.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    let meta = first.get("payload").cloned().unwrap_or(Value::Null);
    // Threads an agent spawned are part of their parent's conversation.
    if meta.get("parent_thread_id").is_some()
        || meta.get("source").and_then(|s| s.get("subagent")).is_some()
    {
        return Ok(None);
    }
    let Some(thread_id) = meta.get("id").and_then(Value::as_str).map(str::to_string) else {
        return Ok(None);
    };
    let Some(cwd) = meta.get("cwd").and_then(Value::as_str).map(str::to_string) else {
        return Ok(None);
    };
    let mut first_at = meta
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut last_at = first_at.clone();
    let mut model = None;
    let mut effort = None;
    let mut first_prompt: Option<String> = None;
    let mut conversation = false;
    for line in lines {
        let line = line?;
        let Ok(light) = serde_json::from_str::<LightLine>(&line) else {
            continue;
        };
        let Some(payload) = light.payload else {
            continue;
        };
        match light.kind.as_deref() {
            Some("turn_context") => {
                model = payload.model.or(model);
                effort = payload.effort.or(effort);
            }
            Some("response_item") if payload.kind.as_deref() == Some("message") => {
                let role = payload.role.as_deref().unwrap_or("");
                if role == "user" && first_prompt.is_none() {
                    first_prompt = serde_json::from_str::<Value>(&line)
                        .ok()
                        .and_then(|v| v.get("payload").and_then(message_text))
                        .and_then(|t| clean_user_text(&t));
                }
                if role == "user" || role == "assistant" {
                    conversation = true;
                    if let Some(at) = light.timestamp {
                        first_at.get_or_insert_with(|| at.clone());
                        last_at = Some(at);
                    }
                }
            }
            _ => {}
        }
    }
    if !conversation || first_prompt.is_none() {
        return Ok(None);
    }
    let project_path = project_root_for(&cwd);
    // A saved name can carry an injected block too when Codex named the
    // thread from its first message.
    let title = titles
        .get(&thread_id)
        .map(|name| strip_tagged_blocks(name).trim())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| first_prompt.as_deref().and_then(title_from_prompt))
        .unwrap_or_else(|| "Codex thread".into());
    Ok(Some(SourceChat {
        key: format!("codex:{thread_id}"),
        project_title: Some(basename(&project_path)),
        project_path,
        title,
        cwd,
        created_at: first_at.as_deref().and_then(parse_time),
        updated_at: last_at.as_deref().and_then(parse_time),
        branch: meta
            .get("git")
            .and_then(|g| g.get("branch"))
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty())
            .map(str::to_string),
        harness: Some(HarnessId::Codex),
        model,
        reasoning: effort.as_deref().and_then(reasoning_level),
        access_mode: None,
        archived,
        pinned: false,
        resume: Some(thread_id.clone()),
        native_ids: vec![thread_id],
    }))
}

/// Concatenated text blocks of a `message` payload.
fn message_text(payload: &Value) -> Option<String> {
    let blocks = payload.get("content")?.as_array()?;
    let text: Vec<&str> = blocks
        .iter()
        .filter(|b| {
            matches!(
                b.get("type").and_then(Value::as_str),
                Some("input_text" | "output_text" | "text")
            )
        })
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then(|| text.join("\n\n"))
}

/// Context Codex injects as user messages (instructions, environment,
/// turn bookkeeping) — not things the user typed. Codex keeps adding new
/// `<tag>…</tag>` wrappers (`<recommended_plugins>` was the first we missed),
/// so any leading tagged block goes, not just the known ones; the text the
/// user typed after it stays.
fn clean_user_text(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("# AGENTS.md instructions") {
        return None;
    }
    let rest = strip_tagged_blocks(trimmed);
    if rest.is_empty() {
        return None;
    }
    Some(if rest.len() == trimmed.len() {
        text.to_string()
    } else {
        rest.to_string()
    })
}

/// `text` past any leading `<name …>…</name>` blocks (and the whitespace
/// between them). An opening tag with no matching close is the user's own
/// text and stops the strip.
fn strip_tagged_blocks(text: &str) -> &str {
    let mut rest = text.trim_start();
    while let Some(after_lt) = rest.strip_prefix('<') {
        let Some(end) = after_lt.find('>') else {
            break;
        };
        let tag = &after_lt[..end];
        let name = tag.split_whitespace().next().unwrap_or_default();
        let plausible = (1..=64).contains(&tag.len())
            && name.starts_with(|c: char| c.is_ascii_alphabetic())
            && !tag.contains(['<', '/', '\n']);
        if !plausible {
            break;
        }
        let body = &after_lt[end + 1..];
        // Codex closes with the full opening text (`</permissions
        // instructions>`); markup closes with the bare name.
        let close = [format!("</{tag}>"), format!("</{name}>")]
            .into_iter()
            .filter_map(|close| body.find(&close).map(|at| at + close.len()))
            .min();
        let Some(close) = close else {
            break;
        };
        rest = body[close..].trim_start();
    }
    rest
}

/// A rollout tool call as a typed call.
fn function_call(name: &str, arguments: &Value) -> ToolCall {
    let str_arg = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match name {
        "exec_command" | "shell" | "container.exec" | "local_shell" => {
            let command = str_arg("cmd").or_else(|| str_arg("command")).or_else(|| {
                arguments
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            });
            ToolCall::Exec {
                command: command.unwrap_or_default(),
            }
        }
        "apply_patch" => ToolCall::ApplyPatch { path: None },
        "update_plan" => ToolCall::Todo {
            items: arguments
                .get("plan")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|step| {
                    TodoItem::new(
                        step.get("step").and_then(Value::as_str).unwrap_or(""),
                        TodoStatus::parse(step.get("status").and_then(Value::as_str).unwrap_or("")),
                    )
                })
                .collect(),
        },
        "view_image" => ToolCall::ReadFile {
            path: str_arg("path").unwrap_or_default(),
        },
        _ => match name.split_once("__") {
            Some((server, tool)) => ToolCall::Mcp {
                server: server
                    .trim_start_matches("mcp")
                    .trim_matches('_')
                    .to_string(),
                tool: tool.to_string(),
                input: None,
            },
            None => ToolCall::Unknown {
                name: name.to_string(),
                input: None,
            },
        },
    }
}

/// The first `*** Update/Add/Delete File:` path of a freeform patch.
fn patch_path(patch: &str) -> Option<String> {
    patch.lines().find_map(|line| {
        ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
            .iter()
            .find_map(|p| line.strip_prefix(p))
            .map(|p| p.trim().to_string())
    })
}

/// `data:image/png;base64,...` → (media type, payload).
fn data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type.to_string(), data.to_string()))
}

/// The full transcript of one rollout.
pub(crate) fn transcript(path: &Path) -> Result<Vec<SourceMessage>, EngineError> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut builder = TranscriptBuilder::default();
    let mut thread = String::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = value.get("payload").unwrap_or(&Value::Null);
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if thread.is_empty() {
                    thread = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                continue;
            }
            Some("response_item") => {}
            _ => continue,
        }
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_time)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0);
        let id = format!("codex:{thread}:{index}");
        match payload.get("type").and_then(Value::as_str) {
            Some("message") => match payload.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let images = payload
                        .get("content")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("input_image"))
                        .filter_map(|b| b.get("image_url").and_then(Value::as_str))
                        .filter_map(data_url)
                        .map(|(media_type, base64)| SourceImage::Inline { base64, media_type })
                        .collect::<Vec<_>>();
                    let text = message_text(payload)
                        .and_then(|t| clean_user_text(&t))
                        .unwrap_or_default();
                    builder.user(id, at, text, images);
                }
                Some("assistant") => {
                    builder.text(&id, at, &message_text(payload).unwrap_or_default());
                }
                _ => {}
            },
            Some("reasoning") => {
                let summary: Vec<&str> = payload
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|s| s.get("text").and_then(Value::as_str))
                    .collect();
                builder.reasoning(&id, at, &summary.join("\n\n"));
            }
            Some("function_call") => {
                let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
                let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
                let arguments = payload
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .unwrap_or(Value::Null);
                builder.tool_call(&id, at, call_id, function_call(name, &arguments));
            }
            Some("custom_tool_call") => {
                let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
                let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
                let input = payload.get("input").and_then(Value::as_str).unwrap_or("");
                let call = match name {
                    "apply_patch" => ToolCall::ApplyPatch {
                        path: patch_path(input),
                    },
                    other => function_call(other, &Value::Null),
                };
                builder.tool_call(&id, at, call_id, call);
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
                builder.tool_result(call_id, false);
            }
            Some("web_search_call") => {
                let query = payload
                    .get("action")
                    .and_then(|a| a.get("query"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let call_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| id.clone());
                builder.tool_call(&id, at, &call_id, ToolCall::WebSearch { query });
                builder.tool_result(&call_id, false);
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
    fn injected_context_is_not_a_user_message() {
        assert_eq!(
            clean_user_text("<environment_context>\n</environment_context>"),
            None
        );
        assert_eq!(clean_user_text("# AGENTS.md instructions for /x"), None);
        assert!(clean_user_text("ship it").is_some());
        // Any leading tagged block is injected context, known or not.
        assert_eq!(
            clean_user_text("<recommended_plugins>\n- gh\n</recommended_plugins>"),
            None
        );
        assert_eq!(
            clean_user_text(
                "<recommended_plugins>gh</recommended_plugins>\n\
                 <permissions instructions>x</permissions instructions>\n\nfix the login bug"
            )
            .as_deref(),
            Some("fix the login bug")
        );
        assert_eq!(
            clean_user_text("<skill name=\"x\">body</skill> then this").as_deref(),
            Some("then this")
        );
        // An unclosed tag or a comparison is the user's own text.
        assert_eq!(
            clean_user_text("<div> is broken").as_deref(),
            Some("<div> is broken")
        );
        assert_eq!(clean_user_text("<3 thanks").as_deref(), Some("<3 thanks"));
        assert_eq!(
            patch_path("*** Begin Patch\n*** Update File: src/a.rs\n@@").as_deref(),
            Some("src/a.rs")
        );
        assert!(matches!(
            function_call("exec_command", &serde_json::json!({"cmd": "ls"})),
            ToolCall::Exec { command } if command == "ls"
        ));
    }
}
