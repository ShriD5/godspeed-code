//! Turn receipts on the phone: the desktop's receipt (`crates/ui/src/receipt.rs`)
//! as a compact line under each finished turn that expands into its details.
//!
//! The assembly mirrors the desktop's exactly (same sums, same fallbacks, same
//! one-liner) so a turn reads the same on both; only the paper slip is left
//! out. It lives here, not in `zeron-ui`, because that crate is GPUI-bound.
//! TODO: move both copies into `zeron-doc` once the desktop can take the churn.

use std::sync::Arc;

use zeron_doc::parts::{MessagePart, MessageStatus};
use zeron_doc::schema::{MessageRole, SessionMessageEntry};
use zeron_doc::{TurnStats, TurnUsage};
use zeron_proto::ToolCall;

/// Changed files listed by name; the rest fold into "…and N more".
const FILES_SHOWN: usize = 6;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ToolCounts {
    pub commands: u32,
    pub reads: u32,
    pub edits: u32,
    pub searches: u32,
    pub web: u32,
    pub subagents: u32,
    pub other: u32,
}

impl ToolCounts {
    pub fn total(&self) -> u32 {
        self.commands + self.reads + self.edits + self.searches + self.web + self.subagents + self.other
    }

    fn count(&mut self, call: &ToolCall) {
        if call.is_subagent_spawn() {
            self.subagents += 1;
            return;
        }
        match call {
            ToolCall::Exec { .. } => self.commands += 1,
            ToolCall::ReadFile { .. } => self.reads += 1,
            ToolCall::WriteFile { .. } | ToolCall::EditFile { .. } | ToolCall::ApplyPatch { .. } => self.edits += 1,
            ToolCall::Search { .. } | ToolCall::Glob { .. } => self.searches += 1,
            ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => self.web += 1,
            // A plan/todo refresh is bookkeeping, not work.
            ToolCall::Todo { .. } => {}
            ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => self.other += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileChange {
    pub path: String,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Receipt {
    /// The turn's last assistant entry — the row the receipt follows.
    pub anchor_id: String,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub wall_ms: Option<i64>,
    pub wait_ms: Option<i64>,
    pub tools: ToolCounts,
    pub files: Vec<FileChange>,
    pub usage: Option<TurnUsage>,
    pub stopped: bool,
}

/// Fold one turn (its prompt, when present, and the replies) into a receipt.
/// `None` while anything streams or nothing answered yet — and, unlike the
/// desktop, for turns without a stamp at all (chats from before receipts),
/// so old transcripts don't sprout a line under every reply.
pub(crate) fn assemble(turn: &[Arc<SessionMessageEntry>]) -> Option<Receipt> {
    let replies: Vec<&SessionMessageEntry> = turn.iter().map(|e| &**e).filter(|e| e.role != MessageRole::User).collect();
    let anchor = *replies.last()?;
    if replies.iter().any(|e| e.status == Some(MessageStatus::Streaming)) {
        return None;
    }
    let stamps: Vec<&TurnStats> = replies.iter().filter_map(|e| e.turn_stats.as_ref()).collect();
    if stamps.is_empty() {
        return None;
    }
    let start = turn.first()?.created_at;
    let end = replies.iter().filter_map(|e| e.duration_ms.map(|ms| e.created_at + ms.max(0))).max();

    let mut tools = ToolCounts::default();
    let mut tool_files: Vec<FileChange> = Vec::new();
    let mut note_file = |path: &str, adds: Option<u64>, dels: Option<u64>| match tool_files.iter_mut().find(|f| f.path == path) {
        Some(file) => {
            let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
            };
            file.additions = sum(file.additions, adds);
            file.deletions = sum(file.deletions, dels);
        }
        None => tool_files.push(FileChange { path: path.to_owned(), additions: adds, deletions: dels }),
    };
    for part in replies.iter().flat_map(|e| &e.parts) {
        let MessagePart::Tool { call, diff_stats, diff, .. } = part else {
            continue;
        };
        tools.count(call);
        if let Some(stats) = diff_stats {
            for stat in stats {
                note_file(&stat.path, Some(stat.additions), Some(stat.deletions));
            }
        } else if let Some(diff) = diff {
            let stat = zeron_doc::diff_stat(diff);
            note_file(&stat.path, Some(stat.additions), Some(stat.deletions));
        } else {
            let path = match call {
                ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => Some(path),
                ToolCall::ApplyPatch { path } => path.as_ref(),
                _ => None,
            };
            if let Some(path) = path {
                note_file(path, None, None);
            }
        }
    }
    let latest = |pick: fn(&TurnStats) -> Option<&String>| stamps.iter().rev().find_map(|s| pick(s)).cloned();
    let usage = stamps.iter().filter_map(|s| s.usage).reduce(TurnUsage::plus);
    let wait_ms = stamps.iter().filter_map(|s| s.wait_ms).reduce(|a, b| a + b);
    let files = match stamps.iter().rev().find_map(|s| s.files.as_ref()) {
        Some(files) => files
            .iter()
            .map(|f| FileChange { path: f.path.clone(), additions: Some(f.additions), deletions: Some(f.deletions) })
            .collect(),
        None => tool_files,
    };
    Some(Receipt {
        anchor_id: anchor.id.clone(),
        harness: latest(|s| s.harness.as_ref()),
        model: latest(|s| s.model.as_ref()),
        effort: latest(|s| s.effort.as_ref()),
        wall_ms: end.map(|end| (end - start).max(0)),
        wait_ms,
        tools,
        files,
        usage,
        stopped: anchor.status == Some(MessageStatus::Aborted),
    })
}

/// Cheap change key for a turn: ids, part counts, statuses, durations,
/// stamps and tool resolution — O(entries), never O(text).
pub(crate) fn fingerprint(turn: &[Arc<SessionMessageEntry>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for entry in turn {
        eat(entry.id.as_bytes());
        eat(&(entry.parts.len() as u32).to_le_bytes());
        eat(&[match entry.status {
            None => 0,
            Some(MessageStatus::Streaming) => 1,
            Some(MessageStatus::Complete) => 2,
            Some(MessageStatus::Aborted) => 3,
        }]);
        eat(&entry.duration_ms.unwrap_or(-1).to_le_bytes());
        if let Some(stats) = &entry.turn_stats {
            eat(serde_json::to_string(stats).unwrap_or_default().as_bytes());
        }
        for part in &entry.parts {
            if let MessagePart::Tool { resolved, diff_stats, .. } = part {
                eat(&[*resolved as u8, diff_stats.as_ref().map_or(0, |s| s.len() as u8)]);
            }
        }
    }
    h
}

/// `4m 12s`, `38s`, `1h 02m`.
pub(crate) fn format_duration(ms: i64) -> String {
    let secs = (ms.max(0) + 500) / 1000;
    match secs {
        0 => "<1s".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `12,345`.
pub(crate) fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn diff_totals(files: &[FileChange]) -> (Option<u64>, Option<u64>) {
    if !files.iter().any(|f| f.additions.is_some() || f.deletions.is_some()) {
        return (None, None);
    }
    (Some(files.iter().filter_map(|f| f.additions).sum()), Some(files.iter().filter_map(|f| f.deletions).sum()))
}

/// The collapsed one-liner: `Receipt · 4m 12s · 23 tools · +120 −8`.
pub(crate) fn summary(receipt: &Receipt) -> String {
    let mut parts = vec!["Receipt".to_owned()];
    if let Some(ms) = receipt.wall_ms {
        parts.push(format_duration(ms));
    }
    parts.push(match receipt.tools.total() {
        0 => "no tools".into(),
        1 => "1 tool".into(),
        n => format!("{n} tools"),
    });
    if !receipt.files.is_empty() {
        parts.push(match diff_totals(&receipt.files) {
            (Some(a), Some(d)) => format!("+{a} \u{2212}{d}"),
            _ => match receipt.files.len() {
                1 => "1 file".into(),
                n => format!("{n} files"),
            },
        });
    }
    parts.join(" · ")
}

/// The expanded details, key → value, in the desktop slip's order.
pub(crate) fn details(receipt: &Receipt) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut pair = |k: &str, v: String| out.push((k.to_owned(), v));
    if let Some(h) = &receipt.harness {
        pair("Agent", zeron_client::catalog::harness_label(h));
    }
    if let Some(m) = &receipt.model {
        let label = match &receipt.harness {
            Some(h) => zeron_client::catalog::model_label(h, m),
            None => m.clone(),
        };
        pair("Model", label);
    }
    if let Some(e) = &receipt.effort {
        pair("Effort", zeron_client::catalog::reasoning_label(e));
    }
    if let Some(ms) = receipt.wall_ms {
        pair("Wall time", format_duration(ms));
    }
    if let Some(ms) = receipt.wait_ms.filter(|ms| *ms > 0) {
        pair("Waiting on you", format_duration(ms));
    }
    if receipt.stopped {
        pair("Stopped", "Before it finished".into());
    }
    let t = receipt.tools;
    for (label, n) in [
        ("Commands", t.commands),
        ("Reads", t.reads),
        ("Edits", t.edits),
        ("Searches", t.searches),
        ("Web", t.web),
        ("Subagents", t.subagents),
        ("Other tools", t.other),
    ] {
        if n > 0 {
            pair(label, n.to_string());
        }
    }
    if !receipt.files.is_empty() {
        let total = match diff_totals(&receipt.files) {
            (Some(a), Some(d)) => format!("+{a} \u{2212}{d}"),
            _ => receipt.files.len().to_string(),
        };
        pair("Files changed", total);
        for f in receipt.files.iter().take(FILES_SHOWN) {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_owned();
            let value = match (f.additions, f.deletions) {
                (None, None) => String::new(),
                (a, d) => format!("+{} \u{2212}{}", a.unwrap_or(0), d.unwrap_or(0)),
            };
            out.push((format!("  {name}"), value));
        }
        if receipt.files.len() > FILES_SHOWN {
            out.push((format!("  …and {} more", receipt.files.len() - FILES_SHOWN), String::new()));
        }
    }
    match receipt.usage {
        Some(u) => {
            out.push(("Tokens in".into(), format_count(u.input_tokens)));
            out.push(("Tokens out".into(), format_count(u.output_tokens)));
            if let Some(n) = u.cache_read_tokens.filter(|n| *n > 0) {
                out.push(("Cache read".into(), format_count(n)));
            }
            if let Some(n) = u.cache_write_tokens.filter(|n| *n > 0) {
                out.push(("Cache write".into(), format_count(n)));
            }
        }
        None => out.push(("Tokens".into(), "Not reported".into())),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::parts::ToolDiffStat;

    fn entry(id: &str, role: MessageRole, at_s: i64, duration_ms: Option<i64>, stats: Option<TurnStats>, parts: Vec<MessagePart>) -> Arc<SessionMessageEntry> {
        Arc::new(SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 1_790_000_000_000 + at_s * 1000,
            device_id: "mac".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms,
            turn_stats: stats,
        })
    }

    #[test]
    fn stamped_turn_reads_like_the_desktop() {
        let stats = TurnStats {
            harness: Some("claude-code".into()),
            model: Some("claude-fable-5".into()),
            effort: Some("high".into()),
            usage: Some(TurnUsage { input_tokens: 1200, output_tokens: 3400, cache_read_tokens: Some(50_000), cache_write_tokens: None }),
            wait_ms: Some(12_000),
            files: Some(vec![
                ToolDiffStat { path: "src/a.rs".into(), additions: 100, deletions: 5 },
                ToolDiffStat { path: "src/b.rs".into(), additions: 20, deletions: 3 },
            ]),
            session_id: None,
        };
        let turn = vec![
            entry("u1", MessageRole::User, 0, None, None, vec![]),
            entry("a1", MessageRole::Assistant, 1, Some(251_000), Some(stats), vec![]),
        ];
        let r = assemble(&turn).expect("receipt");
        assert_eq!(r.anchor_id, "a1");
        assert_eq!(summary(&r), "Receipt · 4m 12s · no tools · +120 \u{2212}8");
        let d = details(&r);
        assert!(d.contains(&("Wall time".into(), "4m 12s".into())));
        assert!(d.contains(&("Waiting on you".into(), "12s".into())));
        assert!(d.contains(&("Tokens out".into(), "3,400".into())));
        assert!(d.contains(&("Cache read".into(), "50,000".into())));
        assert!(d.iter().any(|(k, v)| k == "  a.rs" && v == "+100 \u{2212}5"));
    }

    #[test]
    fn unstamped_or_streaming_turns_get_no_receipt() {
        let turn = vec![entry("u1", MessageRole::User, 0, None, None, vec![]), entry("a1", MessageRole::Assistant, 1, Some(1000), None, vec![])];
        assert!(assemble(&turn).is_none());
        let mut streaming = (*entry("a2", MessageRole::Assistant, 1, None, Some(TurnStats::default()), vec![])).clone();
        streaming.status = Some(MessageStatus::Streaming);
        assert!(assemble(&[Arc::new(streaming)]).is_none());
    }

    #[test]
    fn durations_and_counts_format() {
        assert_eq!(format_duration(0), "<1s");
        assert_eq!(format_duration(38_000), "38s");
        assert_eq!(format_duration(3_720_000), "1h 02m");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }
}
