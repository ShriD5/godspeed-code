//! Access modes and approvals: how much an agent may do before it asks, and
//! the approval request it raises when it has to.
//!
//! An approval rides the existing question plumbing: it is a
//! [`UserInputQuestion`] carrying [`UserInputQuestion::approval`], so the
//! engine's input bridge, the command ledger (`RespondInput`), cross-device
//! host routing and the doc's input parts all carry it unchanged. A client
//! that predates approvals still sees an ordinary question whose options are
//! the decision labels, and answering one of them works.

use serde::{Deserialize, Serialize};

use crate::{HarnessId, SandboxLevel, UserInputAnswer, UserInputQuestion};

/// Per-thread access mode. Stored on the chat config, so it syncs like the
/// other chat settings; a run carries it in [`crate::RunRequest::access`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessMode {
    /// Ask before commands, file edits and anything else risky.
    Supervised,
    /// File edits inside the workspace run; commands ask.
    #[default]
    AutoAcceptEdits,
    /// The agent's own automatic mode (a classifier or the CLI's "auto"
    /// preset); auto-accept edits where the CLI has none.
    Auto,
    /// Never ask.
    FullAccess,
}

impl AccessMode {
    /// Picker order, which is also the cycle order.
    pub const ALL: [AccessMode; 4] = [
        AccessMode::Supervised,
        AccessMode::AutoAcceptEdits,
        AccessMode::Auto,
        AccessMode::FullAccess,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Supervised => "Supervised",
            Self::AutoAcceptEdits => "Auto-accept edits",
            Self::Auto => "Auto",
            Self::FullAccess => "Full access",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Supervised => "Ask before commands, edits and anything risky",
            Self::AutoAcceptEdits => "Edit files in the workspace; ask before commands",
            Self::Auto => "Let the agent's own auto mode decide what needs a yes",
            Self::FullAccess => "Never ask; no sandbox",
        }
    }

    /// The legacy `(sandbox, auto_approve)` pair a run request read before
    /// access modes existed. Old runs, old clients and always-on agents
    /// saved before the field map through here.
    pub fn from_legacy(sandbox: SandboxLevel, auto_approve: bool) -> Self {
        match (sandbox, auto_approve) {
            (SandboxLevel::DangerFullAccess, true) => Self::FullAccess,
            (_, true) => Self::Auto,
            (SandboxLevel::ReadOnly, false) => Self::Supervised,
            (_, false) => Self::AutoAcceptEdits,
        }
    }

    /// The inverse of [`Self::from_legacy`], written alongside the mode so an
    /// older host still runs with no more access than the mode allows.
    pub fn legacy(self) -> (SandboxLevel, bool) {
        match self {
            Self::Supervised => (SandboxLevel::ReadOnly, false),
            Self::AutoAcceptEdits => (SandboxLevel::WorkspaceWrite, false),
            Self::Auto => (SandboxLevel::WorkspaceWrite, true),
            Self::FullAccess => (SandboxLevel::DangerFullAccess, true),
        }
    }

    /// The next mode supported by `harness`, wrapping (the composer's cycle
    /// shortcut).
    pub fn cycle(self, harness: HarnessId) -> Self {
        let modes = supported_access_modes(harness);
        let at = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        (1..=Self::ALL.len())
            .map(|step| Self::ALL[(at + step) % Self::ALL.len()])
            .find(|m| modes.contains(m))
            .unwrap_or(self)
    }
}

/// The modes `harness` can honor without running with more access than the
/// mode allows. The picker greys out the rest, and the host refuses a run in
/// one rather than silently widening it. Verified per driver; see each
/// driver's `access` docs.
pub fn supported_access_modes(harness: HarnessId) -> &'static [AccessMode] {
    match harness {
        // The SDK has no permission callback; its auto review is the one
        // gate short of full access.
        HarnessId::Cursor => &[AccessMode::Auto, AccessMode::FullAccess],
        // Pi has no permission system: every tool runs.
        HarnessId::Pi => &[AccessMode::FullAccess],
        _ => &AccessMode::ALL,
    }
}

/// One line of the support table, for the picker's unavailable rows.
pub fn access_mode_supported(harness: HarnessId, mode: AccessMode) -> bool {
    supported_access_modes(harness).contains(&mode)
}

/// The access a run launches with.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAccess {
    pub mode: AccessMode,
    /// Directories outside the workspace the agent may also write under the
    /// sandboxed modes, where the driver supports extra writable roots (an
    /// always-on agent's memory directory).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
}

impl RunAccess {
    pub fn new(mode: AccessMode) -> Self {
        Self {
            mode,
            writable_roots: Vec::new(),
        }
    }
}

/// What an approval is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalKind {
    Command,
    FileChange,
    FileRead,
    Network,
    McpTool,
    Other,
}

