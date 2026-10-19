//! Turn receipts: a thermal-printer slip under each finished turn — what ran,
//! for how long, what it touched and (when the harness says) what it cost.
//!
//! Zero model tokens: everything comes from the session doc the transcript
//! already holds — the turn's tool parts, segment timestamps and durations,
//! and the [`zeron_doc::TurnStats`] the host stamps when a turn ends.
//!
//! Layering, all pure except the last:
//! - [`assemble`] folds one turn's entries into a [`Receipt`];
//! - [`lines`] lays a receipt out as [`Line`]s, the ONE layout every surface
//!   shares, so the card, the copied text and the copied image never drift;
//! - [`plain_text`] / [`svg`] / [`summary`] render it;
//! - [`card`] and [`png`] are the GPUI and raster sides.

use std::fmt::Write as _;

use gpui::{
    AnimationExt as _, AnyElement, Hsla, IntoElement, ParentElement, PathBuilder, SharedString,
    Styled, canvas, div, point, prelude::*, px,
};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, TurnUsage};
use zeron_proto::{HarnessId, ToolCall};

use crate::theme::Theme;

/// Plain-text receipt width in columns (also the card's character grid).
pub const WIDTH: usize = 40;
/// Changed files listed by name; the rest fold into "…and N more".
const FILES_SHOWN: usize = 6;

/// Tool calls of a turn, by what they did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounts {
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
            ToolCall::WriteFile { .. } | ToolCall::EditFile { .. } | ToolCall::ApplyPatch { .. } => {
                self.edits += 1
            }
            ToolCall::Search { .. } | ToolCall::Glob { .. } => self.searches += 1,
            ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => self.web += 1,
            // A plan/todo refresh is bookkeeping, not work.
            ToolCall::Todo { .. } => {}
            ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => self.other += 1,
        }
    }
}

/// One changed file. Counts are `None` when only the path is known (an edit
/// tool that reported no diff, on a turn without a checkout capture).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

/// Everything a receipt shows, before account/billing context.
#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    /// The turn's last assistant entry — the row the receipt follows.
    pub anchor_id: String,
    /// Short stable number off the turn id ([`receipt_number`]).
    pub number: String,
    pub finished_at: Option<i64>,
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub wall_ms: Option<i64>,
    pub wait_ms: Option<i64>,
    pub tools: ToolCounts,
    pub files: Vec<FileChange>,
    pub usage: Option<TurnUsage>,
    /// Stopped by the user (or the run died) before it finished.
    pub stopped: bool,
}

