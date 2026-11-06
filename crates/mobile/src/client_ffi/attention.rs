//! FFI view of the cross-device "Needs you" inbox (`zeron_client::attention`)
//! and the usage feeds (agent plan windows, usage history).

use zeron_client as zc;
use zeron_proto::attention::{AttentionItem, AttentionKind as ProtoKind};
use zeron_proto::{ApprovalRisk as ProtoRisk, UsageBucketSize};

// ── inbox ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AttentionKind {
    Approval,
    Question,
    TimedOut,
    Failed,
    Limited,
    Finished,
}

impl From<ProtoKind> for AttentionKind {
    fn from(k: ProtoKind) -> Self {
        match k {
            ProtoKind::Approval => Self::Approval,
            ProtoKind::Question => Self::Question,
            ProtoKind::TimedOut => Self::TimedOut,
            ProtoKind::Failed => Self::Failed,
            ProtoKind::Limited => Self::Limited,
            ProtoKind::Finished => Self::Finished,
            // The iPhone groups pull request events with failures until its
            // bindings carry the new kind; the summary says what happened.
            ProtoKind::PullRequest => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ApprovalRisk {
    Low,
    Medium,
    High,
}

impl From<ProtoRisk> for ApprovalRisk {
    fn from(r: ProtoRisk) -> Self {
        match r {
            ProtoRisk::Low => Self::Low,
            ProtoRisk::Medium => Self::Medium,
            ProtoRisk::High => Self::High,
        }
    }
}

/// The exact request behind an approval row.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ApprovalPreview {
    /// "Run command", "Edit files", …
    pub kind_label: String,
    /// One line: what the agent wants to do.
    pub title: String,
    /// The exact command line (command approvals).
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub paths: Vec<String>,
    /// Unified diff preview (edit approvals), when the provider gave one.
    pub diff: Option<String>,
    pub reason: Option<String>,
    /// The labels one-tap approve / decline send.
    pub approve_label: Option<String>,
    pub decline_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AttentionItemView {
    /// Stable while the item lasts (unique per host).
    pub id: String,
    /// The host the answer goes to.
    pub device_id: String,
    pub device_name: Option<String>,
    pub kind: AttentionKind,
    pub chat_id: Option<String>,
    pub title: String,
    pub project: Option<String>,
    /// Wire harness id (`claude-code`), when known.
    pub harness: Option<String>,
    /// Always-on agent the item came from, when it did.
    pub agent_name: Option<String>,
    pub created_at_ms: i64,
    pub risk: Option<ApprovalRisk>,
    pub summary: String,
    /// Queue band (0 = high-risk approvals … 7 = finished).
    pub band: u8,
    pub approval: Option<ApprovalPreview>,
    pub can_approve: bool,
    pub can_decline: bool,
    pub can_dismiss: bool,
    /// Part of "Approve all low-risk".
    pub batch_approvable: bool,
    /// From the registry (host without a live inbox): open-only.
    pub derived: bool,
}

fn harness_wire(item: &AttentionItem) -> Option<String> {
    item.harness
        .and_then(|h| serde_json::to_value(h).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
}

impl From<&zc::AttentionEntry> for AttentionItemView {
    fn from(e: &zc::AttentionEntry) -> Self {
        let item = &e.item;
        let label = |approve: bool| {
            let approval = item.approval.as_ref()?;
            let decision = zc::attention::decision_for(approval, approve)?;
            approval
                .decisions
                .iter()
                .find(|o| o.decision == decision)
                .map(|o| o.label.clone())
        };
        Self {
            id: item.id.clone(),
            device_id: e.device_id.clone(),
            device_name: e.device_name.clone(),
            kind: item.kind.into(),
            chat_id: item.chat_id.clone(),
            title: item.title.clone(),
            project: item.project.clone(),
            harness: harness_wire(item),
            agent_name: item.agent_run.as_ref().map(|r| r.agent_name.clone()),
            created_at_ms: item.created_at.timestamp_millis(),
            risk: item.risk.map(Into::into),
            summary: item.summary.clone(),
            band: e.band(),
            approval: item.approval.as_ref().map(|a| ApprovalPreview {
                kind_label: a.kind.label().to_owned(),
                title: a.title.clone(),
                command: a.command.clone(),
                cwd: a.cwd.clone(),
                paths: a.paths.clone(),
                diff: a.diff.clone(),
                reason: a.reason.clone(),
                approve_label: label(true),
                decline_label: label(false),
            }),
            can_approve: e.can_approve(),
            can_decline: e.can_decline(),
            can_dismiss: e.can_dismiss(),
            batch_approvable: e.batch_approvable(),
            derived: e.derived,
        }
    }
}

/// The merged queue, in risk order across every host.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AttentionView {
    pub revision: u64,
    pub items: Vec<AttentionItemView>,
    /// Hosts answering with a live inbox now (others are registry-derived).
    pub live_devices: Vec<String>,
}

impl From<&zc::AttentionView> for AttentionView {
    fn from(v: &zc::AttentionView) -> Self {
        Self {
            revision: v.revision,
            items: v.items.iter().map(Into::into).collect(),
            live_devices: v.live_devices.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BatchResult {
    pub approved: u32,
    pub skipped: u32,
    pub failed_devices: Vec<String>,
}

impl From<zc::BatchResult> for BatchResult {
    fn from(b: zc::BatchResult) -> Self {
        Self {
            approved: b.approved,
            skipped: b.skipped,
            failed_devices: b.failed_devices,
        }
    }
}

// ── usage: plan windows ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum UsageWindowKind {
    FiveHour,
    Weekly,
    Other,
}

/// Desktop `window_kind`: the labels engines emit for the 5-hour and weekly
/// windows, any spelling.
pub fn window_kind(label: &str) -> UsageWindowKind {
    let l = label.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    match l.as_str() {
        "session" | "5h" | "5-hour" | "five-hour" | "5-hours" => UsageWindowKind::FiveHour,
        "week" | "weekly" | "7-day" | "seven-day" | "7d" => UsageWindowKind::Weekly,
        _ => UsageWindowKind::Other,
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UsageWindowView {
    /// The engine's label ("Session", "Week", "Month").
    pub label: String,
    pub kind: UsageWindowKind,
    /// 0…1.
    pub used_fraction: f64,
    pub resets_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AgentAccountView {
    pub id: String,
    /// Wire harness id (`claude-code`).
    pub harness: String,
    pub harness_label: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub plan_label: Option<String>,
    pub active: bool,
    /// 5-hour first, then weekly, then the rest (desktop order).
    pub windows: Vec<UsageWindowView>,
    pub fetched_at_ms: Option<i64>,
    pub error: Option<String>,
}

impl From<&zeron_proto::AgentAccount> for AgentAccountView {
    fn from(a: &zeron_proto::AgentAccount) -> Self {
        let harness = serde_json::to_value(a.harness)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let mut windows: Vec<UsageWindowView> = a
            .usage_windows
            .iter()
            .map(|w| UsageWindowView {
                label: w.label.clone(),
                kind: window_kind(&w.label),
                used_fraction: f64::from(w.used_fraction.clamp(0.0, 1.0)),
                resets_at_ms: w.resets_at.map(|t| t.timestamp_millis()),
            })
            .collect();
        let rank = |k: UsageWindowKind| match k {
            UsageWindowKind::FiveHour => 0,
            UsageWindowKind::Weekly => 1,
            UsageWindowKind::Other => 2,
        };
        windows.sort_by_key(|w| rank(w.kind));
        Self {
            id: a.id.clone(),
            harness_label: zc::catalog::harness_label(&harness),
            harness,
            email: a.email.clone(),
            display_name: a.display_name.clone(),
            plan_label: a.plan_label.clone(),
            active: a.active,
            windows,
            fetched_at_ms: a.usage_fetched_at,
            error: a.usage_error.clone(),
        }
    }
}

// ── usage: history ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum UsageRange {
    Today,
    Week,
    Month,
    Quarter,
    All,
}

impl From<UsageRange> for zeron_proto::UsageRange {
    fn from(r: UsageRange) -> Self {
        match r {
            UsageRange::Today => Self::Today,
            UsageRange::Week => Self::Week,
            UsageRange::Month => Self::Month,
            UsageRange::Quarter => Self::Quarter,
            UsageRange::All => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum UsageBucketKind {
    Hour,
    Day,
    Week,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UsageAmountView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    /// Estimated USD at list API prices (priced tokens only).
    pub cost_usd: f64,
    pub unpriced_tokens: u64,
    pub cache_savings_usd: f64,
    pub turns: u64,
    pub calls: u64,
}

impl From<&zeron_proto::UsageAmount> for UsageAmountView {
    fn from(a: &zeron_proto::UsageAmount) -> Self {
        Self {
            input_tokens: a.tokens.input,
            output_tokens: a.tokens.output,
            cache_read_tokens: a.tokens.cache_read,
            cache_write_tokens: a.tokens.cache_write,
            total_tokens: a.tokens.total(),
            cost_usd: a.cost_usd,
            unpriced_tokens: a.unpriced_tokens,
            cache_savings_usd: a.cache_savings_usd,
            turns: a.turns,
            calls: a.calls,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UsageBucketView {
    pub start_ms: i64,
    /// Local hour, for hourly buckets.
    pub hour: Option<u8>,
    /// Summed over every provider and model.
    pub amount: UsageAmountView,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UsageSliceView {
    pub key: String,
    pub label: String,
    pub amount: UsageAmountView,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UsageHistoryView {
    pub bucket: UsageBucketKind,
    pub time_zone: String,
    pub buckets: Vec<UsageBucketView>,
    pub total: UsageAmountView,
    pub sessions: u64,
    pub by_model: Vec<UsageSliceView>,
    pub by_provider: Vec<UsageSliceView>,
    /// The engine's index is still catching up: ask again shortly.
    pub indexing: bool,
    /// When the price table was last checked (`2026-10-18`).
    pub prices_checked: String,
}

impl From<&zeron_proto::UsageHistory> for UsageHistoryView {
    fn from(h: &zeron_proto::UsageHistory) -> Self {
        let slice = |s: &zeron_proto::UsageSlice| UsageSliceView {
            key: s.key.clone(),
            label: s.label.clone(),
            amount: (&s.amount).into(),
        };
        Self {
            bucket: match h.bucket {
                UsageBucketSize::Hour => UsageBucketKind::Hour,
                UsageBucketSize::Day => UsageBucketKind::Day,
                UsageBucketSize::Week => UsageBucketKind::Week,
            },
            time_zone: h.time_zone.clone(),
            buckets: h
                .buckets
                .iter()
                .map(|b| {
                    let mut sum = zeron_proto::UsageAmount::default();
                    for p in &b.points {
                        sum.add(&p.amount);
                    }
                    UsageBucketView {
                        start_ms: b.start.timestamp_millis(),
                        hour: b.hour,
                        amount: (&sum).into(),
                    }
                })
                .collect(),
            total: (&h.total).into(),
            sessions: h.sessions,
            by_model: h.by_model.iter().map(slice).collect(),
            by_provider: h.by_provider.iter().map(slice).collect(),
            indexing: h.sources.indexing,
            prices_checked: h.prices_checked.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_labels_classify_like_the_desktop() {
        for l in ["Session", "5h", "5-hour", "five_hour", "5 hour"] {
            assert_eq!(window_kind(l), UsageWindowKind::FiveHour, "{l}");
        }
        for l in ["Week", "weekly", "7-day", "seven_day"] {
            assert_eq!(window_kind(l), UsageWindowKind::Weekly, "{l}");
        }
        assert_eq!(window_kind("Month"), UsageWindowKind::Other);
    }
}
