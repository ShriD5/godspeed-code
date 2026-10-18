//! What a finished turn cost and touched, stamped by the host onto the turn's
//! last assistant entry (the transcript's receipt reads it).
//!
//! Stored as ONE atomic JSON string under the entry map's `turnStats` key —
//! like `meta.contextUsage` — so a sync never tears it, older readers ignore
//! an unknown scalar, and every field inside is optional: a reader takes
//! what it understands and a parse failure costs only the receipt's extras.

use serde::{Deserialize, Serialize};

use crate::parts::ToolDiffStat;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStats {
    /// Kebab-case harness id (`claude-code`, `codex`, …). A string, not
    /// `HarnessId`, so a harness this build doesn't know still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The model the harness reported (or the one the run asked for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort the run asked for (`high`, `xhigh`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Harness-reported tokens for the whole turn; `None` when the driver
    /// reports none, or none that covers the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Time the turn sat waiting on the user (approvals, questions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<i64>,
    /// Files the turn changed in its checkout: the "Latest turn" diff, taken
    /// when the turn ended. Stamped a moment after the rest (the capture runs
    /// git); `None` outside a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ToolDiffStat>>,
    /// The harness-native session the turn ran in (Claude Code's session
    /// id, Codex's thread id): the usage history matches it against the
    /// agent's own logs so a turn is never counted twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Token counts of one turn. `input_tokens` excludes cache traffic, which
/// rides its own fields when the provider reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl TurnUsage {
    /// The usage an [`zeron_proto::AgentEvent::Usage`] carries.
    pub fn from_event(event: &zeron_proto::AgentEvent) -> Option<Self> {
        match event {
            zeron_proto::AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => Some(Self {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cache_read_tokens: *cache_read_tokens,
                cache_write_tokens: *cache_write_tokens,
            }),
            _ => None,
        }
    }

    /// Sum of two turns' usage (a receipt spanning several stamped entries).
    pub fn plus(self, other: Self) -> Self {
        let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
        };
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_tokens: add(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: add(self.cache_write_tokens, other.cache_write_tokens),
        }
    }
}

impl TurnStats {
    /// Lenient read of the stored JSON: anything unparseable is no stats.
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_unknown_fields() {
        let stats = TurnStats {
            harness: Some("claude-code".into()),
            model: Some("claude-opus-4-7".into()),
            effort: Some("high".into()),
            usage: Some(TurnUsage {
                input_tokens: 12,
                output_tokens: 340,
                cache_read_tokens: Some(48_000),
                cache_write_tokens: Some(1_200),
            }),
            wait_ms: Some(4_000),
            files: Some(vec![ToolDiffStat {
                path: "src/lib.rs".into(),
                additions: 10,
                deletions: 2,
            }]),
            session_id: Some("5b1c-session".into()),
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert_eq!(TurnStats::parse(&json), Some(stats));
        // A newer writer's extra fields never cost the ones we know.
        let newer = r#"{"model":"m","futureThing":{"x":1},"usage":{"inputTokens":1,"outputTokens":2}}"#;
        let parsed = TurnStats::parse(newer).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("m"));
        assert_eq!(parsed.usage.unwrap().output_tokens, 2);
        assert_eq!(TurnStats::parse("not json"), None);
    }

    #[test]
    fn usage_sums_keep_unreported_cache_unreported() {
        let a = TurnUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: None,
            cache_write_tokens: None,
        };
        assert_eq!(a.plus(a).cache_read_tokens, None);
        let b = TurnUsage {
            cache_read_tokens: Some(5),
            ..a
        };
        let sum = a.plus(b);
        assert_eq!((sum.input_tokens, sum.cache_read_tokens), (2, Some(5)));
    }
}