/// What the chat row knows, for turns stamped before [`zeron_doc::TurnStats`]
/// existed (and fields a stamp left out).
#[derive(Debug, Clone, Default)]
pub struct Fallback {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

/// `4F2A-91C3`: eight hex digits folded out of the turn id. Stable across
/// devices and app versions (FNV-1a, not the std hasher).
pub fn receipt_number(turn_id: &str) -> String {
    let hash = fnv1a(turn_id.as_bytes());
    let folded = (hash ^ (hash >> 32)) as u32;
    format!("{:04X}-{:04X}", folded >> 16, folded & 0xFFFF)
}

/// Fold one turn — its user prompt (when present) and the assistant entries
/// that answered it — into a receipt. `None` while any of it still streams,
/// or when nothing answered yet.
pub fn assemble(turn: &[SessionMessageEntry], fallback: &Fallback) -> Option<Receipt> {
    let replies: Vec<&SessionMessageEntry> =
        turn.iter().filter(|e| e.role != MessageRole::User).collect();
    let anchor = *replies.last()?;
    if replies
        .iter()
        .any(|e| e.status == Some(MessageStatus::Streaming))
    {
        return None;
    }
    let start = turn.first()?.created_at;
    let end = replies
        .iter()
        .filter_map(|e| e.duration_ms.map(|ms| e.created_at + ms.max(0)))
        .max();

    let mut tools = ToolCounts::default();
    let mut tool_files: Vec<FileChange> = Vec::new();
    let mut note_file = |path: &str, adds: Option<u64>, dels: Option<u64>| {
        match tool_files.iter_mut().find(|f| f.path == path) {
            Some(file) => {
                let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                };
                file.additions = sum(file.additions, adds);
                file.deletions = sum(file.deletions, dels);
            }
            None => tool_files.push(FileChange {
                path: path.to_owned(),
                additions: adds,
                deletions: dels,
            }),
        }
    };
    for part in replies.iter().flat_map(|e| &e.parts) {
        let MessagePart::Tool {
            call,
            diff_stats,
            diff,
            ..
        } = part
        else {
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

    // Stamped stats: the latest stamp names the model; usage and waits add
    // up across stamps (a turn a steer split stamps once per Done).
    let stamps: Vec<&zeron_doc::TurnStats> =
        replies.iter().filter_map(|e| e.turn_stats.as_ref()).collect();
    let latest = |pick: fn(&zeron_doc::TurnStats) -> Option<&String>| {
        stamps.iter().rev().find_map(|s| pick(s)).cloned()
    };
    let usage = stamps
        .iter()
        .filter_map(|s| s.usage)
        .reduce(TurnUsage::plus);
    let wait_ms = stamps
        .iter()
        .filter_map(|s| s.wait_ms)
        .reduce(|a, b| a + b);
    // The checkout capture sees edits shell commands made too; the tools'
    // own stats are the fallback for turns outside a checkout.
    let files = match stamps.iter().rev().find_map(|s| s.files.as_ref()) {
        Some(files) => files
            .iter()
            .map(|f| FileChange {
                path: f.path.clone(),
                additions: Some(f.additions),
                deletions: Some(f.deletions),
            })
            .collect(),
        None => tool_files,
    };
    let harness = latest(|s| s.harness.as_ref())
        .and_then(|h| serde_json::from_value(serde_json::Value::String(h)).ok())
        .or(fallback.harness);
    Some(Receipt {
        anchor_id: anchor.id.clone(),
        number: receipt_number(&turn.first()?.id),
        finished_at: end.or(Some(anchor.created_at)),
        harness,
        model: latest(|s| s.model.as_ref()).or_else(|| fallback.model.clone()),
        effort: latest(|s| s.effort.as_ref()).or_else(|| fallback.effort.clone()),
        wall_ms: end.map(|end| (end - start).max(0)),
        wait_ms,
        tools,
        files,
        usage,
        stopped: anchor.status == Some(MessageStatus::Aborted),
    })
}

/// Cheap change key for a turn's entries: ids, part counts, statuses,
/// durations and stamps — O(entries), never O(text).
pub fn turn_fingerprint(turn: &[SessionMessageEntry]) -> u64 {
    let mut acc = Vec::with_capacity(turn.len() * 32);
    for entry in turn {
        acc.extend_from_slice(entry.id.as_bytes());
        acc.extend_from_slice(&(entry.parts.len() as u32).to_le_bytes());
        acc.push(match entry.status {
            None => 0,
            Some(MessageStatus::Streaming) => 1,
            Some(MessageStatus::Complete) => 2,
            Some(MessageStatus::Aborted) => 3,
        });
        acc.extend_from_slice(&entry.duration_ms.unwrap_or(-1).to_le_bytes());
        if let Some(stats) = &entry.turn_stats {
            acc.extend_from_slice(
                serde_json::to_string(stats)
                    .unwrap_or_default()
                    .as_bytes(),
            );
        }
        // Tool parts resolve (and gain diff stats) in place.
        for part in &entry.parts {
            if let MessagePart::Tool {
                resolved,
                diff_stats,
                ..
            } = part
            {
                acc.push(*resolved as u8);
                acc.push(diff_stats.as_ref().map_or(0, |s| s.len() as u8));
            }
        }
    }
    fnv1a(&acc)
}

// ── pricing ─────────────────────────────────────────────────────────────

/// The shared list-price table ([`zeron_proto::pricing`]).
pub use zeron_proto::pricing::{Price, price_for};

/// Estimated USD for a turn's usage at list prices.
pub fn estimate_cost(model: &str, usage: &TurnUsage) -> Option<f64> {
    zeron_proto::pricing::estimate_cost(
        model,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens.unwrap_or(0),
        usage.cache_write_tokens.unwrap_or(0),
    )
}

// ── layout ──────────────────────────────────────────────────────────────

/// Context the receipt itself doesn't carry: who paid and how.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extras {
    /// The account the harness is signed in with (email or name).
    pub account: Option<String>,
    /// Signed in with an API key — billed per token, so a cost means
    /// something. Subscription (OAuth) accounts never show one.
    pub api_billed: bool,
    /// Pre-formatted finish time ("Oct 17 2026  2:32 PM").
    pub when: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Strong,
}

/// One printed line. The card, the text and the image all render these.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// The speedster mark and the wordmark.
    Masthead,
    Center(String, Tone),
    /// Key flush left, value flush right.
    Pair(String, String, Tone),
    Rule,
    DoubleRule,
    /// Decorative bars seeded by the receipt number.
    Barcode(u64),
}

