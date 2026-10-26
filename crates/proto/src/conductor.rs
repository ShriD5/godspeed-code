//! Conductor: one long-lived main chat bound to a folder, whose agent starts,
//! reuses and steers nested sub-agent chats. See
//! `docs/design/2026-10-23-conductor-design.md`.
//!
//! A conductor is a synced registry row. Chats carry no conductor field:
//! membership is derived by walking `Chat::parent_chat_id` up to a conductor's
//! main chat (`depth` 0), so chats outside a conductor are untouched.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ChatConfig;

/// What the UI calls this feature. Everything stored or sent uses `conductor`,
/// so renaming it is a copy change only.
pub const DISPLAY_NAME: &str = "Conductor";

/// Levels of sub-agents allowed below the main chat (the main chat is depth 0).
pub const MAX_CONDUCTOR_DEPTH: u8 = 3;

/// Most sub-agents one conductor runs at once (working, waiting on input, or
/// just started). Settled, archived and finished ones don't count.
pub const MAX_RUNNING_SUB_AGENTS: usize = 8;

/// Notes beyond this are refused on write and cut from the prompt. Notes
/// ride the synced conductor row, and every registry change must stay under
/// the room's 16 KiB per-change budget, so this leaves room for the rest of
/// the row.
pub const MAX_NOTES_BYTES: usize = 12 * 1024;

/// Longest conductor name, so the synced row stays inside the per-change
/// budget next to full notes.
pub const MAX_NAME_BYTES: usize = 120;

/// Longest conductor purpose line, for the same reason.
pub const MAX_PURPOSE_BYTES: usize = 500;

/// Why `name` / `purpose` can't go on a conductor row, if either is too long.
pub fn check_conductor_text(name: Option<&str>, purpose: Option<&str>) -> Result<(), String> {
    if name.is_some_and(|n| n.len() > MAX_NAME_BYTES) {
        return Err(format!(
            "A conductor name is at most {MAX_NAME_BYTES} bytes"
        ));
    }
    if purpose.is_some_and(|p| p.len() > MAX_PURPOSE_BYTES) {
        return Err(format!(
            "A conductor purpose is at most {MAX_PURPOSE_BYTES} bytes"
        ));
    }
    Ok(())
}

/// A chat at `depth` may start children only while the children stay within
/// [`MAX_CONDUCTOR_DEPTH`].
pub fn can_spawn_at(depth: u8) -> bool {
    depth < MAX_CONDUCTOR_DEPTH
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conductor {
    pub id: String,
    pub name: String,
    /// One line on what this conductor is for; goes into the main agent's
    /// instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// Absolute folder on the owning device.
    pub folder: String,
    pub device_id: String,
    /// The conductor's one long-lived chat.
    pub main_chat_id: String,
    /// Spaces for the repos found in the folder.
    #[serde(default)]
    pub space_ids: Vec<String>,
    /// Main agent's harness, model and access mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ChatConfig>,
    /// Project notes the user and the main agent both edit.
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub archived: bool,
    pub created_at: DateTime<Utc>,
}

/// Where a chat sits inside a conductor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorMembership {
    pub conductor_id: String,
    /// 0 = the main chat, 1 = a sub-agent the main agent started, …
    pub depth: u8,
}

/// One chat in a conductor's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorNode {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_chat_id: Option<String>,
    pub depth: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

