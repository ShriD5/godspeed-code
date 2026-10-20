//! Find in thread, plus scrolling to a message a search result names.
//!
//! Matches are resolved over the transcript's whole row model — every row of
//! the virtualized list, painted or not — from the same text the painted
//! elements carry (user bubbles' shown text; markdown blocks' inline runs,
//! code blocks per line), so the count, the navigation and the washes agree.
//! A row's count is cached by `(row id, version)`: a streaming reply
//! recounts its changed tail rows only. Painting lives in
//! `markdown::find` / `markdown::render`.
//!
//! Rows a closed compact-work fold has unmounted are not searched, and a
//! match inside a collapsed long user bubble scrolls to the bubble only.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{Context, ListOffset, SharedString, px};

use crate::markdown::find::{FindHighlight, find_ranges};
use crate::markdown::parser::Block;
use crate::theme::Theme;
use crate::transcript::{OWN_SEND_TOP_INSET_PX, Row, RowKind, Transcript};

/// Room the find bar takes under the titlebar when landing on a match.
const FIND_BAR_CLEARANCE_PX: f32 = 44.0;
/// A reveal whose message never shows up is dropped after this long.
const REVEAL_PATIENCE: Duration = Duration::from_secs(15);

/// One match: the row it sits in and its index among that row's matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FindMatch {
    pub row: usize,
    pub occurrence: usize,
}

pub(crate) struct FindState {
    pub needle: String,
    pub case_sensitive: bool,
    pub matches: Vec<FindMatch>,
    pub current: Option<usize>,
    /// Per row id: the version counted and its match count.
    counts: HashMap<SharedString, (u64, usize)>,
}

pub(crate) struct PendingReveal {
    chat_id: String,
    message_id: String,
    since: Instant,
}