/// `4m 12s`, `38s`, `1h 02m`.
pub fn format_duration(ms: i64) -> String {
    let secs = (ms.max(0) + 500) / 1000;
    match secs {
        0 => "<1s".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `12,345`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `$1.23`; sub-cent turns read `<$0.01` rather than a scary `$0.00`.
pub fn format_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.005 {
        "<$0.01".into()
    } else {
        format!("${usd:.2}")
    }
}

fn diff_label(adds: Option<u64>, dels: Option<u64>) -> Option<String> {
    match (adds, dels) {
        (None, None) => None,
        (a, d) => Some(format!("+{} -{}", a.unwrap_or(0), d.unwrap_or(0))),
    }
}

/// Keep the END of a path (the file name matters most): `…/ui/src/receipt.rs`.
pub fn truncate_left(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(1);
    let tail: String = text.chars().skip(count - keep).collect();
    format!("…{tail}")
}

fn truncate_right(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A key/value line whose key yields when the pair overflows [`WIDTH`].
fn pair(key: &str, value: impl Into<String>, tone: Tone, path: bool) -> Line {
    let value = truncate_right(&value.into(), WIDTH - 4);
    let room = WIDTH - value.chars().count() - 1;
    let key = if path {
        truncate_left(key, room)
    } else {
        truncate_right(key, room)
    };
    Line::Pair(key, value, tone)
}

/// Totals over files with known counts.
pub fn diff_totals(files: &[FileChange]) -> (Option<u64>, Option<u64>) {
    let known = files.iter().any(|f| f.additions.is_some() || f.deletions.is_some());
    if !known {
        return (None, None);
    }
    (
        Some(files.iter().filter_map(|f| f.additions).sum()),
        Some(files.iter().filter_map(|f| f.deletions).sum()),
    )
}

/// The printed layout. Pure; the same input always prints the same slip.
pub fn lines(receipt: &Receipt, extras: &Extras) -> Vec<Line> {
    let mut out = vec![
        Line::Masthead,
        Line::Center("AGENT RECEIPT".into(), Tone::Muted),
        Line::Rule,
        pair(
            &format!("No. {}", receipt.number),
            extras.when.clone().unwrap_or_default(),
            Tone::Muted,
            false,
        ),
        Line::Rule,
    ];
    if let Some(harness) = receipt.harness {
        out.push(pair("Agent", agent_name(harness), Tone::Normal, false));
    }
    if let Some(model) = &receipt.model {
        out.push(pair("Model", model.clone(), Tone::Normal, false));
    }
    if let Some(effort) = &receipt.effort {
        out.push(pair("Effort", effort.clone(), Tone::Normal, false));
    }
    if let Some(account) = &extras.account {
        out.push(pair("Account", account.clone(), Tone::Normal, false));
    }
    out.push(Line::Rule);
    out.push(pair(
        "Wall time",
        receipt.wall_ms.map_or_else(|| "—".into(), format_duration),
        Tone::Normal,
        false,
    ));
    if let Some(wait) = receipt.wait_ms {
        out.push(pair("Waiting on you", format_duration(wait), Tone::Normal, false));
    }
    if receipt.stopped {
        out.push(pair("Status", "Stopped", Tone::Normal, false));
    }
    out.push(Line::Rule);

    let tools = receipt.tools;
    for (label, n) in [
        ("Commands run", tools.commands),
        ("Files read", tools.reads),
        ("Files edited", tools.edits),
        ("Searches", tools.searches),
        ("Web fetches", tools.web),
        ("Subagents", tools.subagents),
        ("Other tools", tools.other),
    ] {
        if n > 0 {
            out.push(pair(label, n.to_string(), Tone::Normal, false));
        }
    }
    out.push(pair("TOOL CALLS", tools.total().to_string(), Tone::Strong, false));
    out.push(Line::Rule);

    if receipt.files.is_empty() {
        out.push(pair("FILES CHANGED", "0", Tone::Strong, false));
    } else {
        let (adds, dels) = diff_totals(&receipt.files);
        out.push(pair(
            "FILES CHANGED",
            diff_label(adds, dels).unwrap_or_else(|| receipt.files.len().to_string()),
            Tone::Strong,
            false,
        ));
        for file in receipt.files.iter().take(FILES_SHOWN) {
            out.push(pair(
                &file.path,
                diff_label(file.additions, file.deletions).unwrap_or_else(|| "edited".into()),
                Tone::Normal,
                true,
            ));
        }
        let more = receipt.files.len().saturating_sub(FILES_SHOWN);
        if more > 0 {
            let noun = if more == 1 { "file" } else { "files" };
            out.push(Line::Center(format!("…and {more} more {noun}"), Tone::Muted));
        }
    }
    out.push(Line::Rule);

    match &receipt.usage {
        Some(usage) => {
            out.push(pair("Tokens in", format_count(usage.input_tokens), Tone::Normal, false));
            out.push(pair("Tokens out", format_count(usage.output_tokens), Tone::Normal, false));
            if let Some(read) = usage.cache_read_tokens {
                out.push(pair("Cached (read)", format_count(read), Tone::Normal, false));
            }
            if let Some(write) = usage.cache_write_tokens {
                out.push(pair("Cached (write)", format_count(write), Tone::Normal, false));
            }
            let cost = extras
                .api_billed
                .then(|| estimate_cost(receipt.model.as_deref()?, usage))
                .flatten();
            if let Some(cost) = cost {
                out.push(pair("EST. COST (API)", format_cost(cost), Tone::Strong, false));
            }
        }
        None => out.push(pair("Tokens", "not reported", Tone::Muted, false)),
    }
    out.push(Line::DoubleRule);
    out.push(Line::Center("THANK YOU FOR SHIPPING".into(), Tone::Strong));
    out.push(Line::Barcode(fnv1a(receipt.number.as_bytes())));
    out.push(Line::Center(format!("* {} *", receipt.number), Tone::Muted));
    out
}

fn agent_name(harness: HarnessId) -> &'static str {
    crate::shell::harness_updates::agent_name(harness)
}

/// Barcode bar widths (1–3 units), deterministic per seed. Fills ~`units`.
pub fn barcode_bars(seed: u64, units: usize) -> Vec<(usize, bool)> {
    let mut bars = Vec::new();
    let mut state = seed | 1;
    let mut used = 0;
    let mut ink = true;
    while used < units {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let width = (1 + (state % 3) as usize).min(units - used);
        bars.push((width, ink));
        used += width;
        ink = !ink;
    }
    bars
}

fn center(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_owned();
    }
    let left = (width - len) / 2;
    format!("{}{}", " ".repeat(left), text).trim_end().to_owned()
}

