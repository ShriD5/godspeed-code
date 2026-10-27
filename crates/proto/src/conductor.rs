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

/// Standing orders for the main agent. Drafted at creation from the purpose
/// and edited later. They ride the synced row next to the notes, so a full
/// notes field and a full instructions field together may not fit the 16 KiB
/// per-change budget; the registry refuses a write that would not.
pub const MAX_INSTRUCTIONS_BYTES: usize = 6 * 1024;

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
    /// Standing orders for the main agent. Drafted when the conductor is
    /// created and edited from the chat. Empty on rows from before this
    /// field existed; those keep the generated template.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
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

fn live_context(conductor: &Conductor, repos: &[ConductorRepo]) -> String {
    format!(
        "## Live context\nFolder: {folder}\n\nRepos:\n{repos}\n\n## Notes\n<notes>\n{notes}\n</notes>\n\n",
        folder = conductor.folder,
        repos = repo_lines(repos),
        notes = notes_block(&conductor.notes),
    )
}

/// Standing orders drafted from the job the user named at creation. Repos and
/// notes are not baked in as the only copy: they are appended as live context
/// on every fresh session, so editing the orders does not freeze an old list.
pub fn draft_conductor_instructions(
    name: &str,
    purpose: Option<&str>,
    folder: &str,
    repos: &[ConductorRepo],
) -> String {
    let job = purpose
        .map(str::trim)
        .filter(|purpose| !purpose.is_empty())
        .map(|purpose| {
            format!(
                "The user said this conductor is responsible for:\n\n{purpose}\n\n\
Treat that as the job. Scope every sub-agent to it. If a request sits outside it, \
say so and ask before starting work."
            )
        })
        .unwrap_or_else(|| {
            "The user has not named a responsibility yet. Before starting work, ask what this \
conductor should own — for example, reviewing pull requests on one repo, or carrying one \
feature. Remember their answer in the notes, and work from that until they edit these \
instructions."
                .into()
        });
    let repo_hint = if repos.is_empty() {
        "No git repos were in the folder when this conductor was created. The live context \
after this prompt lists whatever is there now."
            .to_string()
    } else {
        // Names are a hint, not the source of truth, and the stored orders have
        // to fit the row budget even when the folder holds a lot of repos.
        const SHOWN: usize = 8;
        let names = repos
            .iter()
            .take(SHOWN)
            .map(|repo| clip(&repo.name, 48))
            .collect::<Vec<_>>()
            .join(", ");
        let extra = repos.len().saturating_sub(SHOWN);
        let more = if extra == 0 {
            String::new()
        } else {
            format!(", and {extra} more")
        };
        format!(
            "Repos found at creation: {names}{more}. The live context after this prompt is \
authoritative if that list has changed."
        )
    };
    let folder = clip(folder, 240);
    let text = format!(
        "You are the main agent of the \"{name}\" conductor in Godspeed.\n\n\
A conductor is one long-lived chat bound to a folder. The user talks only to you. You \
coordinate the work: you start, reuse and steer sub-agents in the repos, then report back. \
This chat should read like a lead reporting, not a log of tool output.\n\n\
## Responsibility\n\
{job}\n\n\
Folder: {folder}\n\
{repo_hint}\n\n\
## How you work\n\
- Start a sub-agent in the right repo with a short title and a clear goal (Godspeed MCP \
`create_chat`, with that repo as `project`). Say where you sent the work when you start it.\n\
- Before starting one, call `conductor_info` and reuse a sub-agent that already owns the work \
(match on title and goal) by sending it a message instead.\n\
- Read but delegate changes. Answer status questions yourself from the notes and your \
sub-agents' chats. Anything that edits code, runs builds, or touches a remote (pull request \
comments, reviews, releases) goes to a sub-agent.\n\
- Sub-agents may start their own sub-agents, up to {depth} levels below you.\n\
- When a sub-agent finishes, report briefly: the verdict, the one or two things that matter, \
and what the user can do next. No logs.\n\
- You may approve a sub-agent's low-risk approvals. Everything else, and every question, \
waits for the user.\n\
- Keep the notes current with `conductor_notes_write` when you learn how the user works.\n\
- Access never widens going down. A sub-agent gets at most the access you have.\n\n\
## Best practices\n\
- One owner per piece of work. Do not start a second sub-agent for something an existing one \
already has.\n\
- Write each goal as if the sub-agent has no other context: which repo, the outcome, the \
constraints, and how to know it is done.\n\
- Prefer continuing a sub-agent that already knows the area over a fresh one.\n\
- Keep this chat short. A finished sub-agent is a verdict, not a paste of its transcript.\n\
- If the job is reviewing pull requests: find the open ones in the repo the user named, send \
each review to a sub-agent, and bring back a verdict (approve, comment, or request changes) \
with the one or two findings that matter. Do not review by narrating the diff yourself.\n\
- If you are unsure which repo a task belongs in, ask once, then remember the answer in the \
notes.\n\
- When these instructions change, follow the new text from that moment. The live context \
appended after this prompt (current repos and notes) wins if it disagrees with an older \
detail here.\n\n\
These are your standing orders for every session of this conductor.\n",
        depth = MAX_CONDUCTOR_DEPTH,
    );
    // A pathological path or purpose must not make creation fail the cap.
    // The best-practice section sits before this cut.
    if text.len() <= MAX_INSTRUCTIONS_BYTES {
        text
    } else {
        clip(&text, MAX_INSTRUCTIONS_BYTES)
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end().to_string()
}

/// What the main agent is told when its standing orders change mid-session.
/// A fresh session already leads with the new orders; this reaches the one
/// already running.
pub fn instructions_changed_notice(instructions: &str) -> String {
    format!(
        "The conductor's instructions were edited. These are now your standing orders. Follow \
them from here on. Do not start new work because of this notice: acknowledge in one line and \
wait for the user.\n\n<instructions>\n{instructions}\n</instructions>\n"
    )
}

/// A short slice of the standing orders for a sub-agent, so it knows the job
/// without carrying the whole prompt.
fn standing_brief(conductor: &Conductor) -> String {
    let standing = conductor.instructions.trim();
    if !standing.is_empty() {
        let mut end = standing.len().min(400);
        while end > 0 && !standing.is_char_boundary(end) {
            end -= 1;
        }
        return format!(
            "The conductor's standing orders begin:\n{}\n\n",
            standing[..end].trim_end()
        );
    }
    conductor
        .purpose
        .as_deref()
        .map(str::trim)
        .filter(|purpose| !purpose.is_empty())
        .map(|purpose| format!("This conductor is responsible for: {purpose}\n\n"))
        .unwrap_or_default()
}

/// Instructions put in front of the main chat's first prompt of a fresh
/// harness session. Stored standing orders win; conductors from before that
/// field keep the generated template.
pub fn main_agent_instructions(conductor: &Conductor, repos: &[ConductorRepo]) -> String {
    let standing = conductor.instructions.trim();
    if !standing.is_empty() {
        return format!("{standing}\n\n{}", live_context(conductor, repos));
    }
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
{nesting}\n\n{brief}## Notes\n<notes>\n{notes}\n</notes>\n\n",
        name = conductor.name,
        brief = standing_brief(conductor),
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
            instructions: String::new(),
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
    fn stored_instructions_lead_and_live_context_still_follows() {
        let repos = vec![ConductorRepo {
            space_id: "s1".into(),
            path: "/code/acme/api".into(),
            name: "api".into(),
        }];
        let mut conductor = conductor();
        conductor.instructions = "Review every pull request on api before merge.".into();
        let text = main_agent_instructions(&conductor, &repos);
        assert!(text.starts_with("Review every pull request"));
        assert!(text.contains("/code/acme/api"));
        assert!(text.contains("Reviews: be blunt."));
        assert!(text.contains("Live context"));
    }

    #[test]
    fn a_draft_carries_the_job_and_review_practice_and_fits() {
        let repos = vec![ConductorRepo {
            space_id: "s1".into(),
            path: "/code/acme/api".into(),
            name: "api".into(),
        }];
        let text = draft_conductor_instructions(
            "Acme",
            Some("Review pull requests on the api repo"),
            "/code/acme",
            &repos,
        );
        assert!(text.contains("Review pull requests on the api repo"));
        assert!(text.contains("api"));
        assert!(text.contains("Best practices"));
        assert!(text.contains("request changes"));
        assert!(text.contains("standing orders"));
        assert!(text.len() <= MAX_INSTRUCTIONS_BYTES);
        let unnamed = draft_conductor_instructions("Acme", None, "/code/acme", &[]);
        assert!(unnamed.contains("has not named a responsibility"));
        assert!(unnamed.len() <= MAX_INSTRUCTIONS_BYTES);
        let crowded: Vec<ConductorRepo> = (0..40)
            .map(|i| ConductorRepo {
                space_id: format!("s{i}"),
                path: format!("/code/{}", "n".repeat(80)),
                name: "n".repeat(80),
            })
            .collect();
        let huge = draft_conductor_instructions(
            "Acme",
            Some(&"Review pull requests. ".repeat(40)),
            &format!("/code/{}", "p".repeat(400)),
            &crowded,
        );
        assert!(huge.len() <= MAX_INSTRUCTIONS_BYTES, "{}", huge.len());
        assert!(huge.contains("Best practices"), "{huge}");
        assert!(huge.contains("and 32 more"), "{huge}");
    }

    #[test]
    fn an_edit_notice_quotes_the_new_orders_and_asks_the_agent_to_wait() {
        let notice = instructions_changed_notice("Review only billing.");
        assert!(notice.contains("were edited"));
        assert!(notice.contains("Review only billing."));
        assert!(notice.contains("wait for the user"));
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
