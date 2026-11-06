//! Turning what the composer holds (prompt, When, project and run config)
//! into the engine's [`AgentDraft`], and prefilling the composer from an
//! existing chat ("Run this on a schedule…").

use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zeron_proto::agents::AgentDraft;
use zeron_proto::{AccessMode, Chat, HarnessId, ReasoningLevel};

use super::naming::derive_name;
use super::schedule::ParsedSchedule;

/// Where the zoomie would run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectRef<'a> {
    pub space_id: &'a str,
    pub device_id: &'a str,
}

/// Everything the composer contributes to a new zoomie.
#[derive(Debug, Clone)]
pub struct ComposerZoomie<'a> {
    pub prompt: &'a str,
    /// The user's own name, when they typed one.
    pub name: Option<&'a str>,
    pub schedule: Result<&'a ParsedSchedule, &'a str>,
    pub project: Option<ProjectRef<'a>>,
    /// The device whose engine hosts zoomies (this one).
    pub local_device_id: Option<&'a str>,
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub access_mode: AccessMode,
}

/// The draft to save, or the inline hint that blocks Send.
pub fn assemble_draft(input: &ComposerZoomie<'_>) -> Result<AgentDraft, String> {
    let instructions = input.prompt.trim();
    if instructions.is_empty() {
        return Err("Tell the zoomie what to do.".into());
    }
    let schedule = input.schedule.map_err(str::to_string)?;
    let project = input
        .project
        .ok_or_else(|| "Zoomies run inside a project. Pick one above.".to_string())?;
    if input.local_device_id != Some(project.device_id) {
        return Err("Zoomies run on this device. Pick a project on it.".into());
    }
    let harness = input
        .harness
        .filter(|harness| *harness != HarnessId::Mock)
        .ok_or_else(|| "Pick an agent for the zoomie.".to_string())?;
    let name = input
        .name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| derive_name(instructions));
    let (sandbox, auto_approve) = input.access_mode.legacy();
    let mut draft = AgentDraft {
        name,
        harness,
        model: input.model.clone(),
        reasoning: input.reasoning,
        space_id: project.space_id.to_string(),
        instructions: instructions.to_string(),
        enabled: true,
        sandbox,
        auto_approve,
        access_mode: Some(input.access_mode),
        triggers: vec![schedule.trigger.clone()],
    };
    draft.normalize_access();
    Ok(draft)
}

/// What "Run this on a schedule…" puts into the composer.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoomiePrefill {
    pub prompt: String,
    pub space_id: Option<String>,
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub access_mode: Option<AccessMode>,
}

