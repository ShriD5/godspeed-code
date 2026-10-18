//! Godspeed's own turns as usage records: every entry whose stamped
//! [`zeron_doc::TurnStats`] carries usage, timed from the prompt that
//! started the turn to the end of its last segment.

use zeron_doc::{MessageRole, SessionMessageEntry};
use zeron_proto::UsageTokens;

use super::{Source, UsageRecord};

/// The usage records of one chat's transcript. Pure.
pub(super) fn turn_records(chat_id: &str, entries: &[SessionMessageEntry]) -> Vec<UsageRecord> {
    let mut out = Vec::new();
    let mut turn_start: Option<i64> = None;
    for entry in entries {
        if entry.role == MessageRole::User {
            turn_start = Some(entry.created_at);
            continue;
        }
        let Some(stats) = &entry.turn_stats else {
            continue;
        };
        let Some(usage) = stats.usage else {
            // A stamp without usage (the harness reported none) still ends
            // the turn: the next one starts at its own prompt.
            turn_start = None;
            continue;
        };
        let end = entry.created_at + entry.duration_ms.unwrap_or(0).max(0);
        out.push(UsageRecord {
            key: format!("gs:{chat_id}:{}", entry.id),
            source: Source::Godspeed,
            at_ms: end,
            started_ms: Some(turn_start.unwrap_or(entry.created_at).min(end)),
            provider: stats.harness.clone().unwrap_or_else(|| "unknown".into()),
            model: stats.model.clone(),
            session_id: stats.session_id.clone(),
            chat_id: Some(chat_id.to_owned()),
            cwd: None,
            tokens: UsageTokens {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_read: usage.cache_read_tokens.unwrap_or(0),
                cache_write: usage.cache_write_tokens.unwrap_or(0),
            },
        });
        turn_start = None;
    }
    out
}