/// The searchable segments of one row, one per painted text element.
pub(crate) fn row_segments(row: &Row) -> Vec<String> {
    let mut out = Vec::new();
    match &row.kind {
        RowKind::User { text, .. } => out.push(text.to_string()),
        RowKind::Markdown { tree, block_ix } | RowKind::LiveMarkdown { tree, block_ix } => {
            if let Some(top) = tree.blocks.get(*block_ix) {
                block_segments(&top.block, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn runs_text(runs: &[crate::markdown::InlineRun]) -> String {
    runs.iter().map(|run| run.text.as_str()).collect()
}

fn block_segments(block: &Block, out: &mut Vec<String>) {
    match block {
        Block::Paragraph { runs } | Block::Heading { runs, .. } => out.push(runs_text(runs)),
        Block::CodeBlock { code, .. } => out.extend(code.lines().map(str::to_owned)),
        Block::Math { tex } => out.extend(tex.lines().map(str::to_owned)),
        Block::BlockQuote { children } => {
            for child in children {
                block_segments(child, out);
            }
        }
        Block::List { items, .. } => {
            for child in items.iter().flatten() {
                block_segments(child, out);
            }
        }
        Block::Table { header, rows, .. } => {
            out.extend(header.iter().map(|cell| runs_text(cell)));
            for row in rows {
                out.extend(row.iter().map(|cell| runs_text(cell)));
            }
        }
        Block::Rule => {}
    }
}

/// Matches of `needle` in one row.
pub(crate) fn row_match_count(row: &Row, needle: &str, case_sensitive: bool) -> usize {
    row_segments(row)
        .iter()
        .map(|segment| find_ranges(segment, needle, case_sensitive).len())
        .sum()
}

/// Flatten per-row counts into document-ordered matches.
pub(crate) fn collect_matches(counts: impl IntoIterator<Item = usize>) -> Vec<FindMatch> {
    counts
        .into_iter()
        .enumerate()
        .flat_map(|(row, count)| (0..count).map(move |occurrence| FindMatch { row, occurrence }))
        .collect()
}

/// The next (or previous) match index, wrapping; the first (or last) when
/// nothing is current.
pub(crate) fn step(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(ix), true) => (ix + 1) % len,
        (Some(ix), false) => (ix + len - 1) % len,
    })
}

/// The first match at or below `row`, else the first match (wrapping).
pub(crate) fn nearest(matches: &[FindMatch], row: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().position(|m| m.row >= row).unwrap_or(0))
}

impl FindState {
    fn new(needle: &str, case_sensitive: bool) -> Self {
        Self {
            needle: needle.to_owned(),
            case_sensitive,
            matches: Vec::new(),
            current: None,
            counts: HashMap::new(),
        }
    }

    /// Recount `rows` (cached per row version) and rebuild the match list.
    fn recount(&mut self, rows: &[Row]) {
        let mut counts = HashMap::with_capacity(rows.len());
        let per_row: Vec<usize> = rows
            .iter()
            .map(|row| {
                let count = match self.counts.get(&row.id) {
                    Some(&(version, count)) if version == row.version => count,
                    _ => row_match_count(row, &self.needle, self.case_sensitive),
                };
                counts.insert(row.id.clone(), (row.version, count));
                count
            })
            .collect();
        self.counts = counts;
        self.matches = collect_matches(per_row);
    }
}

impl Transcript {
    /// Scroll to `message_id` once `chat_id` is the selected chat and its
    /// rows hold the message (now, or on a later sync).
    pub(crate) fn reveal_entry(
        &mut self,
        chat_id: String,
        message_id: String,
        cx: &mut Context<Self>,
    ) {
        self.pending_reveal = Some(PendingReveal {
            chat_id,
            message_id,
            since: Instant::now(),
        });
        self.apply_pending_reveal(cx);
    }

    fn apply_pending_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending_reveal else {
            return;
        };
        if pending.since.elapsed() > REVEAL_PATIENCE {
            self.pending_reveal = None;
            return;
        }
        let state = self.state_entity().read(cx);
        if state.selected_chat.as_deref() != Some(pending.chat_id.as_str()) {
            return;
        }
        let replayed = state.transcript_replayed;
        let row = self
            .rows()
            .iter()
            .position(|row| row.entry_id.as_ref() == pending.message_id);
        match row {
            Some(ix) => {
                self.pending_reveal = None;
                self.jump_to_row(ix, OWN_SEND_TOP_INSET_PX + 8.0, cx);
            }
            // The whole transcript is here and the message is not.
            None if replayed && !self.rows().is_empty() => self.pending_reveal = None,
            None => {}
        }
    }

    /// Bring row `ix` under the chrome unless it already sits in view.
    fn jump_to_row(&mut self, ix: usize, inset: f32, cx: &mut Context<Self>) {
        let list = self.list_state().clone();
        let viewport = list.viewport_bounds();
        let visible = list.bounds_for_item(ix).is_some_and(|bounds| {
            f32::from(bounds.top()) >= f32::from(viewport.top()) + inset
                && f32::from(bounds.top()) <= f32::from(viewport.bottom()) - 120.0
        });
        if visible {
            return;
        }
        self.begin_scroll_navigation();
        list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(-inset),
        });
        cx.notify();
    }

    /// Re-resolve after the rows changed (streaming, chat switch).
    pub(crate) fn after_sync_find(&mut self, cx: &mut Context<Self>) {
        self.apply_pending_reveal(cx);
        let Some(mut find) = self.find.take() else {
            return;
        };
        let rows = self.rows();
        // Keep the current match by identity when it survives, else stay
        // near where it was.
        let previous = find.current;
        let identity = previous
            .and_then(|ix| find.matches.get(ix).copied())
            .and_then(|m| rows.get(m.row).map(|row| (row.id.clone(), m.occurrence)));
        find.recount(rows);
        let kept = identity.and_then(|(id, occurrence)| {
            find.matches.iter().position(|m| {
                m.occurrence == occurrence && rows.get(m.row).is_some_and(|row| row.id == id)
            })
        });
        let len = find.matches.len();
        find.current = kept.or_else(|| previous.filter(|_| len > 0).map(|ix| ix.min(len - 1)));
        self.find = Some(find);
        self.publish_find(cx);
    }

    /// Search for `needle` (empty clears the matches, keeping the bar),
    /// landing on the first match at or below the viewport.
    pub(crate) fn set_find_query(
        &mut self,
        needle: &str,
        case_sensitive: bool,
        cx: &mut Context<Self>,
    ) {
        let mut find = FindState::new(needle, case_sensitive);
        if let Some(previous) = self.find.take()
            && previous.case_sensitive == case_sensitive
            && previous.needle == needle
        {
            // Same query (the bar re-sent it): keep counts and position.
            find = previous;
        } else if !needle.is_empty() {
            find.recount(self.rows());
            let top = self.list_state().logical_scroll_top().item_ix;
            find.current = nearest(&find.matches, top);
        }
        let target = find.current.map(|ix| find.matches[ix].row);
        self.find = Some(find);
        self.publish_find(cx);
        if let Some(row) = target {
            self.jump_to_row(row, OWN_SEND_TOP_INSET_PX + FIND_BAR_CLEARANCE_PX, cx);
        }
        cx.notify();
    }

    /// Enter / Shift+Enter.
    pub(crate) fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.current = step(find.current, find.matches.len(), forward);
        let target = find.current.map(|ix| find.matches[ix].row);
        self.publish_find(cx);
        if let Some(row) = target {
            self.jump_to_row(row, OWN_SEND_TOP_INSET_PX + FIND_BAR_CLEARANCE_PX, cx);
        }
        cx.notify();
    }

    /// `(current, total)`, the current one 1-based (0 when none).
    pub(crate) fn find_status(&self) -> Option<(usize, usize)> {
        let find = self.find.as_ref()?;
        Some((find.current.map_or(0, |ix| ix + 1), find.matches.len()))
    }

    pub(crate) fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find = None;
        crate::markdown::find::clear(cx.entity_id().as_u64());
        cx.notify();
    }

    fn publish_find(&self, cx: &mut Context<Self>) {
        let surface = cx.entity_id().as_u64();
        let Some(find) = &self.find else {
            crate::markdown::find::clear(surface);
            return;
        };
        let theme = Theme::of(cx);
        let current = find
            .current
            .and_then(|ix| find.matches.get(ix))
            .and_then(|m| {
                self.rows()
                    .get(m.row)
                    .map(|row| (row.id.to_string(), m.occurrence))
            });
        crate::markdown::find::set(FindHighlight {
            surface,
            needle: find.needle.clone(),
            case_sensitive: find.case_sensitive,
            current,
            match_wash: theme.warning.opacity(0.25),
            current_wash: theme.warning.opacity(0.6),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn user_row(id: &str, text: &str) -> Row {
        Row {
            id: id.to_owned().into(),
            version: 1,
            turn_start: true,
            kind: RowKind::User {
                text: text.to_owned().into(),
                mentions: Arc::default(),
                attachments: Arc::default(),
                badges: Arc::default(),
                pending: false,
            },
            entry_id: id.to_owned().into(),
            timestamp: None,
            copy_text: None,
            compact_fold: None,
        }
    }

    fn markdown_row(id: &str, source: &str) -> Row {
        Row {
            id: format!("{id}#0").into(),
            version: 1,
            turn_start: true,
            kind: RowKind::Markdown {
                tree: Arc::new(crate::markdown::parse_full(source)),
                block_ix: 0,
            },
            entry_id: id.to_owned().into(),
            timestamp: None,
            copy_text: None,
            compact_fold: None,
        }
    }

    #[test]
    fn segments_follow_painted_elements() {
        let row = markdown_row("a", "- **Bold** cat\n- `code` cat");
        assert_eq!(row_segments(&row), ["Bold cat", "code cat"]);
        let fence = markdown_row("b", "```rust\nlet cat = 1;\nlet dog = cat;\n```");
        assert_eq!(row_segments(&fence), ["let cat = 1;", "let dog = cat;"]);
        let table = markdown_row("c", "| Cat | Dog |\n|---|---|\n| cat | x |");
        assert_eq!(row_match_count(&table, "cat", false), 2);
        assert_eq!(row_match_count(&table, "cat", true), 1);
        assert_eq!(row_segments(&user_row("u", "my cat")), ["my cat"]);
    }

    #[test]
    fn matches_flatten_in_document_order() {
        let matches = collect_matches([2, 0, 1]);
        assert_eq!(
            matches,
            [
                FindMatch { row: 0, occurrence: 0 },
                FindMatch { row: 0, occurrence: 1 },
                FindMatch { row: 2, occurrence: 0 },
            ]
        );
    }

    #[test]
    fn navigation_wraps_both_ways() {
        assert_eq!(step(None, 0, true), None);
        assert_eq!(step(None, 3, true), Some(0));
        assert_eq!(step(None, 3, false), Some(2));
        assert_eq!(step(Some(2), 3, true), Some(0));
        assert_eq!(step(Some(0), 3, false), Some(2));
        assert_eq!(step(Some(1), 3, true), Some(2));
    }

    #[test]
    fn landing_prefers_the_viewport_then_wraps() {
        let matches = collect_matches([1, 0, 0, 2, 0]);
        assert_eq!(nearest(&matches, 0), Some(0));
        assert_eq!(nearest(&matches, 2), Some(1));
        assert_eq!(nearest(&matches, 4), Some(0), "nothing below wraps to the top");
        assert_eq!(nearest(&[], 0), None);
    }

    #[test]
    fn recount_reuses_unchanged_rows_and_sees_new_text() {
        let mut rows = vec![user_row("a", "cat"), markdown_row("b", "a cat and a CAT")];
        let mut find = FindState::new("cat", false);
        find.recount(&rows);
        assert_eq!(find.matches.len(), 3);
        // A streamed row bumps its version; the count follows.
        rows[1] = markdown_row("b", "a cat, a CAT, a cat");
        rows[1].version = 2;
        find.recount(&rows);
        assert_eq!(find.matches.len(), 4);
        // An unchanged version is trusted from the cache.
        rows[0].kind = RowKind::User {
            text: "no match".into(),
            mentions: Arc::default(),
            attachments: Arc::default(),
            badges: Arc::default(),
            pending: false,
        };
        find.recount(&rows);
        assert_eq!(find.matches.len(), 4);
    }
}
