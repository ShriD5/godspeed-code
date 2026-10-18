//! Usage history (`GetUsageHistory`): tokens and estimated API cost over a
//! range, per local day (or hour, or week), with breakdowns by model,
//! provider, project and chat.
//!
//! The engine answers from its own index: the turns Godspeed ran (their
//! stamped `TurnStats` usage) plus, for history from
//! before Godspeed or outside it, Claude Code and Codex's own transcript
//! logs, with a turn Godspeed ran never counted twice. Costs are estimates
//! at list API prices ([`crate::pricing`]), shown for subscription logins
//! too; tokens on a model the price table doesn't know count as unpriced.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// The window the page asks for. Days are the viewer's local days, today
/// included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsageRange {
    /// Today, in hourly buckets.
    #[serde(rename = "today")]
    Today,
    #[serde(rename = "7d")]
    Week,
    #[default]
    #[serde(rename = "30d")]
    Month,
    #[serde(rename = "90d")]
    Quarter,
    /// Everything indexed, daily (weekly past 92 days).
    #[serde(rename = "all")]
    All,
}

impl UsageRange {
    pub const ALL: [UsageRange; 5] = [
        UsageRange::Today,
        UsageRange::Week,
        UsageRange::Month,
        UsageRange::Quarter,
        UsageRange::All,
    ];

    /// Local days covered (today counts as one); `None` for everything.
    pub fn days(self) -> Option<u32> {
        match self {
            UsageRange::Today => Some(1),
            UsageRange::Week => Some(7),
            UsageRange::Month => Some(30),
            UsageRange::Quarter => Some(90),
            UsageRange::All => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UsageRange::Today => "Today",
            UsageRange::Week => "7 days",
            UsageRange::Month => "30 days",
            UsageRange::Quarter => "90 days",
            UsageRange::All => "All",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistoryRequest {
    #[serde(default)]
    pub range: UsageRange,
    /// IANA zone whose days the buckets follow (`Asia/Kolkata`). Absent or
    /// unknown: the engine host's zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageBucketSize {
    Hour,
    #[default]
    Day,
    /// Weeks starting Monday.
    Week,
}

/// Token counts. `input` excludes cache traffic, which rides its own fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl UsageTokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &UsageTokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

/// What some slice of usage came to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAmount {
    pub tokens: UsageTokens,
    /// Estimated USD at list API prices, over the priced tokens only.
    pub cost_usd: f64,
    /// Tokens on models the price table doesn't know (left out of the cost).
    pub unpriced_tokens: u64,
    /// What cache reads saved over plain input, at the same prices.
    pub cache_savings_usd: f64,
    /// Turns Godspeed ran.
    pub turns: u64,
    /// Model calls read from Claude Code / Codex logs (history from before
    /// or outside Godspeed).
    pub calls: u64,
}

impl UsageAmount {
    pub fn add(&mut self, other: &UsageAmount) {
        self.tokens.add(&other.tokens);
        self.cost_usd += other.cost_usd;
        self.unpriced_tokens += other.unpriced_tokens;
        self.cache_savings_usd += other.cache_savings_usd;
        self.turns += other.turns;
        self.calls += other.calls;
    }

    /// Every token was on an unpriced model.
    pub fn wholly_unpriced(&self) -> bool {
        self.unpriced_tokens > 0 && self.unpriced_tokens >= self.tokens.total()
    }
}

/// One provider+model's share of a bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePoint {
    /// Kebab-case harness id (`claude-code`, `codex`, …).
    pub provider: String,
    /// Canonical model id (`claude-opus-5-5`), `unknown` when none was
    /// recorded.
    pub model: String,
    pub amount: UsageAmount,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub start: DateTime<Utc>,
    /// The local day the bucket starts on (the Monday, for weeks).
    pub day: NaiveDate,
    /// Local hour, for hourly buckets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hour: Option<u8>,
    pub points: Vec<UsagePoint>,
}

/// One row of a breakdown (model, provider or project).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSlice {
    pub key: String,
    pub label: String,
    pub amount: UsageAmount,
}

