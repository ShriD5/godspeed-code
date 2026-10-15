//! Chat import wire types — bringing conversations from other agent apps
//! (T3 Code, plain Claude Code, plain Codex, the Cursor Agent CLI) into this
//! workspace.
//!
//! The engine scans each source read-only, the UI previews it per project,
//! and an import streams [`ChatImportProgress`] items ending in a
//! [`ChatImportSummary`]. Every imported chat remembers its origin as a
//! source key (`t3:<thread>`, `claude:<session>`, `codex:<thread>`,
//! `cursor:<chat>`) so a
//! re-run updates or skips instead of duplicating.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// Where chats are imported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatImportSource {
    /// T3 Code's projection database (`~/.t3/userdata/state.sqlite`).
    T3,
    /// Claude Code transcripts (`~/.claude/projects/<dir>/<session>.jsonl`).
    Claude,
    /// Codex rollouts (`~/.codex/sessions/YYYY/MM/DD/*.jsonl`).
    Codex,
    /// Cursor Agent CLI chat stores (`~/.cursor/chats/<workspace>/<chat>/store.db`).
    Cursor,
}

impl ChatImportSource {
    pub const ALL: [ChatImportSource; 4] = [Self::T3, Self::Claude, Self::Codex, Self::Cursor];

    pub fn label(self) -> &'static str {
        match self {
            Self::T3 => "T3 Code",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
        }
    }

    /// The CLI spelling (`godspeed import <slug>`).
    pub fn slug(self) -> &'static str {
        match self {
            Self::T3 => "t3",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.slug() == slug)
    }

    /// The prefix of the source keys this source mints.
    pub fn key_prefix(self) -> &'static str {
        self.slug()
    }
}

/// What happens to one source chat when the import runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatImportState {
    /// Not in this workspace yet.
    New,
    /// Imported before; the source has messages the import will append.
    Update,
    /// Imported before and unchanged (or continued here since).
    Imported,
    /// Already present through another route: a T3 thread that owns this
    /// CLI session, or a chat in this workspace already resuming it.
    Elsewhere,
}

/// One importable conversation, as the preview lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportPreviewChat {
    /// `t3:<thread_id>`, `claude:<session_id>`, `codex:<thread_id>`.
    pub source_key: String,
    pub title: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    /// The harness the chat continues on, when this app has it.
    pub harness: Option<HarnessId>,
    /// Whether sending a message resumes the original agent session.
    pub resumable: bool,
    pub state: ChatImportState,
}

/// One source project (a workspace folder) and its chats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportPreviewProject {
    /// The folder the project's space maps to.
    pub path: String,
    pub title: String,
    pub chats: Vec<ChatImportPreviewChat>,
    /// A space for this folder already exists on this device.
    pub has_space: bool,
}

impl ChatImportPreviewProject {
    pub fn count(&self, state: ChatImportState) -> usize {
        self.chats.iter().filter(|c| c.state == state).count()
    }

    /// Earliest and latest activity across the project's chats.
    pub fn date_range(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let first = self.chats.iter().filter_map(|c| c.created_at).min()?;
        let last = self
            .chats
            .iter()
            .filter_map(|c| c.updated_at.or(c.created_at))
            .max()?;
        Some((first, last))
    }
}

/// A scanned source. `found == false` means its files are absent on this
/// device (the page still lists it, greyed out).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportPreview {
    pub source: ChatImportSource,
    pub found: bool,
    /// The directory or database that was read.
    pub location: String,
    pub projects: Vec<ChatImportPreviewProject>,
    /// Non-fatal scan problems (an unreadable file, a malformed row).
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl ChatImportPreview {
    pub fn chat_count(&self) -> usize {
        self.projects.iter().map(|p| p.chats.len()).sum()
    }

    pub fn count(&self, state: ChatImportState) -> usize {
        self.projects.iter().map(|p| p.count(state)).sum()
    }
}

/// `RunChatImport` params: which source and which of its projects. `None`
/// imports every project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportRequest {
    pub source: ChatImportSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<String>>,
}

/// Totals for a finished (or dry) run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportSummary {
    pub imported: usize,
    pub updated: usize,
    pub skipped: usize,
    pub spaces_created: usize,
    pub messages: usize,
    pub attachments: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// One `RunChatImport` stream item; the last is always `Summary`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ChatImportProgress {
    Start {
        total: usize,
    },
    Chat {
        index: usize,
        total: usize,
        title: String,
    },
    Summary {
        summary: ChatImportSummary,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_serializes_camel_case() {
        let item = serde_json::to_value(ChatImportProgress::Summary {
            summary: ChatImportSummary {
                spaces_created: 1,
                ..Default::default()
            },
        })
        .unwrap();
        assert_eq!(item["kind"], "summary");
        assert_eq!(item["summary"]["spacesCreated"], 1);
        let source = serde_json::to_value(ChatImportSource::T3).unwrap();
        assert_eq!(source, "t3");
        assert_eq!(
            ChatImportSource::from_slug("codex"),
            Some(ChatImportSource::Codex)
        );
    }
}
