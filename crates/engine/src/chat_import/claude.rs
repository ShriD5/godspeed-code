//! Claude Code reader: `<projects>/<encoded-cwd>/<session-id>.jsonl`.
//!
//! Each line is one event; conversation lines are `user`/`assistant` with an
//! Anthropic message body (one line per content block). The file stem is the
//! session id `claude --resume` takes. Subagent transcripts
//! (`<session>/subagents/*.jsonl`) and sidechain lines are not conversations.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use zeron_proto::HarnessId;

use super::model::{
    SourceChat, SourceImage, SourceMessage, TranscriptBuilder, basename, parse_time,
    project_root_for, title_from_prompt,
};
use crate::EngineError;

/// Every top-level session file under the projects root.
pub(crate) fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return files;
    };
    for dir in dirs.flatten() {
        let Ok(entries) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The header fields of one line; `serde` skips everything else (message
/// bodies included) without building it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LightLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    git_branch: Option<String>,
    ai_title: Option<String>,
    custom_title: Option<String>,
    is_sidechain: Option<bool>,
    is_meta: Option<bool>,
    message: Option<LightMessage>,
}

#[derive(Deserialize)]
struct LightMessage {
    model: Option<String>,
}

/// Read one session's metadata. `None` for files with no conversation.
pub(crate) fn scan_file(path: &Path) -> Result<Option<SourceChat>, EngineError> {
    let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(None);
    };
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut cwd: Option<String> = None;
    let mut first_at: Option<String> = None;
    let mut last_at: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut model: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut conversation = false;
    for line in reader.lines() {
        let line = line?;
        let Ok(light) = serde_json::from_str::<LightLine>(&line) else {
            continue; // a torn trailing line
        };
        if let Some(title) = light.custom_title {
            custom_title = Some(title);
        }
        if let Some(title) = light.ai_title {
            ai_title = Some(title);
        }
        let kind = light.kind.as_deref().unwrap_or("");
        if !matches!(kind, "user" | "assistant") || light.is_sidechain == Some(true) {
            continue;
        }
        conversation = true;
        if cwd.is_none() {
            cwd = light.cwd;
        }
        if let Some(at) = light.timestamp {
            first_at.get_or_insert_with(|| at.clone());
            last_at = Some(at);
        }
        if let Some(b) = light.git_branch.filter(|b| !b.is_empty() && b != "HEAD") {
            branch = Some(b);
        }
        if kind == "assistant"
            && let Some(m) = light.message.and_then(|m| m.model)
            && !m.starts_with('<')
        {
            model = Some(m);
        }
        if kind == "user" && first_prompt.is_none() && light.is_meta != Some(true) {
            first_prompt = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|v| user_text(&v))
                .filter(|t| !t.trim().is_empty());
        }
    }
    let Some(cwd) = cwd.filter(|_| conversation) else {
        return Ok(None);
    };
    let project_path = project_root_for(&cwd);
    let title = custom_title
        .or(ai_title)
        .or_else(|| first_prompt.as_deref().and_then(title_from_prompt))
        .unwrap_or_else(|| "Claude Code session".into());
    Ok(Some(SourceChat {
        key: format!("claude:{session_id}"),
        project_title: Some(basename(&project_path)),
        project_path,
        title,
        cwd,
        created_at: first_at.as_deref().and_then(parse_time),
        updated_at: last_at.as_deref().and_then(parse_time),
        branch,
        harness: Some(HarnessId::ClaudeCode),
        model,
        reasoning: None,
        access_mode: None,
        archived: false,
        pinned: false,
        resume: Some(session_id.to_string()),
        native_ids: vec![session_id.to_string()],
    }))
}

/// CLI-injected text riding user lines that is not conversation.
fn is_synthetic(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<system-reminder>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<local-command-caveat>",
        "<command-message>",
        "[Request interrupted",
        "Caveat: The messages below were generated",
    ]
    .iter()
    .any(|p| text.starts_with(p))
}

/// `<command-name>/x</command-name><command-args>y</command-args>` → `/x y`.
fn slash_command(text: &str) -> Option<String> {
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let start = text.find(&open)? + open.len();
        let end = text[start..].find(&close)? + start;
        Some(text[start..end].trim().to_string())
    };
    let name = tag("command-name")?;
    let args = tag("command-args").unwrap_or_default();
    Some(if args.is_empty() {
        name
    } else {
        format!("{name} {args}")
    })
}

fn clean_user_text(text: &str) -> Option<String> {
    if let Some(command) = slash_command(text) {
        return Some(command);
    }
    (!is_synthetic(text)).then(|| text.to_string())
}

/// A user line's typed prose (tool results and injected context excluded).
fn user_text(line: &Value) -> Option<String> {
    let content = line.get("message")?.get("content")?;
    match content {
        Value::String(s) => clean_user_text(s),
        Value::Array(blocks) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter_map(clean_user_text)
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n\n"))
        }
        _ => None,
    }
}

/// The full transcript of one session file.
pub(crate) fn transcript(path: &Path) -> Result<Vec<SourceMessage>, EngineError> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut builder = TranscriptBuilder::default();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "user" | "assistant")
            || value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let id = value
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("line-{index}"));
        let id = format!("claude:{id}");
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_time)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0);
        let content = value
            .get("message")
            .and_then(|m| m.get("content"))
            .cloned()
            .unwrap_or(Value::Null);
        if kind == "assistant" {
            for block in content.as_array().map(Vec::as_slice).unwrap_or_default() {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => builder.text(
                        &id,
                        at,
                        block.get("text").and_then(Value::as_str).unwrap_or(""),
                    ),
                    Some("thinking") => builder.reasoning(
                        &id,
                        at,
                        block.get("thinking").and_then(Value::as_str).unwrap_or(""),
                    ),
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let tool_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        builder.tool_call(
                            &id,
                            at,
                            tool_id,
                            zeron_harness::claude::decode_tool_use(name, &input),
                        );
                    }
                    _ => {}
                }
            }
            continue;
        }
        // User lines: tool results settle chips; meta lines and compaction
        // summaries are context the CLI fed the model, not things typed.
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let tool_id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    builder.tool_result(tool_id, is_error);
                }
            }
        }
        if value.get("isMeta").and_then(Value::as_bool) == Some(true)
            || value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let images: Vec<SourceImage> = content
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
            .filter_map(|b| {
                let source = b.get("source")?;
                (source.get("type").and_then(Value::as_str) == Some("base64")).then(|| {
                    SourceImage::Inline {
                        base64: source
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        media_type: source
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string(),
                    }
                })
            })
            .collect();
        let text = user_text(&value).unwrap_or_default();
        builder.user(id, at, text, images);
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_user_text_is_dropped_and_commands_kept() {
        assert_eq!(
            clean_user_text("<system-reminder>x</system-reminder>"),
            None
        );
        assert_eq!(
            clean_user_text("<command-name>/model</command-name><command-args>opus</command-args>")
                .as_deref(),
            Some("/model opus")
        );
        assert_eq!(clean_user_text("fix it").as_deref(), Some("fix it"));
    }
}