/// Monospace text for X/Slack: fixed [`WIDTH`] columns, no trailing spaces.
pub fn plain_text(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        let text = match line {
            Line::Masthead => center("//// G O D S P E E D ////", WIDTH),
            Line::Center(text, _) => center(text, WIDTH),
            Line::Pair(key, value, _) => {
                let pad = WIDTH.saturating_sub(key.chars().count() + value.chars().count()).max(1);
                format!("{key}{}{value}", " ".repeat(pad))
            }
            Line::Rule => "-".repeat(WIDTH),
            Line::DoubleRule => "=".repeat(WIDTH),
            Line::Barcode(seed) => {
                let bars: String = barcode_bars(*seed, WIDTH - 8)
                    .into_iter()
                    .map(|(w, ink)| if ink { "|".repeat(w) } else { " ".repeat(w) })
                    .collect();
                center(&bars, WIDTH)
            }
        };
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// The collapsed one-liner: `Receipt · 4m 12s · 23 tools · +120 -8`.
pub fn summary(receipt: &Receipt) -> String {
    let mut parts = vec!["Receipt".to_owned()];
    if let Some(ms) = receipt.wall_ms {
        parts.push(format_duration(ms));
    }
    let tools = receipt.tools.total();
    parts.push(match tools {
        0 => "no tools".into(),
        1 => "1 tool".into(),
        n => format!("{n} tools"),
    });
    if !receipt.files.is_empty() {
        let (adds, dels) = diff_totals(&receipt.files);
        parts.push(match (adds, dels) {
            (Some(a), Some(d)) => format!("+{a} \u{2212}{d}"),
            _ => match receipt.files.len() {
                1 => "1 file".into(),
                n => format!("{n} files"),
            },
        });
    }
    parts.join(" · ")
}

// ── paper ───────────────────────────────────────────────────────────────

/// Thermal paper colours for the current appearance.
#[derive(Debug, Clone, Copy)]
pub struct Paper {
    pub bg: Hsla,
    pub ink: Hsla,
    pub muted: Hsla,
    pub rule: Hsla,
}

pub fn paper(theme: &Theme) -> Paper {
    if theme.appearance.is_light() {
        let ink = gpui::hsla(30.0 / 360.0, 0.10, 0.15, 1.0);
        Paper {
            bg: gpui::hsla(43.0 / 360.0, 0.50, 0.975, 1.0),
            ink,
            muted: ink.opacity(0.58),
            rule: ink.opacity(0.32),
        }
    } else {
        let ink = gpui::hsla(40.0 / 360.0, 0.14, 0.88, 1.0);
        Paper {
            bg: gpui::hsla(40.0 / 360.0, 0.05, 0.145, 1.0),
            ink,
            muted: ink.opacity(0.55),
            rule: ink.opacity(0.28),
        }
    }
}

fn hex(color: Hsla) -> String {
    let rgba = color.to_rgb();
    let channel = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b)
    )
}

// ── native card ─────────────────────────────────────────────────────────

/// Card geometry, shared by the GPUI card and the SVG export.
pub const CARD_WIDTH: f32 = 332.0;
const CARD_PAD_X: f32 = 20.0;
const CARD_PAD_TOP: f32 = 20.0;
const CARD_PAD_BOTTOM: f32 = 14.0;
const LINE_HEIGHT: f32 = 18.0;
const FONT_SIZE: f32 = 12.0;
const MASTHEAD_HEIGHT: f32 = 46.0;
const RULE_HEIGHT: f32 = 12.0;
const BARCODE_HEIGHT: f32 = 40.0;
/// Depth of the torn edge's teeth, and their spacing.
const TOOTH_DEPTH: f32 = 6.0;
const TOOTH_WIDTH: f32 = 12.0;

fn line_height(line: &Line) -> f32 {
    match line {
        Line::Masthead => MASTHEAD_HEIGHT,
        Line::Rule | Line::DoubleRule => RULE_HEIGHT,
        Line::Barcode(_) => BARCODE_HEIGHT,
        Line::Center(..) | Line::Pair(..) => LINE_HEIGHT,
    }
}

fn tone_color(tone: Tone, paper: &Paper) -> Hsla {
    match tone {
        Tone::Muted => paper.muted,
        Tone::Normal | Tone::Strong => paper.ink,
    }
}

fn dashed_rule(color: Hsla, double: bool) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mid = bounds.origin.y + bounds.size.height / 2.0;
            let rows: &[f32] = if double { &[-1.5, 1.5] } else { &[0.0] };
            for dy in rows {
                let mut x = bounds.origin.x;
                let end = bounds.origin.x + bounds.size.width;
                while x < end {
                    let w = px(4.0).min(end - x);
                    window.paint_quad(gpui::fill(
                        gpui::Bounds::new(point(x, mid + px(*dy) - px(0.5)), gpui::size(w, px(1.0))),
                        color,
                    ));
                    x += px(7.0);
                }
            }
        },
    )
    .w_full()
    .h(px(RULE_HEIGHT))
    .into_any_element()
}

fn barcode(seed: u64, color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let bars = barcode_bars(seed, 96);
            let unit = 2.0;
            let total = 96.0 * unit;
            let mut x = bounds.origin.x + (bounds.size.width - px(total)) / 2.0;
            for (width, ink) in bars {
                let w = px(width as f32 * unit);
                if ink {
                    window.paint_quad(gpui::fill(
                        gpui::Bounds::new(
                            point(x, bounds.origin.y + px(6.0)),
                            gpui::size(w, bounds.size.height - px(10.0)),
                        ),
                        color,
                    ));
                }
                x += w;
            }
        },
    )
    .w_full()
    .h(px(BARCODE_HEIGHT))
    .into_any_element()
}