/// A chat (or, for logs outside Godspeed, a CLI session) in the top list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageChatSlice {
    /// The Godspeed chat, when the usage belongs to one; `None` for a CLI
    /// session no chat continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub title: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub last_at: DateTime<Utc>,
    pub amount: UsageAmount,
}

/// Where the numbers came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSources {
    /// Godspeed turns with usage, all time.
    pub godspeed_turns: u64,
    /// Claude Code transcript files indexed.
    pub claude_files: u64,
    /// Codex session files indexed.
    pub codex_files: u64,
    /// Log records skipped because Godspeed already counted that turn, in
    /// this range.
    pub deduplicated: u64,
    /// The index is still catching up (first backfill, or a scan running):
    /// ask again shortly.
    pub indexing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistory {
    pub range: UsageRange,
    pub bucket: UsageBucketSize,
    /// The IANA zone the buckets follow.
    pub time_zone: String,
    /// `[start, end)` of the range; `None` when nothing is indexed (`All`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    /// Every bucket of the range in order, empty ones included.
    pub buckets: Vec<UsageBucket>,
    pub total: UsageAmount,
    /// Distinct chats and unlinked CLI sessions with usage in the range.
    pub sessions: u64,
    /// Most expensive first (then most tokens).
    pub by_model: Vec<UsageSlice>,
    pub by_provider: Vec<UsageSlice>,
    pub by_project: Vec<UsageSlice>,
    pub top_chats: Vec<UsageChatSlice>,
    pub sources: UsageSources,
    /// When the price table was last checked (`2026-10-18`).
    pub prices_checked: String,
}

/// The display name of a provider id (`claude-code` → "Claude Code");
/// an id this build doesn't know shows as itself.
pub fn provider_label(provider: &str) -> String {
    match provider {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "devin" => "Devin",
        "grok" => "Grok",
        "hermes" => "Hermes",
        "pi" => "Pi",
        "muse" => "Muse Code",
        "opencode" => "OpenCode",
        "antigravity" => "Antigravity",
        "mock" => "Mock",
        "unknown" | "" => "Unknown agent",
        other => return other.to_owned(),
    }
    .to_owned()
}

/// This host's IANA zone name: `$TZ`, else the `/etc/localtime` link
/// target. `None` when neither names one (the engine then uses its own).
pub fn local_time_zone() -> Option<String> {
    std::env::var("TZ")
        .ok()
        .map(|zone| zone.trim().trim_start_matches(':').to_owned())
        .filter(|zone| !zone.is_empty() && !zone.starts_with('/'))
        .or_else(|| {
            let target = std::fs::read_link("/etc/localtime").ok()?;
            let target = target.to_string_lossy();
            target
                .split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_use_short_wire_names() {
        let json = serde_json::to_string(&UsageHistoryRequest {
            range: UsageRange::Week,
            time_zone: Some("Asia/Kolkata".into()),
        })
        .unwrap();
        assert_eq!(json, r#"{"range":"7d","timeZone":"Asia/Kolkata"}"#);
        let back: UsageHistoryRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(back.range, UsageRange::Month);
        assert_eq!(UsageRange::Today.days(), Some(1));
        assert_eq!(UsageRange::All.days(), None);
    }

    #[test]
    fn amounts_add_up() {
        let mut a = UsageAmount {
            tokens: UsageTokens {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
            },
            cost_usd: 0.5,
            unpriced_tokens: 0,
            cache_savings_usd: 0.1,
            turns: 1,
            calls: 0,
        };
        let b = UsageAmount {
            unpriced_tokens: 10,
            calls: 2,
            ..a
        };
        a.add(&b);
        assert_eq!(a.tokens.total(), 20);
        assert_eq!((a.turns, a.calls, a.unpriced_tokens), (2, 2, 10));
        assert!(!a.wholly_unpriced());
        let none = UsageAmount {
            tokens: UsageTokens {
                input: 5,
                ..Default::default()
            },
            unpriced_tokens: 5,
            ..Default::default()
        };
        assert!(none.wholly_unpriced());
    }
}
