//! Always-on agents: device-scoped agent definitions the engine fires on a
//! schedule, a webhook or a file change. Every fire is a normal chat in the
//! agent's workspace; the run record (inbox row) links back to it.
//!
//! Wire shape for the `ListAgents` / `CreateAgent` / … RPCs. Not glob
//! re-exported from the crate root: `Agent*` names there already mean the
//! harness event stream.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{HarnessId, ReasoningLevel, SandboxLevel};

/// What makes an agent run. An agent may carry several; each keeps its own
/// schedule state (keyed by the trigger's content, so an edit that leaves a
/// trigger unchanged keeps its history).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AgentTrigger {
    /// Standard 5-field cron (`min hour dom month dow`), evaluated in an
    /// IANA time zone so "09:00" stays 09:00 across DST changes.
    #[serde(rename_all = "camelCase")]
    Cron { expr: String, timezone: String },
    /// Every `every_secs`, anchored at the agent's enable time.
    #[serde(rename_all = "camelCase")]
    Interval { every_secs: u64 },
    /// One shot at a fixed instant.
    #[serde(rename_all = "camelCase")]
    Once { at: DateTime<Utc> },
    /// `POST` to the agent's secret webhook URL. The JSON body renders into
    /// the instructions through `{{body.path}}` placeholders.
    Webhook,
    /// Changes under `paths` (workspace-relative; empty = the whole
    /// workspace), debounced so a burst of writes is one run.
    #[serde(rename_all = "camelCase")]
    FileWatch {
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        debounce_ms: Option<u64>,
    },
}

/// The editable part of an agent (create/update payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDraft {
    pub name: String,
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningLevel>,
    /// The space (device + folder) every run's chat is created in. Must be
    /// owned by the device that hosts the agent.
    pub space_id: String,
    /// Prompt template. Webhook runs substitute `{{body.*}}` / `{{headers.*}}`.
    pub instructions: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Same access levels as an interactive chat.
    #[serde(default = "default_sandbox")]
    pub sandbox: SandboxLevel,
    /// Same meaning as [`crate::RunRequest::auto_approve`]. Off by default:
    /// an unattended run that needs a decision waits in the inbox instead.
    #[serde(default)]
    pub auto_approve: bool,
    /// The agent's access mode. `None` (an agent saved before access modes)
    /// reads it off `sandbox` + `auto_approve`; saving writes all three in
    /// step (see [`Self::access_mode`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_mode: Option<crate::AccessMode>,
    #[serde(default)]
    pub triggers: Vec<AgentTrigger>,
}

impl AgentDraft {
    /// The access mode every run of this agent uses.
    pub fn access_mode(&self) -> crate::AccessMode {
        self.access_mode
            .unwrap_or_else(|| crate::AccessMode::from_legacy(self.sandbox, self.auto_approve))
    }

    /// Pin the access mode and keep the legacy pair in step with it, so an
    /// older engine reading the stored draft runs with no more access.
    pub fn normalize_access(&mut self) {
        let mode = self.access_mode();
        let (sandbox, auto_approve) = mode.legacy();
        self.access_mode = Some(mode);
        self.sandbox = sandbox;
        self.auto_approve = auto_approve;
    }
}

fn default_true() -> bool {
    true
}

fn default_sandbox() -> SandboxLevel {
    SandboxLevel::WorkspaceWrite
}

/// A stored agent plus its live scheduler state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub id: String,
    #[serde(flatten)]
    pub draft: AgentDraft,
    /// Folder of `space_id` when the agent was saved (display + fallback).
    pub workspace_path: String,
    /// `{data_dir}/agents/{id}/MEMORY.md` on the hosting device.
    pub memory_path: String,
    /// Path + secret token on the engine's local IPC port; `POST` to fire.
    pub webhook_path: String,
    /// `webhook_path` on `127.0.0.1` when this engine serves IPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    /// Set after repeated failures; scheduling stops until re-enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<DateTime<Utc>>,
    /// The agent's run currently queued, running or waiting on approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<AgentRun>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What started a run (inbox label + chat title suffix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentTriggerKind {
    Cron,
    Interval,
    Once,
    Webhook,
    FileWatch,
    Manual,
}

