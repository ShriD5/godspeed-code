//! The sidebar's Zoomies rows as plain data: name, when it runs next, how
//! the last run went, and whether a run is working right now.

use chrono::{DateTime, Utc};
use zeron_proto::agents::{AgentRunStatus, AgentSummary, AgentTrigger, AgentsSnapshot};

/// The status dot beside a zoomie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    /// Never ran.
    Fresh,
    /// A run is queued or working (the mascot jogs).
    Running,
    /// A run waits on the user (approval or question).
    NeedsYou,
    Succeeded,
    Failed,
    /// Stopped on a usage limit, or skipped.
    Held,
    /// Auto-paused after repeated failures.
    Paused,
}

impl RowStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fresh => "Hasn't run yet",
            Self::Running => "Running",
            Self::NeedsYou => "Needs you",
            Self::Succeeded => "Last run finished",
            Self::Failed => "Last run failed",
            Self::Held => "Last run was held",
            Self::Paused => "Paused after failures",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoomieRow {
    pub id: String,
    pub name: String,
    /// "in 2h", "running", "on file change", "off" …
    pub detail: String,
    pub status: RowStatus,
    /// The switch: on and not auto-paused.
    pub enabled: bool,
    /// The chat of the run in flight, else of the last run.
    pub chat_id: Option<String>,
}

impl ZoomieRow {
    pub fn running(&self) -> bool {
        self.status == RowStatus::Running
    }
}

/// Rows in the engine's order (creation order).
pub fn rows(snapshot: &AgentsSnapshot, now: DateTime<Utc>) -> Vec<ZoomieRow> {
    snapshot
        .agents
        .iter()
        .map(|agent| row(agent, now))
        .collect()
}

pub fn row(agent: &AgentSummary, now: DateTime<Utc>) -> ZoomieRow {
    let last = agent.last_run.as_ref();
    let status = if agent.paused_reason.is_some() {
        RowStatus::Paused
    } else {
        match last.map(|run| run.status) {
            None => RowStatus::Fresh,
            Some(AgentRunStatus::Queued | AgentRunStatus::Running) => RowStatus::Running,
            Some(AgentRunStatus::NeedsApproval) => RowStatus::NeedsYou,
            Some(AgentRunStatus::Succeeded) => RowStatus::Succeeded,
            Some(AgentRunStatus::Failed) => RowStatus::Failed,
            Some(AgentRunStatus::Skipped | AgentRunStatus::Limited) => RowStatus::Held,
        }
    };
    // The active run outranks whatever the last finished one said.
    let status = if agent.active_run_id.is_some() && status != RowStatus::NeedsYou {
        RowStatus::Running
    } else {
        status
    };
    let enabled = agent.draft.enabled && agent.paused_reason.is_none();
    let detail = if status == RowStatus::Running {
        "running".to_string()
    } else if status == RowStatus::NeedsYou {
        "needs you".to_string()
    } else if agent.paused_reason.is_some() {
        "paused".to_string()
    } else if !agent.draft.enabled {
        "off".to_string()
    } else if let Some(next) = agent.next_fire_at {
        relative_next(next, now)
    } else if let Some(event) = event_label(&agent.draft.triggers) {
        event.to_string()
    } else {
        "done".to_string()
    };
    ZoomieRow {
        id: agent.id.clone(),
        name: agent.draft.name.clone(),
        detail,
        status,
        enabled,
        chat_id: last.and_then(|run| run.chat_id.clone()),
    }
}

/// What an event-only zoomie waits for.
fn event_label(triggers: &[AgentTrigger]) -> Option<&'static str> {
    if triggers
        .iter()
        .any(|t| matches!(t, AgentTrigger::FileWatch { .. }))
    {
        Some("on file change")
    } else if triggers.iter().any(|t| matches!(t, AgentTrigger::Webhook)) {
        Some("on webhook")
    } else {
        None
    }
}