/// Prefill from a chat and as much of its transcript as is at hand. With
/// no user message loaded, the chat's title stands in for the prompt.
pub fn prefill_from_chat(chat: &Chat, transcript: &[SessionMessageEntry]) -> ZoomiePrefill {
    let prompt = first_user_prompt(transcript)
        .or_else(|| {
            chat.title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let config = chat.config.as_ref();
    ZoomiePrefill {
        prompt,
        space_id: chat.space_id.clone(),
        harness: config.map(|c| c.harness),
        model: config.and_then(|c| c.model.clone()),
        reasoning: config.and_then(|c| c.reasoning),
        access_mode: config.map(|c| c.effective_access_mode()),
    }
}

/// The text of the first user message: its text parts joined, trimmed.
pub fn first_user_prompt(transcript: &[SessionMessageEntry]) -> Option<String> {
    transcript
        .iter()
        .filter(|entry| entry.role == MessageRole::User)
        .map(|entry| {
            entry
                .parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .find(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoomies::schedule::parse_schedule;
    use chrono::{TimeZone as _, Utc};
    use zeron_proto::SandboxLevel;
    use zeron_proto::agents::AgentTrigger;

    fn schedule(text: &str) -> ParsedSchedule {
        let now = Utc.with_ymd_and_hms(2026, 10, 28, 4, 30, 0).unwrap();
        parse_schedule(text, "Asia/Kolkata", now).unwrap()
    }

    fn input<'a>(prompt: &'a str, parsed: &'a ParsedSchedule) -> ComposerZoomie<'a> {
        ComposerZoomie {
            prompt,
            name: None,
            schedule: Ok(parsed),
            project: Some(ProjectRef {
                space_id: "space-1",
                device_id: "this-mac",
            }),
            local_device_id: Some("this-mac"),
            harness: Some(HarnessId::ClaudeCode),
            model: Some("claude-opus-5-5".into()),
            reasoning: Some(ReasoningLevel::High),
            access_mode: AccessMode::FullAccess,
        }
    }

    #[test]
    fn assembles_the_full_draft() {
        let parsed = schedule("weekdays at 6pm");
        let draft = assemble_draft(&input(
            "  Summarize today's merged PRs and post them to #eng  ",
            &parsed,
        ))
        .unwrap();
        assert_eq!(draft.name, "Summarize today's merged PRs");
        assert_eq!(
            draft.instructions,
            "Summarize today's merged PRs and post them to #eng"
        );
        assert_eq!(draft.space_id, "space-1");
        assert_eq!(draft.harness, HarnessId::ClaudeCode);
        assert_eq!(draft.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(draft.reasoning, Some(ReasoningLevel::High));
        assert!(draft.enabled);
        assert_eq!(draft.access_mode, Some(AccessMode::FullAccess));
        // The legacy pair follows the access mode, so an older engine runs
        // the zoomie with no more access.
        assert_eq!(draft.sandbox, SandboxLevel::DangerFullAccess);
        assert!(draft.auto_approve);
        assert_eq!(
            draft.triggers,
            vec![AgentTrigger::Cron {
                expr: "0 18 * * 1-5".into(),
                timezone: "Asia/Kolkata".into()
            }]
        );
        // The wire shape the engine reads.
        let json = serde_json::to_value(&draft).unwrap();
        assert_eq!(json["spaceId"], "space-1");
        assert_eq!(json["accessMode"], "full-access");
    }

    #[test]
    fn a_typed_name_wins_over_the_derived_one() {
        let parsed = schedule("every hour");
        let mut zoomie = input("check CI", &parsed);
        zoomie.name = Some("  CI watch ");
        assert_eq!(assemble_draft(&zoomie).unwrap().name, "CI watch");
        zoomie.name = Some("   ");
        assert_eq!(assemble_draft(&zoomie).unwrap().name, "Check CI");
    }

    #[test]
    fn supervised_keeps_the_sandbox_read_only() {
        let parsed = schedule("when files change");
        let mut zoomie = input("lint", &parsed);
        zoomie.access_mode = AccessMode::Supervised;
        let draft = assemble_draft(&zoomie).unwrap();
        assert_eq!(draft.sandbox, SandboxLevel::ReadOnly);
        assert!(!draft.auto_approve);
        assert_eq!(
            draft.triggers,
            vec![AgentTrigger::FileWatch {
                paths: vec![],
                debounce_ms: None
            }]
        );
    }

    #[test]
    fn blocks_send_with_a_reason() {
        let parsed = schedule("every hour");
        let mut zoomie = input("   ", &parsed);
        assert_eq!(
            assemble_draft(&zoomie).unwrap_err(),
            "Tell the zoomie what to do."
        );
        zoomie.prompt = "check CI";
        zoomie.schedule = Err("Couldn't read that.");
        assert_eq!(assemble_draft(&zoomie).unwrap_err(), "Couldn't read that.");
        zoomie.schedule = Ok(&parsed);
        zoomie.project = None;
        assert!(assemble_draft(&zoomie).unwrap_err().contains("project"));
        zoomie.project = Some(ProjectRef {
            space_id: "space-2",
            device_id: "the-vps",
        });
        assert!(assemble_draft(&zoomie).unwrap_err().contains("this device"));
        zoomie.project = Some(ProjectRef {
            space_id: "space-1",
            device_id: "this-mac",
        });
        zoomie.harness = None;
        assert!(assemble_draft(&zoomie).unwrap_err().contains("agent"));
        zoomie.harness = Some(HarnessId::Mock);
        assert!(assemble_draft(&zoomie).unwrap_err().contains("agent"));
        zoomie.harness = Some(HarnessId::Codex);
        zoomie.local_device_id = None;
        assert!(assemble_draft(&zoomie).unwrap_err().contains("this device"));
    }

    fn entry(id: &str, role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "role": role, "parts": parts, "createdAt": 0, "deviceId": "d",
        }))
        .unwrap()
    }

    fn text(t: &str) -> MessagePart {
        MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }
    }

    fn chat() -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": "c1",
            "deviceId": "this-mac",
            "title": "Nightly report",
            "archived": false,
            "cwd": "/repo",
            "branch": null,
            "checkoutId": null,
            "config": {
                "harness": "codex",
                "model": "gpt-5.4",
                "reasoning": "medium",
                "sandbox": "read-only",
            },
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": "2026-10-01T00:00:00Z",
            "spaceId": "space-9",
        }))
        .unwrap()
    }

    #[test]
    fn convert_prefills_from_the_first_user_prompt() {
        let transcript = vec![
            entry("a0", MessageRole::Assistant, vec![text("hello")]),
            entry("u0", MessageRole::User, vec![text("   ")]),
            entry(
                "u1",
                MessageRole::User,
                vec![
                    text("Check the nightly build"),
                    text("and report flaky tests"),
                ],
            ),
            entry("u2", MessageRole::User, vec![text("thanks")]),
        ];
        let prefill = prefill_from_chat(&chat(), &transcript);
        assert_eq!(
            prefill,
            ZoomiePrefill {
                prompt: "Check the nightly build\nand report flaky tests".into(),
                space_id: Some("space-9".into()),
                harness: Some(HarnessId::Codex),
                model: Some("gpt-5.4".into()),
                reasoning: Some(ReasoningLevel::Medium),
                // A config from before access modes reads its sandbox.
                access_mode: Some(AccessMode::Supervised),
            }
        );
    }

    #[test]
    fn convert_falls_back_to_the_title_and_tolerates_missing_config() {
        let mut chat = chat();
        chat.config = None;
        let prefill = prefill_from_chat(&chat, &[]);
        assert_eq!(prefill.prompt, "Nightly report");
        assert_eq!(prefill.harness, None);
        assert_eq!(prefill.access_mode, None);
        chat.title = None;
        assert_eq!(prefill_from_chat(&chat, &[]).prompt, "");
    }
}