impl ApprovalKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "Run command",
            Self::FileChange => "Edit files",
            Self::FileRead => "Read files",
            Self::Network => "Network access",
            Self::McpTool => "Use tool",
            Self::Other => "Permission",
        }
    }

    /// File edits are what auto-accept-edits lets through.
    pub fn is_edit(self) -> bool {
        self == Self::FileChange
    }
}

/// An answer to an approval. Providers offer a subset, in their own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Approve,
    /// Approve this and matching requests for the rest of the session.
    ApproveForSession,
    /// The provider remembers the approval beyond this session.
    ApproveAlways,
    Decline,
    /// The provider remembers the refusal beyond this session.
    DeclineAlways,
    /// Decline and stop the turn.
    Cancel,
}

impl ApprovalDecision {
    pub fn approves(self) -> bool {
        matches!(
            self,
            Self::Approve | Self::ApproveForSession | Self::ApproveAlways
        )
    }

    pub fn default_label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::ApproveForSession => "Approve for this session",
            Self::ApproveAlways => "Always approve",
            Self::Decline => "Decline",
            Self::DeclineAlways => "Always decline",
            Self::Cancel => "Decline and stop",
        }
    }

    /// The stable spelling an orchestrator or the phone may answer with.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::ApproveForSession => "approveForSession",
            Self::ApproveAlways => "approveAlways",
            Self::Decline => "decline",
            Self::DeclineAlways => "declineAlways",
            Self::Cancel => "cancel",
        }
    }
}

/// One button on the approval card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalOption {
    pub decision: ApprovalDecision,
    pub label: String,
}

impl ApprovalOption {
    pub fn new(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            label: decision.default_label().into(),
        }
    }
}

/// Bounds on what an approval copies into the chat doc.
pub const APPROVAL_COMMAND_MAX_BYTES: usize = 16 * 1024;
pub const APPROVAL_DIFF_MAX_BYTES: usize = 48 * 1024;
pub const APPROVAL_MAX_PATHS: usize = 64;

/// A permission request from the agent. It is identified by the question it
/// rides ([`UserInputQuestion::id`]) and the input request around that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub kind: ApprovalKind,
    /// One line: what the agent wants to do.
    pub title: String,
    /// The exact command line, for command approvals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Files the request touches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Unified diff preview of an edit, when the provider gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The agent's stated reason, when it gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The decisions the provider accepts, in display order.
    pub decisions: Vec<ApprovalOption>,
}

impl ApprovalRequest {
    pub fn new(kind: ApprovalKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            command: None,
            cwd: None,
            paths: Vec::new(),
            diff: None,
            reason: None,
            decisions: vec![
                ApprovalOption::new(ApprovalDecision::Approve),
                ApprovalOption::new(ApprovalDecision::ApproveForSession),
                ApprovalOption::new(ApprovalDecision::Decline),
            ],
        }
    }

    pub fn with_decisions(mut self, decisions: &[ApprovalDecision]) -> Self {
        self.decisions = decisions.iter().copied().map(ApprovalOption::new).collect();
        self
    }

    /// Enforce the doc bounds: the full command and diff stay in the host's
    /// run journal through the tool call itself.
    pub fn bounded(mut self) -> Self {
        if let Some(command) = &mut self.command {
            truncate_marked(command, APPROVAL_COMMAND_MAX_BYTES);
        }
        if let Some(diff) = &mut self.diff {
            truncate_marked(diff, APPROVAL_DIFF_MAX_BYTES);
        }
        self.paths.truncate(APPROVAL_MAX_PATHS);
        self
    }

    /// The question this request rides. Its options are the decision labels,
    /// so a client without approval support can still answer it.
    pub fn into_question(self, id: impl Into<String>) -> UserInputQuestion {
        let request = self.bounded();
        UserInputQuestion {
            id: id.into(),
            header: request.kind.label().into(),
            question: request.title.clone(),
            options: request.decisions.iter().map(|d| d.label.clone()).collect(),
            multi_select: false,
            prefill: None,
            multiline: false,
            approval: Some(request),
        }
    }

    /// Read a decision off answer labels: an option label, else a decision's
    /// wire name, else (for old yes/no clients) "yes"/"no".
    pub fn decision_for(&self, label: &str) -> Option<ApprovalDecision> {
        let label = label.trim();
        let offered = |d: ApprovalDecision| self.decisions.iter().any(|o| o.decision == d);
        self.decisions
            .iter()
            .find(|o| o.label == label)
            .map(|o| o.decision)
            .or_else(|| {
                self.decisions
                    .iter()
                    .find(|o| o.decision.wire_name().eq_ignore_ascii_case(label))
                    .map(|o| o.decision)
            })
            .or_else(|| match label.to_ascii_lowercase().as_str() {
                "yes" | "allow" | "approve" if offered(ApprovalDecision::Approve) => {
                    Some(ApprovalDecision::Approve)
                }
                "no" | "deny" | "reject" | "decline" => Some(self.decline()),
                _ => None,
            })
    }

    /// The decision `answers` make for the question `question_id`. No answer
    /// (the run stopped, the asker went away) declines; never a silent allow.
    pub fn decide(&self, question_id: &str, answers: &[UserInputAnswer]) -> ApprovalDecision {
        answers
            .iter()
            .find(|a| a.question_id == question_id)
            .and_then(|a| a.labels.iter().find_map(|l| self.decision_for(l)))
            .unwrap_or_else(|| self.decline())
    }

    /// The plain refusal this request offers.
    pub fn decline(&self) -> ApprovalDecision {
        [ApprovalDecision::Decline, ApprovalDecision::Cancel]
            .into_iter()
            .find(|d| self.decisions.iter().any(|o| o.decision == *d))
            .unwrap_or(ApprovalDecision::Decline)
    }
}