/// "now", "in 5m", "in 2h", "in 3d" — compact, for a sidebar row.
pub fn relative_next(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (at - now).num_seconds();
    if secs < 60 {
        return "now".into();
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("in {minutes}m");
    }
    // Round to the nearest hour so 1h59m reads "in 2h".
    let hours = (minutes + 30) / 60;
    if hours < 24 {
        return format!("in {hours}h");
    }
    let days = (hours + 12) / 24;
    format!("in {days}d")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone as _};
    use zeron_proto::agents::{AgentRun, AgentTriggerKind};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 28, 10, 0, 0).unwrap()
    }

    fn agent(triggers: serde_json::Value) -> AgentSummary {
        serde_json::from_value(serde_json::json!({
            "id": "z1",
            "name": "Nightly",
            "harness": "codex",
            "spaceId": "s1",
            "instructions": "go",
            "triggers": triggers,
            "workspacePath": "/repo",
            "memoryPath": "/m",
            "webhookPath": "/w",
            "consecutiveFailures": 0,
            "createdAt": "2026-10-01T00:00:00Z",
            "updatedAt": "2026-10-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn run(status: AgentRunStatus) -> AgentRun {
        AgentRun {
            id: "r1".into(),
            agent_id: "z1".into(),
            agent_name: "Nightly".into(),
            trigger: AgentTriggerKind::Cron,
            scheduled_at: now(),
            started_at: None,
            finished_at: None,
            status,
            summary: None,
            chat_id: Some("chat-1".into()),
            collapsed: 0,
            read: false,
            archived: false,
            created_at: now(),
        }
    }

    #[test]
    fn next_run_text() {
        for (secs, text) in [
            (-30, "now"),
            (0, "now"),
            (59, "now"),
            (60, "in 1m"),
            (59 * 60, "in 59m"),
            (60 * 60, "in 1h"),
            (119 * 60, "in 2h"),
            (89 * 60, "in 1h"),
            (23 * 3600, "in 23h"),
            (24 * 3600, "in 1d"),
            (36 * 3600, "in 2d"),
            (7 * 86_400, "in 7d"),
        ] {
            assert_eq!(
                relative_next(now() + Duration::seconds(secs), now()),
                text,
                "{secs}s"
            );
        }
    }

    #[test]
    fn scheduled_row_shows_its_next_run_and_last_result() {
        let mut agent = agent(serde_json::json!([{ "kind": "interval", "everySecs": 7200 }]));
        agent.next_fire_at = Some(now() + Duration::hours(2));
        let fresh = row(&agent, now());
        assert_eq!(fresh.detail, "in 2h");
        assert_eq!(fresh.status, RowStatus::Fresh);
        assert!(fresh.enabled);
        assert_eq!(fresh.chat_id, None);

        agent.last_run = Some(run(AgentRunStatus::Succeeded));
        let done = row(&agent, now());
        assert_eq!(done.status, RowStatus::Succeeded);
        assert_eq!(done.chat_id.as_deref(), Some("chat-1"));

        agent.last_run = Some(run(AgentRunStatus::Failed));
        assert_eq!(row(&agent, now()).status, RowStatus::Failed);
        agent.last_run = Some(run(AgentRunStatus::Limited));
        assert_eq!(row(&agent, now()).status, RowStatus::Held);
        agent.last_run = Some(run(AgentRunStatus::Skipped));
        assert_eq!(row(&agent, now()).status, RowStatus::Held);
    }

    #[test]
    fn a_working_run_makes_the_row_run() {
        let mut agent = agent(serde_json::json!([]));
        agent.last_run = Some(run(AgentRunStatus::Running));
        agent.active_run_id = Some("r1".into());
        let running = row(&agent, now());
        assert_eq!(running.status, RowStatus::Running);
        assert!(running.running());
        assert_eq!(running.detail, "running");
        // Queued counts as working too.
        agent.last_run = Some(run(AgentRunStatus::Queued));
        assert!(row(&agent, now()).running());
        // The active run outranks a stale finished status.
        agent.last_run = Some(run(AgentRunStatus::Failed));
        assert!(row(&agent, now()).running());
        // But a run waiting on the user says so.
        agent.last_run = Some(run(AgentRunStatus::NeedsApproval));
        let waiting = row(&agent, now());
        assert_eq!(waiting.status, RowStatus::NeedsYou);
        assert_eq!(waiting.detail, "needs you");
    }

    #[test]
    fn off_paused_and_event_rows() {
        let mut agent = agent(serde_json::json!([{ "kind": "fileWatch" }]));
        assert_eq!(row(&agent, now()).detail, "on file change");
        agent.draft.triggers = vec![AgentTrigger::Webhook];
        assert_eq!(row(&agent, now()).detail, "on webhook");
        agent.draft.triggers = vec![];
        assert_eq!(row(&agent, now()).detail, "done");

        agent.draft.enabled = false;
        agent.next_fire_at = Some(now() + Duration::hours(1));
        let off = row(&agent, now());
        assert_eq!(off.detail, "off");
        assert!(!off.enabled);

        agent.draft.enabled = true;
        agent.paused_reason = Some("Failed 3 times in a row".into());
        let paused = row(&agent, now());
        assert_eq!(paused.status, RowStatus::Paused);
        assert_eq!(paused.detail, "paused");
        assert!(!paused.enabled);
    }

    #[test]
    fn rows_keep_engine_order() {
        let mut a = agent(serde_json::json!([]));
        let mut b = a.clone();
        a.id = "a".into();
        b.id = "b".into();
        b.draft.name = "Second".into();
        let snapshot = AgentsSnapshot {
            agents: vec![a, b],
            unread_runs: 0,
            needs_approval_runs: 0,
        };
        let ids: Vec<_> = rows(&snapshot, now()).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
