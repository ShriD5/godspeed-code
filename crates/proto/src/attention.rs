//! The "Needs you" inbox (`WatchAttention` / … RPCs): one ordered queue of
//! everything waiting on the user across the chats and always-on agent runs
//! an engine hosts. Device-scoped like the agents it folds in, so every RPC
//! forwards on `targetDeviceId`.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ApprovalDecision, ApprovalRequest, ApprovalRisk, HarnessId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttentionKind {
    /// A permission request waiting in a live turn.
    Approval,
    /// A question waiting in a live turn.
    Question,
    /// A question nobody answered in time: the agent was told to carry on.
    TimedOut,
    /// The last turn or agent run failed.
    Failed,
    /// Stopped on a usage limit until its window resets.
    Limited,
    /// Finished and not seen yet.
    Finished,
}

impl AttentionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "Approvals",
            Self::Question => "Questions",
            Self::TimedOut => "Timed out",
            Self::Failed => "Failed",
            Self::Limited => "Limited",
            Self::Finished => "Finished",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttentionAction {
    Approve,
    Decline,
    /// Open the chat (answering a question happens there).
    Open,
    /// Mark it seen and drop it from the queue.
    Dismiss,
}

/// The always-on agent run an item came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionAgentRun {
    pub run_id: String,
    pub agent_id: String,
    pub agent_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionItem {
    /// Stable while the item lasts: `approval:{chat}:{request}`, `chat:{chat}`, `run:{run}` ….
    pub id: String,
    pub kind: AttentionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// The project's display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    /// When it started needing the user.
    pub created_at: DateTime<Utc>,
    /// Approvals only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<ApprovalRisk>,
    /// One line: what it is waiting on or what happened.
    pub summary: String,
    pub actions: Vec<AttentionAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The exact request behind an approval, for the approval card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run: Option<AttentionAgentRun>,
}

impl AttentionItem {
    pub fn supports(&self, action: AttentionAction) -> bool {
        self.actions.contains(&action)
    }

    /// Only low-risk approvals may be approved in a batch.
    pub fn batch_approvable(&self) -> bool {
        self.kind == AttentionKind::Approval
            && self.risk == Some(ApprovalRisk::Low)
            && self.supports(AttentionAction::Approve)
    }

    /// Queue position band: high-risk approvals and questions first, then
    /// the other approvals, failures, and finished work.
    pub fn band(&self) -> u8 {
        match (self.kind, self.risk) {
            (AttentionKind::Approval, Some(ApprovalRisk::High)) => 0,
            (AttentionKind::Question, _) => 1,
            (AttentionKind::Approval, Some(ApprovalRisk::Medium) | None) => 2,
            (AttentionKind::Approval, Some(ApprovalRisk::Low)) => 3,
            (AttentionKind::Failed, _) => 4,
            (AttentionKind::Limited, _) => 5,
            (AttentionKind::TimedOut, _) => 6,
            (AttentionKind::Finished, _) => 7,
        }
    }
}

/// Queue order: band, then oldest first, id tiebreak so it is total.
pub fn attention_order(a: &AttentionItem, b: &AttentionItem) -> Ordering {
    a.band()
        .cmp(&b.band())
        .then_with(|| a.created_at.cmp(&b.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

/// `GetAttention` / `WatchAttention` item.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionSnapshot {
    /// The engine that hosts these items (where answers must go).
    pub device_id: String,
    /// In [`attention_order`].
    pub items: Vec<AttentionItem>,
}

impl AttentionSnapshot {
    pub fn batch_approvable(&self) -> impl Iterator<Item = &AttentionItem> {
        self.items.iter().filter(|item| item.batch_approvable())
    }
}

/// `RespondAttention` params: approve or decline one approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionResponse {
    pub item_id: String,
    pub decision: ApprovalDecision,
}

/// `ApproveLowRisk` reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchApproval {
    pub approved: Vec<String>,
    /// Asked for but no longer low-risk approvals (answered, or the request changed).
    pub skipped: Vec<String>,
}

// ---------------------------------------------------------------------------
// Question timeout
// ---------------------------------------------------------------------------

/// A chat's own question timeout, overriding the device default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ChatQuestionTimeout {
    Off,
    After { minutes: u32 },
}