impl UserInputQuestion {
    pub fn is_approval(&self) -> bool {
        self.approval.is_some()
    }
}

fn truncate_marked(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n… (truncated)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_pairs_round_trip_through_every_mode() {
        for mode in AccessMode::ALL {
            let (sandbox, auto) = mode.legacy();
            assert_eq!(AccessMode::from_legacy(sandbox, auto), mode);
        }
        // Old chats sent workspace-write without auto-approve.
        assert_eq!(
            AccessMode::from_legacy(SandboxLevel::WorkspaceWrite, false),
            AccessMode::AutoAcceptEdits
        );
        assert_eq!(AccessMode::default(), AccessMode::AutoAcceptEdits);
        assert_eq!(
            serde_json::to_value(AccessMode::AutoAcceptEdits).unwrap(),
            "auto-accept-edits"
        );
    }

    #[test]
    fn cycle_skips_modes_the_harness_cannot_honor() {
        assert_eq!(
            AccessMode::Supervised.cycle(HarnessId::Codex),
            AccessMode::AutoAcceptEdits
        );
        assert_eq!(
            AccessMode::FullAccess.cycle(HarnessId::Codex),
            AccessMode::Supervised
        );
        assert_eq!(
            AccessMode::Supervised.cycle(HarnessId::Cursor),
            AccessMode::Auto
        );
        assert_eq!(
            AccessMode::FullAccess.cycle(HarnessId::Cursor),
            AccessMode::Auto
        );
        assert_eq!(
            AccessMode::Supervised.cycle(HarnessId::Pi),
            AccessMode::FullAccess
        );
    }

    #[test]
    fn approval_question_answers_by_label_wire_name_or_yes_no() {
        let mut request = ApprovalRequest::new(ApprovalKind::Command, "Run cargo test");
        request.command = Some("cargo test".into());
        let question = request.clone().into_question("q1");
        assert_eq!(
            question.options,
            ["Approve", "Approve for this session", "Decline"]
        );
        assert_eq!(question.header, "Run command");
        let answer = |label: &str| {
            vec![UserInputAnswer {
                question_id: "q1".into(),
                labels: vec![label.into()],
            }]
        };
        assert_eq!(
            request.decide("q1", &answer("Approve for this session")),
            ApprovalDecision::ApproveForSession
        );
        assert_eq!(
            request.decide("q1", &answer("approveForSession")),
            ApprovalDecision::ApproveForSession
        );
        assert_eq!(
            request.decide("q1", &answer("yes")),
            ApprovalDecision::Approve
        );
        assert_eq!(request.decide("q1", &[]), ApprovalDecision::Decline);
        assert_eq!(
            request.decide("other", &answer("Approve")),
            ApprovalDecision::Decline
        );
    }

    #[test]
    fn approval_rides_the_question_wire_additively() {
        let question = ApprovalRequest::new(ApprovalKind::FileChange, "Edit src/lib.rs")
            .into_question("q1");
        let json = serde_json::to_value(&question).unwrap();
        assert_eq!(json["approval"]["kind"], "fileChange");
        assert_eq!(json["approval"]["decisions"][1]["decision"], "approveForSession");
        let round: UserInputQuestion = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(round, question);
        // A plain question still parses and serializes without the field.
        let mut plain = json;
        plain.as_object_mut().unwrap().remove("approval");
        let plain: UserInputQuestion = serde_json::from_value(plain).unwrap();
        assert!(!plain.is_approval());
        assert!(serde_json::to_value(&plain).unwrap().get("approval").is_none());
    }

    #[test]
    fn bounded_truncates_on_char_boundaries() {
        let mut request = ApprovalRequest::new(ApprovalKind::Command, "x");
        request.command = Some("界".repeat(APPROVAL_COMMAND_MAX_BYTES));
        let bounded = request.bounded();
        let command = bounded.command.unwrap();
        assert!(command.len() <= APPROVAL_COMMAND_MAX_BYTES + 20);
        assert!(command.ends_with("(truncated)"));
    }
}
