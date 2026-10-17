//! The source-neutral shape every reader produces, and the one place it turns
//! into session-doc entries.
//!
//! Readers never build doc parts themselves: assistant turns are recorded as
//! the same [`AgentEvent`]s a live run emits and folded through
//! [`fold_event_into_parts`], with tool calls passed through
//! [`sanitize_tool_call`] — so an imported transcript renders exactly like a
//! native one and obeys the same render-only privacy policy (tool outputs and
//! heavy inputs never enter the doc).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use zeron_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, continuation_id,
    fold_event_into_parts, sanitize_tool_call, split_parts,
};
use zeron_proto::{AccessMode, AgentEvent, HarnessId, ReasoningLevel, ToolCall};

/// Doc entries minted by the importer carry this id prefix; a chat whose
/// entries all have it has never been continued here, so a re-import may
/// append to it.
pub(crate) const ENTRY_PREFIX: &str = "import:";

/// One source conversation's metadata (no transcript — readers load that
/// lazily, only for chats actually imported).
#[derive(Debug, Clone)]
pub(crate) struct SourceChat {
    /// `t3:<thread>`, `claude:<session>`, `codex:<thread>`.
    pub key: String,
    /// The folder the chat's space maps to.
    pub project_path: String,
    pub project_title: Option<String>,
    pub title: String,
    /// Where the agent ran (a worktree path, else the project folder).
    pub cwd: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub branch: Option<String>,
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub access_mode: Option<AccessMode>,
    pub archived: bool,
    pub pinned: bool,
    /// The harness-native session id sending a message resumes (Claude
    /// session id / Codex thread id). Only set for harnesses whose resume
    /// semantics match the source's id.
    pub resume: Option<String>,
    /// Native ids this chat owns, for cross-source dedupe (the resume id plus
    /// any provider session id that can't be resumed here).
    pub native_ids: Vec<String>,
}