/// Device setting for chats this engine hosts (`GetQuestionTimeoutSettings`):
/// after this long an unanswered question is parked as timed out and the
/// agent is told to use its judgement. Approvals never time out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionTimeoutSettings {
    /// `None` (the default) never times out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub chats: BTreeMap<String, ChatQuestionTimeout>,
}

impl QuestionTimeoutSettings {
    /// Minutes before `chat_id`'s questions time out; `None` never.
    pub fn minutes_for(&self, chat_id: &str) -> Option<u32> {
        match self.chats.get(chat_id) {
            Some(ChatQuestionTimeout::Off) => None,
            Some(ChatQuestionTimeout::After { minutes }) => Some(*minutes),
            None => self.default_minutes,
        }
        .filter(|minutes| *minutes > 0)
    }
}

/// What a timed-out question is answered with.
pub const QUESTION_TIMEOUT_ANSWER: &str = "No answer from the user in time. Use your best judgement and continue, or stop if you can't proceed safely.";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn item(
        id: &str,
        kind: AttentionKind,
        risk: Option<ApprovalRisk>,
        minute: u32,
    ) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind,
            chat_id: None,
            title: id.into(),
            space_id: None,
            project: None,
            harness: None,
            created_at: Utc.with_ymd_and_hms(2026, 10, 17, 9, minute, 0).unwrap(),
            risk,
            summary: String::new(),
            actions: vec![AttentionAction::Approve, AttentionAction::Open],
            request_id: None,
            approval: None,
            agent_run: None,
        }
    }

    #[test]
    fn queue_order_puts_risk_and_questions_first_then_failures_then_finished_oldest_first() {
        use ApprovalRisk::*;
        use AttentionKind::*;
        let mut items = vec![
            item("finished-old", Finished, None, 1),
            item("low", Approval, Some(Low), 2),
            item("failed", Failed, None, 3),
            item("question", Question, None, 9),
            item("high-new", Approval, Some(High), 8),
            item("high-old", Approval, Some(High), 4),
            item("medium", Approval, Some(Medium), 0),
            item("limited", Limited, None, 0),
            item("timed-out", TimedOut, None, 0),
        ];
        items.sort_by(attention_order);
        let order: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            order,
            [
                "high-old",
                "high-new",
                "question",
                "medium",
                "low",
                "failed",
                "limited",
                "timed-out",
                "finished-old"
            ]
        );
    }

    #[test]
    fn only_low_risk_approvals_batch() {
        assert!(item("a", AttentionKind::Approval, Some(ApprovalRisk::Low), 0).batch_approvable());
        assert!(
            !item("b", AttentionKind::Approval, Some(ApprovalRisk::Medium), 0).batch_approvable()
        );
        assert!(
            !item("c", AttentionKind::Approval, Some(ApprovalRisk::High), 0).batch_approvable()
        );
        assert!(!item("d", AttentionKind::Question, Some(ApprovalRisk::Low), 0).batch_approvable());
        let mut no_approve = item("e", AttentionKind::Approval, Some(ApprovalRisk::Low), 0);
        no_approve.actions = vec![AttentionAction::Open];
        assert!(!no_approve.batch_approvable());
    }

    #[test]
    fn chat_timeouts_override_the_default_and_zero_means_never() {
        let mut settings = QuestionTimeoutSettings::default();
        assert_eq!(settings.minutes_for("a"), None);
        settings.default_minutes = Some(15);
        assert_eq!(settings.minutes_for("a"), Some(15));
        settings.chats.insert("a".into(), ChatQuestionTimeout::Off);
        settings
            .chats
            .insert("b".into(), ChatQuestionTimeout::After { minutes: 5 });
        settings
            .chats
            .insert("c".into(), ChatQuestionTimeout::After { minutes: 0 });
        assert_eq!(settings.minutes_for("a"), None);
        assert_eq!(settings.minutes_for("b"), Some(5));
        assert_eq!(settings.minutes_for("c"), None);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            json["chats"]["b"],
            serde_json::json!({"mode": "after", "minutes": 5})
        );
        let round: QuestionTimeoutSettings = serde_json::from_value(json).unwrap();
        assert_eq!(round, settings);
    }
}