/// A repo the conductor covers, for prompts and tool output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorRepo {
    pub space_id: String,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateConductorParams {
    /// Client-minted id of the main chat, so retries are idempotent.
    pub chat_id: String,
    /// Client-minted conductor id; defaults to a fresh one when empty.
    #[serde(default)]
    pub id: String,
    pub folder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// Defaults to the engine's own device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ChatConfig>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConductorParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `Some("")` clears the purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ChatConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

/// Cut `notes` to the prompt budget on a char boundary.
pub fn clamp_notes(notes: &str) -> String {
    if notes.len() <= MAX_NOTES_BYTES {
        return notes.to_string();
    }
    let mut end = MAX_NOTES_BYTES;
    while !notes.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(notes truncated)", &notes[..end])
}

fn notes_block(notes: &str) -> String {
    let notes = notes.trim();
    if notes.is_empty() {
        "(empty so far)".into()
    } else {
        clamp_notes(notes)
    }
}

fn repo_lines(repos: &[ConductorRepo]) -> String {
    if repos.is_empty() {
        return "(no git repos found in the folder yet)".into();
    }
    repos
        .iter()
        .map(|r| format!("- {} — {}", r.name, r.path))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Instructions put in front of the main chat's first prompt of a fresh
/// harness session.
pub fn main_agent_instructions(conductor: &Conductor, repos: &[ConductorRepo]) -> String {
    let purpose = conductor
        .purpose
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("Purpose: {p}\n"))
        .unwrap_or_default();
    format!(
        "You are the main agent of the \"{name}\" conductor in Godspeed: one long-lived chat \
that coordinates work across the repos below. The user talks only to you; you send the work to \
sub-agents and report back.\n\n\
{purpose}Folder: {folder}\n\nRepos:\n{repos}\n\n\
## How to work\n\
- Start a sub-agent in the right repo with a short title and a clear goal (use the Godspeed \
MCP `create_chat` with that repo as `project`). Say where you sent the work when you start it.\n\
- Before starting one, call `conductor_info` and reuse a sub-agent that already owns the work \
(match on title and goal) by sending it a message instead.\n\
- Read but delegate changes: answer status questions yourself from the notes and your \
sub-agents' chats. Anything that edits code, runs builds or touches a remote goes to a sub-agent.\n\
- Sub-agents may start their own sub-agents, up to {depth} levels below you.\n\
- When a sub-agent finishes, report briefly: the verdict, the one or two things that matter, \
and what the user can do next. No logs.\n\
- You may approve a sub-agent's low-risk approvals; everything else, and every question, \
waits for the user.\n\
- Keep the notes current with `conductor_notes_write` when you learn how the user works.\n\n\
## Notes\n<notes>\n{notes}\n</notes>\n\n",
        name = conductor.name,
        folder = conductor.folder,
        repos = repo_lines(repos),
        depth = MAX_CONDUCTOR_DEPTH,
        notes = notes_block(&conductor.notes),
    )
}

/// Instructions for a sub-agent at `depth` (1 or deeper).
pub fn sub_agent_instructions(conductor: &Conductor, depth: u8) -> String {
    let nesting = if can_spawn_at(depth) {
        format!(
            "You may start your own sub-agents ({} more level(s) allowed).",
            MAX_CONDUCTOR_DEPTH - depth
        )
    } else {
        "You are at the deepest level and cannot start sub-agents.".into()
    };
    format!(
        "You are a sub-agent of the \"{name}\" conductor, started by another agent to do one \
piece of work. Stay on the goal you were given and finish with a short summary of the outcome. \
{nesting}\n\n## Notes\n<notes>\n{notes}\n</notes>\n\n",
        name = conductor.name,
        notes = notes_block(&conductor.notes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conductor() -> Conductor {
        Conductor {
            id: "c1".into(),
            name: "Acme".into(),
            purpose: Some("Ship billing".into()),
            folder: "/code/acme".into(),
            device_id: "d1".into(),
            main_chat_id: "chat-main".into(),
            space_ids: vec![],
            config: None,
            notes: "Reviews: be blunt.".into(),
            archived: false,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn spawning_stops_at_the_depth_cap() {
        assert!(can_spawn_at(0));
        assert!(can_spawn_at(MAX_CONDUCTOR_DEPTH - 1));
        assert!(!can_spawn_at(MAX_CONDUCTOR_DEPTH));
    }

    #[test]
    fn main_instructions_carry_purpose_repos_and_notes() {
        let repos = vec![ConductorRepo {
            space_id: "s1".into(),
            path: "/code/acme/api".into(),
            name: "api".into(),
        }];
        let text = main_agent_instructions(&conductor(), &repos);
        assert!(text.contains("Ship billing"));
        assert!(text.contains("/code/acme/api"));
        assert!(text.contains("Reviews: be blunt."));
        assert!(text.contains("conductor_info"));
    }

    #[test]
    fn main_instructions_say_so_when_no_repos_or_notes() {
        let mut c = conductor();
        c.notes.clear();
        let text = main_agent_instructions(&c, &[]);
        assert!(text.contains("no git repos found"));
        assert!(text.contains("empty so far"));
    }

    #[test]
    fn sub_agent_at_the_cap_is_told_it_cannot_nest() {
        let deepest = sub_agent_instructions(&conductor(), MAX_CONDUCTOR_DEPTH);
        assert!(deepest.contains("cannot start sub-agents"));
        let shallow = sub_agent_instructions(&conductor(), 1);
        assert!(shallow.contains("2 more level(s)"));
    }

    #[test]
    fn oversized_notes_are_truncated_on_a_char_boundary() {
        let notes = "é".repeat(MAX_NOTES_BYTES);
        let clamped = clamp_notes(&notes);
        assert!(clamped.ends_with("(notes truncated)"));
        assert!(clamped.len() < notes.len());
        assert_eq!(clamp_notes("short"), "short");
    }

    #[test]
    fn conductor_round_trips_camel_case() {
        let original = conductor();
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(json["mainChatId"], "chat-main");
        let back: Conductor = serde_json::from_value(json).unwrap();
        assert_eq!(back, original);
    }
}