impl AgentTriggerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cron => "Schedule",
            Self::Interval => "Interval",
            Self::Once => "One-shot",
            Self::Webhook => "Webhook",
            Self::FileWatch => "File change",
            Self::Manual => "Run now",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRunStatus {
    Queued,
    Running,
    /// Parked on an approval or question. Never auto-denied: it stays here
    /// until someone answers in the chat.
    NeedsApproval,
    Succeeded,
    Failed,
    /// Not run: misfire grace exceeded, previous run still active, or the
    /// agent was paused. `summary` says which.
    Skipped,
    /// Stopped on the provider's usage limit. Not a failure: it never counts
    /// toward the auto-pause, and the agent's next fire waits for the reset.
    Limited,
}

impl AgentRunStatus {
    /// Holds the agent's single run slot (skip-if-running).
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::NeedsApproval)
    }
}

/// One inbox row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRun {
    pub id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub trigger: AgentTriggerKind,
    /// The fire time this run answers (equals `created_at` for event triggers).
    pub scheduled_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub status: AgentRunStatus,
    /// Excerpt of the final assistant message, or why it failed/was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Earlier fires folded into this row (missed backlog, repeated skips).
    #[serde(default)]
    pub collapsed: u32,
    pub read: bool,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
}

/// `ListAgentRuns` filters; all optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<AgentRunStatus>,
    #[serde(default)]
    pub unread_only: bool,
    #[serde(default)]
    pub include_archived: bool,
    /// Newest first; defaults to 100, capped at 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `WatchAgents` item: the whole (small) agent list plus inbox counters,
/// re-sent on every change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsSnapshot {
    pub agents: Vec<AgentSummary>,
    pub unread_runs: u32,
    pub needs_approval_runs: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_wire_shape_is_tagged_camel_case() {
        let cron = AgentTrigger::Cron {
            expr: "0 9 * * 1-5".into(),
            timezone: "Europe/Berlin".into(),
        };
        assert_eq!(
            serde_json::to_value(&cron).unwrap(),
            serde_json::json!({"kind": "cron", "expr": "0 9 * * 1-5", "timezone": "Europe/Berlin"})
        );
        let watch: AgentTrigger =
            serde_json::from_value(serde_json::json!({"kind": "fileWatch"})).unwrap();
        assert_eq!(
            watch,
            AgentTrigger::FileWatch {
                paths: vec![],
                debounce_ms: None
            }
        );
        assert_eq!(
            serde_json::to_value(AgentRunStatus::NeedsApproval).unwrap(),
            "needs-approval"
        );
    }

    #[test]
    fn draft_defaults_to_enabled_workspace_write_without_auto_approve() {
        let draft: AgentDraft = serde_json::from_value(serde_json::json!({
            "name": "Nightly", "harness": "mock", "spaceId": "s1", "instructions": "go",
        }))
        .unwrap();
        assert!(draft.enabled);
        assert!(!draft.auto_approve);
        assert_eq!(draft.sandbox, SandboxLevel::WorkspaceWrite);
        assert!(draft.triggers.is_empty());
        assert_eq!(draft.access_mode(), crate::AccessMode::AutoAcceptEdits);
    }

    #[test]
    fn agent_access_maps_onto_the_legacy_pair_both_ways() {
        let mut draft: AgentDraft = serde_json::from_value(serde_json::json!({
            "name": "Nightly", "harness": "mock", "spaceId": "s1", "instructions": "go",
            "sandbox": "danger-full-access", "autoApprove": true,
        }))
        .unwrap();
        assert_eq!(draft.access_mode(), crate::AccessMode::FullAccess);
        draft.access_mode = Some(crate::AccessMode::Supervised);
        draft.normalize_access();
        assert_eq!(draft.sandbox, SandboxLevel::ReadOnly);
        assert!(!draft.auto_approve);
        let json = serde_json::to_value(&draft).unwrap();
        assert_eq!(json["accessMode"], "supervised");
    }
}