/// The torn bottom edge: paper-coloured teeth hanging off the card.
fn torn_edge(color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mut path = PathBuilder::fill();
            let top = bounds.origin.y;
            let left = bounds.origin.x;
            let width = f32::from(bounds.size.width);
            let teeth = (width / TOOTH_WIDTH).floor().max(1.0);
            let step = width / teeth;
            path.move_to(point(left, top));
            for i in 0..teeth as usize {
                let x0 = left + px(i as f32 * step);
                path.line_to(point(x0 + px(step / 2.0), top + px(TOOTH_DEPTH)));
                path.line_to(point(x0 + px(step), top));
            }
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w_full()
    .h(px(TOOTH_DEPTH))
    .into_any_element()
}

/// The slip itself. `animate` staggers the lines in like a printer feed
/// (opacity only — the row's height never moves). `actions` sits in the
/// card's top-right corner.
pub fn card(
    id: &SharedString,
    lines: &[Line],
    theme: &Theme,
    animate: bool,
    actions: AnyElement,
) -> AnyElement {
    let paper = paper(theme);
    let mono = theme.font_mono.clone();
    let mut body = div()
        .relative()
        .w(px(CARD_WIDTH))
        .px(px(CARD_PAD_X))
        .pt(px(CARD_PAD_TOP))
        .pb(px(CARD_PAD_BOTTOM))
        .bg(paper.bg)
        .rounded_t(px(4.0))
        .font_family(mono)
        .text_size(px(FONT_SIZE))
        .line_height(px(LINE_HEIGHT))
        .text_color(paper.ink)
        .flex()
        .flex_col();
    for (ix, line) in lines.iter().enumerate() {
        let el: AnyElement = match line {
            Line::Masthead => div()
                .h(px(MASTHEAD_HEIGHT))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .child(
                    crate::icons::icon(crate::icons::GODSPEED_LOGO)
                        .size(px(20.0))
                        .text_color(paper.ink),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("G O D S P E E D"),
                )
                .into_any_element(),
            Line::Center(text, tone) => div()
                .w_full()
                .flex()
                .justify_center()
                .text_color(tone_color(*tone, &paper))
                .when(*tone == Tone::Strong, |el| el.font_weight(gpui::FontWeight::BOLD))
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            Line::Pair(key, value, tone) => div()
                .w_full()
                .flex()
                .justify_between()
                .gap(px(8.0))
                .text_color(tone_color(*tone, &paper))
                .when(*tone == Tone::Strong, |el| el.font_weight(gpui::FontWeight::BOLD))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(SharedString::from(key.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .child(SharedString::from(value.clone())),
                )
                .into_any_element(),
            Line::Rule => dashed_rule(paper.rule, false),
            Line::DoubleRule => dashed_rule(paper.rule, true),
            Line::Barcode(seed) => barcode(*seed, paper.ink),
        };
        let el = if animate {
            let spec = crate::motion::MotionSpec::new(200, crate::motion::EASE_OUT)
                .with_delay(ix as u64 * 22);
            div()
                .w_full()
                .child(el)
                .with_animation(
                    SharedString::from(format!("{id}-print-{ix}")),
                    spec.animation(),
                    |el, t| el.opacity(t),
                )
                .into_any_element()
        } else {
            el
        };
        body = body.child(el);
    }
    let body = body.child(div().absolute().top(px(8.0)).right(px(8.0)).child(actions));
    div()
        .w(px(CARD_WIDTH))
        .flex()
        .flex_col()
        .child(body)
        .child(torn_edge(paper.bg))
        .into_any_element()
}

// ── image export ────────────────────────────────────────────────────────

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Inner shapes of the bundled speedster mark (the SVG's children).
fn logo_shapes() -> &'static str {
    const LOGO: &str = include_str!("../assets/icons/godspeed-logo.svg");
    let start = LOGO.find('>').map_or(0, |ix| ix + 1);
    let end = LOGO.rfind("</svg>").unwrap_or(LOGO.len());
    &LOGO[start..end]
}

/// The slip as SVG, laid out exactly like [`card`] (same line heights and
/// paddings), with a transparent torn edge.
pub fn svg(lines: &[Line], paper: &Paper) -> String {
    let body: f32 = CARD_PAD_TOP + CARD_PAD_BOTTOM + lines.iter().map(line_height).sum::<f32>();
    let height = body + TOOTH_DEPTH;
    let w = CARD_WIDTH;
    let (ink, muted, rule, bg) = (hex(paper.ink), hex(paper.muted), hex(paper.rule), hex(paper.bg));
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{height}" viewBox="0 0 {w} {height}">"#
    );
    // Paper with its torn edge as one shape.
    let teeth = (w / TOOTH_WIDTH).floor().max(1.0);
    let step = w / teeth;
    let mut d = format!("M0 0 H{w} V{body}");
    for i in (0..teeth as usize).rev() {
        let x0 = i as f32 * step;
        let _ = write!(d, " L{} {} L{} {}", x0 + step / 2.0, body + TOOTH_DEPTH, x0, body);
    }
    d.push_str(" Z");
    let _ = write!(out, r#"<path d="{d}" fill="{bg}"/>"#);
    let _ = write!(
        out,
        r#"<g font-family="Geist Mono, monospace" font-size="{FONT_SIZE}" fill="{ink}">"#
    );
    let mut y = CARD_PAD_TOP;
    let (left, right, mid) = (CARD_PAD_X, w - CARD_PAD_X, w / 2.0);
    // Text baseline inside an 18px line box.
    let baseline = |y: f32| y + LINE_HEIGHT / 2.0 + FONT_SIZE * 0.36;
    for line in lines {
        let h = line_height(line);
        match line {
            Line::Masthead => {
                let size = 20.0;
                let _ = write!(
                    out,
                    r#"<svg x="{}" y="{}" width="{size}" height="{size}" viewBox="0 0 820 940" fill="{ink}">{}</svg>"#,
                    mid - size / 2.0,
                    y + 4.0,
                    logo_shapes()
                );
                let _ = write!(
                    out,
                    r#"<text x="{mid}" y="{}" text-anchor="middle" font-size="11" font-weight="bold">G O D S P E E D</text>"#,
                    y + 40.0
                );
            }
            Line::Center(text, tone) => {
                let fill = if *tone == Tone::Muted { &muted } else { &ink };
                let weight = if *tone == Tone::Strong { r#" font-weight="bold""# } else { "" };
                let _ = write!(
                    out,
                    r#"<text x="{mid}" y="{}" text-anchor="middle" fill="{fill}"{weight}>{}</text>"#,
                    baseline(y),
                    xml_escape(text)
                );
            }
            Line::Pair(key, value, tone) => {
                let fill = if *tone == Tone::Muted { &muted } else { &ink };
                let weight = if *tone == Tone::Strong { r#" font-weight="bold""# } else { "" };
                let _ = write!(
                    out,
                    r#"<text x="{left}" y="{b}" fill="{fill}"{weight}>{}</text><text x="{right}" y="{b}" text-anchor="end" fill="{fill}"{weight}>{}</text>"#,
                    xml_escape(key),
                    xml_escape(value),
                    b = baseline(y),
                );
            }
            Line::Rule | Line::DoubleRule => {
                let rows: &[f32] = if matches!(line, Line::DoubleRule) { &[-1.5, 1.5] } else { &[0.0] };
                for dy in rows {
                    let ly = y + h / 2.0 + dy;
                    let _ = write!(
                        out,
                        r#"<line x1="{left}" y1="{ly}" x2="{right}" y2="{ly}" stroke="{rule}" stroke-width="1" stroke-dasharray="4 3"/>"#
                    );
                }
            }
            Line::Barcode(seed) => {
                let unit = 2.0;
                let mut x = mid - 96.0 * unit / 2.0;
                for (width, ink_bar) in barcode_bars(*seed, 96) {
                    let bw = width as f32 * unit;
                    if ink_bar {
                        let _ = write!(
                            out,
                            r#"<rect x="{x}" y="{}" width="{bw}" height="{}"/>"#,
                            y + 6.0,
                            h - 10.0
                        );
                    }
                    x += bw;
                }
            }
        }
        y += h;
    }
    out.push_str("</g></svg>");
    out
}

/// Rasterize [`svg`] to PNG bytes at `scale`x (2 for retina pastes).
pub fn png(svg: &str, scale: f32) -> Result<Vec<u8>, String> {
    use resvg::tiny_skia;
    let tree = resvg::usvg::Tree::from_str(svg, &crate::image_media::svg_options())
        .map_err(|e| e.to_string())?;
    let size = tree.size();
    let (w, h) = (
        (size.width() * scale).ceil() as u32,
        (size.height() * scale).ceil() as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("empty receipt image")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    let image = image::RgbaImage::from_raw(w, h, rgba).ok_or("receipt raster size mismatch")?;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(bytes.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::{ToolDiffStat, TurnStats};

    fn entry(id: &str, role: MessageRole, at: i64, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: at,
            device_id: "dev".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
            turn_stats: None,
        }
    }

    fn tool(id: &str, call: ToolCall) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call,
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    fn with_stats(mut part: MessagePart, path: &str, adds: u64, dels: u64) -> MessagePart {
        if let MessagePart::Tool { diff_stats, .. } = &mut part {
            *diff_stats = Some(vec![ToolDiffStat {
                path: path.into(),
                additions: adds,
                deletions: dels,
            }]);
        }
        part
    }

    /// A recorded turn: prompt at t=0, the reply segment starting 2 s later
    /// and running 250 s, with a spread of tools.
    fn recorded_turn() -> Vec<SessionMessageEntry> {
        let user = entry("user-1", MessageRole::User, 1_000_000, vec![MessagePart::Text {
            id: "t0".into(),
            text: "fix it".into(),
        }]);
        let mut reply = entry(
            "reply-1",
            MessageRole::Assistant,
            1_002_000,
            vec![
                tool("c1", ToolCall::Exec { command: "cargo test".into() }),
                tool("c2", ToolCall::Exec { command: "ls".into() }),
                tool("r1", ToolCall::ReadFile { path: "src/a.rs".into() }),
                with_stats(
                    tool("e1", ToolCall::EditFile {
                        path: "src/a.rs".into(),
                        old_string: None,
                        new_string: None,
                    }),
                    "src/a.rs",
                    10,
                    2,
                ),
                with_stats(
                    tool("e2", ToolCall::EditFile {
                        path: "src/a.rs".into(),
                        old_string: None,
                        new_string: None,
                    }),
                    "src/a.rs",
                    3,
                    1,
                ),
                tool("w1", ToolCall::WriteFile { path: "src/new.rs".into(), content: None }),
                tool("s1", ToolCall::Search { pattern: "fn".into(), path: None }),
                tool("g1", ToolCall::Glob { pattern: "*.rs".into() }),
                tool("f1", ToolCall::WebFetch { url: "https://x.dev".into(), prompt: None }),
                tool("a1", ToolCall::Unknown { name: "Agent: explore".into(), input: None }),
                tool("t1", ToolCall::Todo { items: vec![] }),
                tool("m1", ToolCall::Mcp { server: "gs".into(), tool: "list_chats".into(), input: None }),
                MessagePart::Text { id: "x".into(), text: "Done.".into() },
            ],
        );
        reply.duration_ms = Some(250_000);
        vec![user, reply]
    }

    #[test]
    fn assembles_counts_durations_and_tool_diffs_from_a_recorded_turn() {
        let receipt = assemble(&recorded_turn(), &Fallback::default()).unwrap();
        assert_eq!(
            receipt.tools,
            ToolCounts {
                commands: 2,
                reads: 1,
                edits: 3,
                searches: 2,
                web: 1,
                subagents: 1,
                other: 1,
            }
        );
        assert_eq!(receipt.tools.total(), 11, "todo refreshes aren't tools");
        // Prompt sent at 1_000_000, reply ended at 1_002_000 + 250_000.
        assert_eq!(receipt.wall_ms, Some(252_000));
        assert_eq!(receipt.finished_at, Some(1_252_000));
        assert_eq!(receipt.wait_ms, None);
        assert_eq!(
            receipt.files,
            vec![
                FileChange { path: "src/a.rs".into(), additions: Some(13), deletions: Some(3) },
                FileChange { path: "src/new.rs".into(), additions: None, deletions: None },
            ]
        );
        assert_eq!(diff_totals(&receipt.files), (Some(13), Some(3)));
        assert_eq!(receipt.usage, None);
        assert_eq!(receipt.anchor_id, "reply-1");
        assert_eq!(summary(&receipt), "Receipt · 4m 12s · 11 tools · +13 \u{2212}3");
    }

    #[test]
    fn stamped_stats_win_and_add_up_across_segments() {
        let mut turn = recorded_turn();
        turn[1].turn_stats = Some(TurnStats {
            harness: Some("codex".into()),
            model: Some("gpt-5.5".into()),
            usage: Some(TurnUsage {
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
            wait_ms: Some(3_000),
            ..Default::default()
        });
        let mut second = entry("reply-2", MessageRole::Assistant, 1_300_000, vec![]);
        second.duration_ms = Some(10_000);
        second.turn_stats = Some(TurnStats {
            effort: Some("xhigh".into()),
            usage: Some(TurnUsage {
                input_tokens: 5,
                output_tokens: 1,
                cache_read_tokens: Some(900),
                cache_write_tokens: None,
            }),
            wait_ms: Some(1_000),
            files: Some(vec![ToolDiffStat {
                path: "README.md".into(),
                additions: 4,
                deletions: 0,
            }]),
            ..Default::default()
        });
        turn.push(second);
        let fallback = Fallback {
            harness: Some(HarnessId::ClaudeCode),
            model: Some("ignored".into()),
            effort: None,
        };
        let receipt = assemble(&turn, &fallback).unwrap();
        assert_eq!(receipt.harness, Some(HarnessId::Codex));
        assert_eq!(receipt.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(receipt.effort.as_deref(), Some("xhigh"));
        assert_eq!(receipt.wait_ms, Some(4_000));
        assert_eq!(
            receipt.usage,
            Some(TurnUsage {
                input_tokens: 105,
                output_tokens: 11,
                cache_read_tokens: Some(900),
                cache_write_tokens: None,
            })
        );
        // The checkout capture replaces the tools' own stats.
        assert_eq!(receipt.files.len(), 1);
        assert_eq!(receipt.files[0].path, "README.md");
        assert_eq!(receipt.wall_ms, Some(310_000));
        assert_eq!(receipt.anchor_id, "reply-2");
    }

    #[test]
    fn no_receipt_while_streaming_or_before_any_reply() {
        let mut turn = recorded_turn();
        assert!(assemble(&turn[..1], &Fallback::default()).is_none());
        turn[1].status = Some(MessageStatus::Streaming);
        assert!(assemble(&turn, &Fallback::default()).is_none());
        turn[1].status = Some(MessageStatus::Aborted);
        assert!(assemble(&turn, &Fallback::default()).unwrap().stopped);
    }

    #[test]
    fn receipt_number_is_stable_and_short() {
        let number = receipt_number("user-1");
        assert_eq!(number, receipt_number("user-1"));
        assert_ne!(number, receipt_number("user-2"));
        assert_eq!(number.len(), 9);
        assert!(number.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        // Pinned: a receipt number never changes between app versions.
        assert_eq!(receipt_number(""), "4FD0-BFC1");
        let receipt = assemble(&recorded_turn(), &Fallback::default()).unwrap();
        assert_eq!(receipt.number, receipt_number("user-1"));
    }

    #[test]
    fn cost_only_for_api_billed_accounts_on_known_models() {
        let usage = TurnUsage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: Some(2_000_000),
            cache_write_tokens: Some(100_000),
        };
        // $4 + $2 + $0.40 + $0.50.
        let cost = estimate_cost("claude-opus-5-5", &usage).unwrap();
        assert!((cost - 6.9).abs() < 1e-9, "{cost}");
        assert_eq!(price_for("claude-opus-5-5[1m]"), price_for("claude-opus-5-5"));
        assert_eq!(price_for("us.anthropic.claude-opus-4-8"), price_for("claude-opus-4-8"));
        assert_eq!(price_for("claude-haiku-4-5-20251001"), price_for("claude-haiku-4-5"));
        assert_ne!(price_for("claude-opus-5-5"), price_for("claude-opus-5"));
        assert_eq!(price_for("gpt-5.6-sol"), None);
        assert_eq!(price_for("opus"), None);

        let mut receipt = assemble(&recorded_turn(), &Fallback::default()).unwrap();
        receipt.model = Some("claude-opus-5-5".into());
        receipt.usage = Some(usage);
        let has_cost = |receipt: &Receipt, extras: &Extras| {
            lines(receipt, extras)
                .iter()
                .any(|l| matches!(l, Line::Pair(k, _, _) if k.starts_with("EST. COST")))
        };
        let api = Extras { api_billed: true, ..Default::default() };
        assert!(has_cost(&receipt, &api));
        assert!(!has_cost(&receipt, &Extras::default()), "subscription: no cost");
        receipt.model = Some("gpt-5.6-terra".into());
        assert!(!has_cost(&receipt, &api), "unknown price");
    }

    #[test]
    fn plain_text_has_a_stable_fixed_width_layout() {
        let mut receipt = assemble(&recorded_turn(), &Fallback::default()).unwrap();
        receipt.harness = Some(HarnessId::ClaudeCode);
        receipt.model = Some("claude-opus-5-5".into());
        receipt.effort = Some("high".into());
        receipt.wait_ms = Some(38_000);
        receipt.usage = Some(TurnUsage {
            input_tokens: 12_345,
            output_tokens: 4_210,
            cache_read_tokens: Some(480_000),
            cache_write_tokens: Some(1_200),
        });
        receipt.files.push(FileChange {
            path: "crates/ui/src/some/really/deep/folder/receipt_renderer.rs".into(),
            additions: Some(120),
            deletions: Some(8),
        });
        let extras = Extras {
            account: Some("me@example.com".into()),
            api_billed: true,
            when: Some("Oct 17 2026  2:32 PM".into()),
        };
        let text = plain_text(&lines(&receipt, &extras));
        for line in text.lines() {
            assert!(line.chars().count() <= WIDTH, "too wide: {line:?}");
            assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
        }
        assert_eq!(receipt.number, "79DC-4DF4");
        let expected = [
            "       //// G O D S P E E D ////",
            "             AGENT RECEIPT",
            "----------------------------------------",
            "No. 79DC-4DF4       Oct 17 2026  2:32 PM",
            "----------------------------------------",
            "Agent                        Claude Code",
            "Model                    claude-opus-5-5",
            "Effort                              high",
            "Account                   me@example.com",
            "----------------------------------------",
            "Wall time                         4m 12s",
            "Waiting on you                       38s",
            "----------------------------------------",
            "Commands run                           2",
            "Files read                             1",
            "Files edited                           3",
            "Searches                               2",
            "Web fetches                            1",
            "Subagents                              1",
            "Other tools                            1",
            "TOOL CALLS                            11",
            "----------------------------------------",
            "FILES CHANGED                   +133 -11",
            "src/a.rs                          +13 -3",
            "src/new.rs                        edited",
            "…deep/folder/receipt_renderer.rs +120 -8",
            "----------------------------------------",
            "Tokens in                         12,345",
            "Tokens out                         4,210",
            "Cached (read)                    480,000",
            "Cached (write)                     1,200",
            "EST. COST (API)                    $0.24",
            "========================================",
            "         THANK YOU FOR SHIPPING",
        ]
        .map(|line| format!("{line}\n"))
        .concat();
        assert!(text.starts_with(&expected), "got:\n{text}");
        // Barcode and number close the slip.
        let tail: Vec<&str> = text.lines().rev().take(2).collect();
        assert_eq!(tail[0].trim(), format!("* {} *", receipt.number));
        assert!(tail[1].contains('|'));
        // Same input, same slip.
        assert_eq!(text, plain_text(&lines(&receipt, &extras)));
    }

    #[test]
    fn unreported_usage_says_so() {
        let receipt = assemble(&recorded_turn(), &Fallback::default()).unwrap();
        let text = plain_text(&lines(&receipt, &Extras::default()));
        assert!(text.contains("Tokens                      not reported"), "{text}");
        assert!(!text.contains("EST. COST"));
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_duration(0), "<1s");
        assert_eq!(format_duration(38_400), "38s");
        assert_eq!(format_duration(252_000), "4m 12s");
        assert_eq!(format_duration(3_720_000), "1h 02m");
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_cost(0.001), "<$0.01");
        assert_eq!(format_cost(1.234), "$1.23");
        assert_eq!(truncate_left("abcdef", 4), "…def");
    }

    #[test]
    fn svg_export_renders_to_a_png() {
        let receipt = assemble(&recorded_turn(), &Fallback::default()).unwrap();
        let lines = lines(&receipt, &Extras::default());
        let paper = paper(&Theme::default());
        let svg = svg(&lines, &paper);
        assert!(svg.contains("THANK YOU FOR SHIPPING"));
        let bytes = png(&svg, 2.0).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), (CARD_WIDTH * 2.0).ceil() as u32);
        // The torn edge's notches are transparent; the paper is opaque.
        let rgba = decoded.to_rgba8();
        assert_eq!(rgba.get_pixel(rgba.width() / 2, 10)[3], 255);
        let bottom = rgba.height() - 1;
        assert_eq!(rgba.get_pixel(0, bottom)[3], 0);
    }
}