impl SourceChat {
    pub fn resumable(&self) -> bool {
        self.resume.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

/// An image a user attached. `File` images are copied into this profile's
/// uploads; `Inline` ones (base64 in the transcript) are written there.
#[derive(Debug, Clone)]
pub(crate) enum SourceImage {
    File {
        path: PathBuf,
        /// The directory the file must stay inside (the source's own store).
        root: PathBuf,
    },
    Inline {
        base64: String,
        media_type: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SourceMessage {
    /// Stable across re-imports (the source's own message id).
    pub id: String,
    pub role: Role,
    /// Epoch millis.
    pub at: i64,
    /// User text (assistant turns carry `events` instead).
    pub text: String,
    pub images: Vec<SourceImage>,
    pub events: Vec<AgentEvent>,
}

/// Accumulates a transcript in source order: user messages stand alone; every
/// assistant item between two user messages folds into one assistant turn,
/// the way a live run writes one entry per turn.
#[derive(Default)]
pub(crate) struct TranscriptBuilder {
    messages: Vec<SourceMessage>,
    current: Option<SourceMessage>,
}

impl TranscriptBuilder {
    pub fn user(&mut self, id: String, at: i64, text: String, images: Vec<SourceImage>) {
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        self.flush();
        self.messages.push(SourceMessage {
            id,
            role: Role::User,
            at,
            text,
            images,
            events: Vec::new(),
        });
    }

    fn turn(&mut self, id: &str, at: i64) -> &mut SourceMessage {
        self.current.get_or_insert_with(|| SourceMessage {
            id: id.to_string(),
            role: Role::Assistant,
            at,
            text: String::new(),
            images: Vec::new(),
            events: Vec::new(),
        })
    }

    /// Assistant prose. Separate source blocks become separate paragraphs.
    pub fn text(&mut self, id: &str, at: i64, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let turn = self.turn(id, at);
        let text = match turn.events.last() {
            Some(AgentEvent::TextDelta { .. }) => format!("\n\n{text}"),
            _ => text.to_string(),
        };
        turn.events.push(AgentEvent::TextDelta { text });
    }

    pub fn reasoning(&mut self, id: &str, at: i64, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let turn = self.turn(id, at);
        let text = match turn.events.last() {
            Some(AgentEvent::ReasoningDelta { .. }) => format!("\n\n{text}"),
            _ => text.to_string(),
        };
        turn.events.push(AgentEvent::ReasoningDelta { text });
    }

    pub fn tool_call(&mut self, id: &str, at: i64, tool_id: &str, call: ToolCall) {
        self.turn(id, at).events.push(AgentEvent::ToolCall {
            id: tool_id.to_string(),
            call: sanitize_tool_call(&call),
        });
    }

    /// A tool result settles its chip. Results with no open turn (a
    /// truncated transcript) are dropped.
    pub fn tool_result(&mut self, tool_id: &str, is_error: bool) {
        if let Some(turn) = self.current.as_mut() {
            turn.events.push(AgentEvent::ToolResult {
                id: tool_id.to_string(),
                is_error,
                output: None,
                diff: None,
            });
        }
    }

    fn flush(&mut self) {
        if let Some(turn) = self.current.take()
            && !turn.events.is_empty()
        {
            self.messages.push(turn);
        }
    }

    pub fn finish(mut self) -> Vec<SourceMessage> {
        self.flush();
        self.messages
    }
}

/// The doc entries for one message. Assistant parts fold from their events;
/// any tool chip still open is settled — the source turn is over, and an
/// unresolved chip would spin forever. Oversized turns split into
/// continuation entries exactly as the live writer splits them.
pub(crate) fn message_entries(
    message: &SourceMessage,
    user_text: &str,
    device_id: &str,
) -> Vec<SessionMessageEntry> {
    let root = format!("{ENTRY_PREFIX}{}", message.id);
    let (role, parts) = match message.role {
        Role::User => (
            MessageRole::User,
            vec![MessagePart::Text {
                id: "t0".into(),
                text: user_text.to_string(),
            }],
        ),
        Role::Assistant => {
            let mut parts = Vec::new();
            for event in &message.events {
                fold_event_into_parts(&mut parts, event);
            }
            for part in &mut parts {
                if let MessagePart::Tool { resolved, .. } = part {
                    *resolved = true;
                }
            }
            (MessageRole::Assistant, parts)
        }
    };
    if parts.is_empty() {
        return Vec::new();
    }
    split_parts(&parts)
        .into_iter()
        .enumerate()
        .map(|(index, parts)| SessionMessageEntry {
            id: if index == 0 {
                root.clone()
            } else {
                continuation_id(&root, index)
            },
            role,
            parts,
            created_at: message.at,
            device_id: device_id.to_string(),
            status: Some(MessageStatus::Complete),
            continuation_of: (index > 0).then(|| root.clone()),
            duration_ms: None,
            turn_stats: None,
        })
        .collect()
}

/// The sidebar preview for a transcript: its last message's prose.
pub(crate) fn last_preview(messages: &[SourceMessage]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        let text = match m.role {
            Role::User => m.text.clone(),
            Role::Assistant => m
                .events
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
        };
        let text = text.trim();
        (!text.is_empty()).then(|| text.chars().take(120).collect())
    })
}

/// A title from a first prompt: its first non-empty line, capped.
pub(crate) fn title_from_prompt(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut title: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    Some(title)
}

pub(crate) fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Harness for a provider/driver name as T3 Code and the CLIs spell it.
pub(crate) fn harness_for_provider(name: &str) -> Option<HarnessId> {
    let name = name.trim();
    let exact = match name {
        "claudeAgent" | "claude" | "claudeCode" | "claude-code" => Some(HarnessId::ClaudeCode),
        "codex" => Some(HarnessId::Codex),
        "cursor" => Some(HarnessId::Cursor),
        "grok" => Some(HarnessId::Grok),
        "opencode" => Some(HarnessId::Opencode),
        "pi" => Some(HarnessId::Pi),
        "muse" => Some(HarnessId::Muse),
        "antigravity" => Some(HarnessId::Antigravity),
        "devin" => Some(HarnessId::Devin),
        "hermes" => Some(HarnessId::Hermes),
        _ => None,
    };
    exact.or_else(|| {
        // Custom provider instances are named after their driver
        // (`codex_work`, `claudeAgent-2`).
        let lower = name.to_ascii_lowercase();
        [
            ("claude", HarnessId::ClaudeCode),
            ("codex", HarnessId::Codex),
            ("cursor", HarnessId::Cursor),
            ("grok", HarnessId::Grok),
            ("opencode", HarnessId::Opencode),
        ]
        .into_iter()
        .find(|(prefix, _)| lower.starts_with(prefix))
        .map(|(_, harness)| harness)
    })
}

pub(crate) fn reasoning_level(value: &str) -> Option<ReasoningLevel> {
    serde_json::from_value(serde_json::Value::String(value.trim().to_ascii_lowercase())).ok()
}

/// The folder a session's space maps to: a Claude Code worktree
/// (`<repo>/.claude/worktrees/<name>`) belongs to its repository's project.
pub(crate) fn project_root_for(cwd: &str) -> String {
    match cwd.find("/.claude/worktrees/") {
        Some(at) if at > 0 => cwd[..at].to_string(),
        _ => cwd.trim_end_matches('/').to_string(),
    }
}

pub(crate) fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_groups_assistant_items_between_user_messages() {
        let mut b = TranscriptBuilder::default();
        b.user("u1".into(), 1, "hi".into(), Vec::new());
        b.text("a1", 2, "first");
        b.tool_call(
            "a1",
            3,
            "tool-1",
            ToolCall::Exec {
                command: "ls".into(),
            },
        );
        b.tool_result("tool-1", false);
        b.text("a2", 4, "second");
        b.user("u2".into(), 5, "again".into(), Vec::new());
        b.text("a3", 6, "done");
        let messages = b.finish();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].id, "a1");
        let entries = message_entries(&messages[1], "", "dev");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].parts.len(), 3, "text, tool, text");
        assert!(matches!(
            &entries[0].parts[1],
            MessagePart::Tool { resolved: true, .. }
        ));
        assert_eq!(entries[0].id, "import:a1");
    }

    #[test]
    fn worktrees_map_to_their_repository() {
        assert_eq!(project_root_for("/r/app/.claude/worktrees/x"), "/r/app");
        assert_eq!(project_root_for("/r/app/"), "/r/app");
        assert_eq!(harness_for_provider("codex_work"), Some(HarnessId::Codex));
        assert_eq!(reasoning_level("XHigh"), Some(ReasoningLevel::XHigh));
    }
}
