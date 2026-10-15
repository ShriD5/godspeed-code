//! The in-memory inverted index: one document per transcript entry, a
//! sorted term dictionary for prefix lookups, and per-term postings sorted
//! by document so an entry re-streamed in place updates without a rebuild.
//!
//! Scoring is BM25 over two fields (prose, and tool titles/commands at
//! [`TOOL_WEIGHT`]), with prefix matches discounted against exact ones, then
//! a recency boost. Matching works per query term over the dictionary range
//! that starts with it, so a one-character query is a short range walk plus
//! a dense pass over its postings.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::Arc;

use zeron_proto::{MessageSearchField, MessageSearchHit, MessageSearchRole};

use super::tokens::{match_ranges, normalize, query_terms, token_ranges};

/// Prose beyond this many bytes of one message is not indexed (pasted logs).
pub(crate) const MAX_TEXT_BYTES: usize = 32 * 1024;
/// Tool titles/commands beyond this many bytes of one message are dropped.
pub(crate) const MAX_TOOL_BYTES: usize = 8 * 1024;
/// A tool-field occurrence counts this much of a prose occurrence.
const TOOL_WEIGHT: f32 = 0.35;
const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.6;
/// A recent message scores up to `1 + RECENCY_BOOST` times an old one; the
/// boost halves every [`RECENCY_HALF_LIFE_DAYS`].
const RECENCY_BOOST: f32 = 0.5;
const RECENCY_HALF_LIFE_DAYS: f32 = 30.0;
/// Characters of context in a snippet.
pub(crate) const SNIPPET_CHARS: usize = 140;
/// Context kept before the first match.
const SNIPPET_LEAD_CHARS: usize = 36;

/// One transcript entry as the index sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MessageText {
    pub id: String,
    pub role: MessageSearchRole,
    pub created_at: i64,
    pub text: String,
    pub tools: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub chat: Arc<str>,
    pub message_id: Box<str>,
    pub role: MessageSearchRole,
    pub created_at: i64,
    pub text: Box<str>,
    pub tools: Box<str>,
    /// Token count over both fields (BM25 length normalization).
    pub len: u32,
    pub hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Posting {
    pub doc: u32,
    pub text_tf: u16,
    pub tool_tf: u16,
}

/// The version of a chat's stored snapshot this index reflects:
/// `(saved_at, byte length)` from the docs store.
pub(crate) type Marker = (i64, i64);

#[derive(Debug, Default, Clone)]
pub(crate) struct ChatEntry {
    pub marker: Option<Marker>,
    pub messages: HashMap<Box<str>, u32>,
}

#[derive(Default)]
pub(crate) struct Index {
    pub(super) docs: Vec<Option<Doc>>,
    pub(super) live: usize,
    pub(super) total_len: u64,
    pub(super) chats: HashMap<Arc<str>, ChatEntry>,
    pub(super) terms: BTreeMap<Box<str>, u32>,
    pub(super) postings: Vec<Vec<Posting>>,
    /// Term ids whose postings emptied, reused before growing.
    pub(super) free_terms: Vec<u32>,
}

/// Search knobs beyond the query.
#[derive(Default)]
pub(crate) struct SearchOptions<'a> {
    pub limit: usize,
    pub per_chat: Option<usize>,
    /// Keep only chats this accepts; checked on ranked candidates only.
    pub chat_filter: Option<&'a dyn Fn(&str) -> bool>,
    pub now_ms: i64,
}

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn content_hash(message: &MessageText) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    fnv1a(&mut hash, &[message.role as u8]);
    fnv1a(&mut hash, &message.created_at.to_le_bytes());
    fnv1a(&mut hash, message.text.as_bytes());
    fnv1a(&mut hash, &[0xff]);
    fnv1a(&mut hash, message.tools.as_bytes());
    hash
}

/// `text` cut to at most `max` bytes on a char boundary.
fn capped(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    &text[..text.floor_char_boundary(max)]
}

/// Per-term frequencies of one document, prose and tool fields apart.
fn term_counts(text: &str, tools: &str) -> (HashMap<String, (u16, u16)>, u32) {
    let mut counts: HashMap<String, (u16, u16)> = HashMap::new();
    let mut len = 0u32;
    let mut term = String::new();
    for (field, source) in [(0, text), (1, tools)] {
        for (start, end) in token_ranges(source) {
            normalize(&source[start..end], &mut term);
            if term.is_empty() {
                continue;
            }
            len += 1;
            let entry = match counts.get_mut(term.as_str()) {
                Some(entry) => entry,
                None => counts.entry(term.clone()).or_default(),
            };
            if field == 0 {
                entry.0 = entry.0.saturating_add(1);
            } else {
                entry.1 = entry.1.saturating_add(1);
            }
        }
    }
    (counts, len)
}

impl Index {
    pub(crate) fn chat_count(&self) -> usize {
        self.chats.len()
    }

    pub(crate) fn message_count(&self) -> usize {
        self.live
    }

    pub(crate) fn term_count(&self) -> usize {
        self.terms.len()
    }

    pub(crate) fn marker(&self, chat_id: &str) -> Option<Marker> {
        self.chats.get(chat_id).and_then(|chat| chat.marker)
    }

    pub(crate) fn set_marker(&mut self, chat_id: &str, marker: Option<Marker>) {
        if let Some(chat) = self.chats.get_mut(chat_id) {
            chat.marker = marker;
        }
    }

    pub(crate) fn chat_ids(&self) -> Vec<String> {
        self.chats.keys().map(|id| id.to_string()).collect()
    }

    /// Make `messages` the chat's indexed transcript: unchanged entries are
    /// skipped by content hash, changed ones re-index in place, vanished ones
    /// are dropped. Returns whether anything changed.
    pub(crate) fn update_chat(&mut self, chat_id: &str, messages: &[MessageText]) -> bool {
        let chat: Arc<str> = match self.chats.get_key_value(chat_id) {
            Some((key, _)) => key.clone(),
            None => {
                let key: Arc<str> = chat_id.into();
                self.chats.insert(key.clone(), ChatEntry::default());
                key
            }
        };
        let mut previous = std::mem::take(&mut self.chats.get_mut(chat_id).unwrap().messages);
        let mut next: HashMap<Box<str>, u32> = HashMap::with_capacity(messages.len());
        let mut changed = false;
        for message in messages {
            if next.contains_key(message.id.as_str()) {
                continue;
            }
            let hash = content_hash(message);
            let slot = match previous.remove(message.id.as_str()) {
                Some(slot) if self.docs[slot as usize].as_ref().is_some_and(|d| d.hash == hash) => {
                    slot
                }
                Some(slot) => {
                    self.unindex(slot);
                    self.index_into(slot, chat.clone(), message, hash);
                    changed = true;
                    slot
                }
                None => {
                    let slot = self.docs.len() as u32;
                    self.docs.push(None);
                    self.index_into(slot, chat.clone(), message, hash);
                    changed = true;
                    slot
                }
            };
            next.insert(message.id.as_str().into(), slot);
        }
        for (_, slot) in previous.drain() {
            self.unindex(slot);
            changed = true;
        }
        self.chats.get_mut(chat_id).unwrap().messages = next;
        changed
    }

    pub(crate) fn remove_chat(&mut self, chat_id: &str) -> bool {
        let Some(chat) = self.chats.remove(chat_id) else {
            return false;
        };
        for slot in chat.messages.into_values() {
            self.unindex(slot);
        }
        true
    }

    fn index_into(&mut self, slot: u32, chat: Arc<str>, message: &MessageText, hash: u64) {
        let text = capped(&message.text, MAX_TEXT_BYTES);
        let tools = capped(&message.tools, MAX_TOOL_BYTES);
        let (counts, len) = term_counts(text, tools);
        for (term, (text_tf, tool_tf)) in counts {
            let id = self.term_id(term);
            let list = &mut self.postings[id as usize];
            let posting = Posting {
                doc: slot,
                text_tf,
                tool_tf,
            };
            // Appends are the common case: new entries take the next slot.
            match list.last() {
                Some(last) if last.doc > slot => {
                    let at = list.partition_point(|p| p.doc < slot);
                    list.insert(at, posting);
                }
                _ => list.push(posting),
            }
        }
        self.docs[slot as usize] = Some(Doc {
            chat,
            message_id: message.id.as_str().into(),
            role: message.role,
            created_at: message.created_at,
            text: text.into(),
            tools: tools.into(),
            len,
            hash,
        });
        self.live += 1;
        self.total_len += u64::from(len);
    }

    fn unindex(&mut self, slot: u32) {
        let Some(doc) = self.docs.get_mut(slot as usize).and_then(Option::take) else {
            return;
        };
        self.live -= 1;
        self.total_len -= u64::from(doc.len);
        let (counts, _) = term_counts(&doc.text, &doc.tools);
        for term in counts.into_keys() {
            let Some(&id) = self.terms.get(term.as_str()) else {
                continue;
            };
            let list = &mut self.postings[id as usize];
            if let Ok(at) = list.binary_search_by_key(&slot, |p| p.doc) {
                list.remove(at);
            }
            if list.is_empty() {
                self.terms.remove(term.as_str());
                self.postings[id as usize] = Vec::new();
                self.free_terms.push(id);
            }
        }
    }

    fn term_id(&mut self, term: String) -> u32 {
        if let Some(&id) = self.terms.get(term.as_str()) {
            return id;
        }
        let id = match self.free_terms.pop() {
            Some(id) => id,
            None => {
                self.postings.push(Vec::new());
                (self.postings.len() - 1) as u32
            }
        };
        self.terms.insert(term.into_boxed_str(), id);
        id
    }

    /// Ranked hits for `query`: every term must prefix-match a token of the
    /// message (either field).
    pub(crate) fn search(&self, query: &str, options: &SearchOptions) -> Vec<MessageSearchHit> {
        let terms = query_terms(query);
        if terms.is_empty() || self.live == 0 || options.limit == 0 {
            return Vec::new();
        }
        let n = self.live as f32;
        let average = (self.total_len as f32 / n).max(1.0);
        // `matched[doc] == i` means the doc matched the first `i` terms.
        let mut matched = vec![0u16; self.docs.len()];
        let mut score = vec![0f32; self.docs.len()];
        let mut best = vec![0f32; self.docs.len()];
        let mut candidates: Vec<u32> = Vec::new();
        for (i, term) in terms.iter().enumerate() {
            let round = i as u16;
            let mut touched: Vec<u32> = Vec::new();
            let expansions: Vec<(&str, &Vec<Posting>)> = self
                .terms
                .range::<str, _>((Bound::Included(term.as_str()), Bound::Unbounded))
                .take_while(|(token, _)| token.starts_with(term.as_str()))
                .map(|(token, &id)| (token.as_ref(), &self.postings[id as usize]))
                .collect();
            // One idf for the whole expansion (its summed document
            // frequency, capped at the corpus): a rare completion must not
            // outrank the exact word just for being rare.
            let df = (expansions.iter().map(|(_, list)| list.len()).sum::<usize>() as f32).min(n);
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (token, list) in expansions {
                let quality = if token.len() == term.len() {
                    1.0
                } else {
                    0.5 + 0.4 * term.len() as f32 / token.len() as f32
                };
                let weight = idf * quality;
                for posting in list {
                    let doc = posting.doc as usize;
                    let state = matched[doc];
                    if state != round && state != round + 1 {
                        continue;
                    }
                    // Postings never point at removed slots.
                    let len = self.docs[doc].as_ref().map_or(1, |d| d.len) as f32;
                    let tf = f32::from(posting.text_tf) + TOOL_WEIGHT * f32::from(posting.tool_tf);
                    let norm = tf * (BM25_K1 + 1.0)
                        / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * len / average));
                    let s = weight * norm;
                    if state == round {
                        matched[doc] = round + 1;
                        best[doc] = s;
                        touched.push(posting.doc);
                    } else if s > best[doc] {
                        best[doc] = s;
                    }
                }
            }
            for &doc in &touched {
                score[doc as usize] += best[doc as usize];
            }
            if touched.is_empty() {
                return Vec::new();
            }
            candidates = touched;
        }
        let all = terms.len() as u16;
        let mut ranked: Vec<(u32, f32)> = candidates
            .into_iter()
            .filter(|&doc| matched[doc as usize] == all)
            .filter_map(|doc| {
                let created_at = self.docs[doc as usize].as_ref()?.created_at;
                let age_days = (options.now_ms - created_at).max(0) as f32 / 86_400_000.0;
                let recency = 1.0 + RECENCY_BOOST * (-age_days / RECENCY_HALF_LIFE_DAYS).exp2();
                Some((doc, score[doc as usize] * recency))
            })
            .collect();
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut per_chat: HashMap<&str, usize> = HashMap::new();
        let mut hits = Vec::new();
        for (slot, score) in ranked {
            let Some(doc) = self.docs[slot as usize].as_ref() else {
                continue;
            };
            if options.chat_filter.is_some_and(|accept| !accept(&doc.chat)) {
                continue;
            }
            if let Some(cap) = options.per_chat {
                let seen = per_chat.entry(&doc.chat).or_default();
                if *seen >= cap {
                    continue;
                }
                *seen += 1;
            }
            hits.push(hit(doc, &terms, score));
            if hits.len() >= options.limit {
                break;
            }
        }
        hits
    }
}

fn hit(doc: &Doc, terms: &[String], score: f32) -> MessageSearchHit {
    let (field, (snippet, ranges)) = if let Some(found) = snippet(&doc.text, terms) {
        (MessageSearchField::Text, found)
    } else if let Some(found) = snippet(&doc.tools, terms) {
        (MessageSearchField::Tool, found)
    } else if !doc.text.is_empty() {
        // Unreachable for a matched doc; show the prose opening regardless.
        (MessageSearchField::Text, (collapse(&doc.text, 0, SNIPPET_CHARS).0, Vec::new()))
    } else {
        (MessageSearchField::Tool, (collapse(&doc.tools, 0, SNIPPET_CHARS).0, Vec::new()))
    };
    MessageSearchHit {
        chat_id: doc.chat.to_string(),
        message_id: doc.message_id.to_string(),
        role: doc.role,
        created_at: doc.created_at,
        field,
        snippet,
        ranges,
        score,
    }
}

/// A one-line window of `text` around its first match, whitespace collapsed,
/// with the matches inside it as snippet byte ranges. `None` when `text`
/// holds no match.
pub(crate) fn snippet(text: &str, terms: &[String]) -> Option<(String, Vec<(u32, u32)>)> {
    let matches = match_ranges(text, terms);
    let first = matches.first()?.0;
    // Back up a little for context, to a word boundary when one is near.
    let mut start = first;
    for (taken, (at, c)) in text[..first].char_indices().rev().enumerate() {
        if taken >= SNIPPET_LEAD_CHARS {
            break;
        }
        start = at;
        if c.is_whitespace() && taken > SNIPPET_LEAD_CHARS / 2 {
            break;
        }
    }
    let (mut out, map) = collapse(text, start, SNIPPET_CHARS);
    let end = map.last().map_or(start, |&(original, _)| original);
    let at = |original: usize| {
        map.iter()
            .find(|&&(o, _)| o >= original)
            .map_or(out.len(), |&(_, s)| s)
    };
    let mut ranges: Vec<(u32, u32)> = matches
        .into_iter()
        .filter(|&(s, e)| s >= start && e <= end)
        .map(|(s, e)| (at(s) as u32, at(e) as u32))
        .filter(|(s, e)| s < e)
        .collect();
    if start > 0 {
        out.insert_str(0, "…");
        let shift = "…".len() as u32;
        for range in &mut ranges {
            range.0 += shift;
            range.1 += shift;
        }
    }
    if !text[end..].trim().is_empty() {
        out.push('…');
    }
    Some((out, ranges))
}

/// Up to `max_chars` of `text` from byte `start` with whitespace runs
/// collapsed to one space (leading whitespace dropped), plus the char
/// boundary map `(original byte, snippet byte)`, ending one past the last
/// character taken.
fn collapse(text: &str, start: usize, max_chars: usize) -> (String, Vec<(usize, usize)>) {
    let mut out = String::new();
    let mut map = Vec::new();
    let mut taken = 0;
    let mut space = false;
    let mut end = start;
    for (at, c) in text[start..].char_indices() {
        let at = start + at;
        if taken >= max_chars {
            break;
        }
        map.push((at, out.len()));
        end = at + c.len_utf8();
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        // The mapped position of a char is where its glyph lands.
        if let Some(last) = map.last_mut() {
            last.1 = out.len();
        }
        out.push(c);
        taken += 1;
    }
    map.push((end, out.len()));
    (out, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn message(id: &str, text: &str) -> MessageText {
        MessageText {
            id: id.into(),
            role: MessageSearchRole::Assistant,
            created_at: 0,
            text: text.into(),
            tools: String::new(),
        }
    }

    fn options(limit: usize) -> SearchOptions<'static> {
        SearchOptions {
            limit,
            now_ms: 0,
            ..Default::default()
        }
    }

    fn ids(hits: &[MessageSearchHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.message_id.as_str()).collect()
    }

    #[test]
    fn prefix_multi_term_and_case_insensitive() {
        let mut index = Index::default();
        index.update_chat(
            "c",
            &[
                message("m1", "The Workspace search index is fast"),
                message("m2", "workspace only"),
                message("m3", "nothing here"),
            ],
        );
        assert_eq!(ids(&index.search("w", &options(10))).len(), 2);
        assert_eq!(ids(&index.search("WORK sea", &options(10))), ["m1"]);
        assert!(index.search("work zebra", &options(10)).is_empty());
        assert!(index.search("  ", &options(10)).is_empty());
    }

    #[test]
    fn unicode_terms_match_case_insensitively() {
        let mut index = Index::default();
        index.update_chat(
            "c",
            &[
                message("fr", "L'ÉCOLE est fermée"),
                message("ru", "Привет, мир"),
                message("ja", "全文検索を追加"),
            ],
        );
        assert_eq!(ids(&index.search("école", &options(5))), ["fr"]);
        assert_eq!(ids(&index.search("ПРИ", &options(5))), ["ru"]);
        assert_eq!(ids(&index.search("検索", &options(5))), ["ja"]);
        let hit = &index.search("fermé", &options(5))[0];
        let (s, e) = hit.ranges[0];
        assert_eq!(&hit.snippet[s as usize..e as usize], "fermé");
    }

    #[test]
    fn exact_beats_prefix_and_prose_beats_tools() {
        let mut index = Index::default();
        let mut tool = message("tool", "");
        tool.tools = "cargo test".into();
        index.update_chat(
            "c",
            &[
                message("prefix", "testing the parser"),
                message("exact", "test the parser"),
                tool,
            ],
        );
        let hits = index.search("test", &options(10));
        assert_eq!(ids(&hits)[0], "exact");
        assert_eq!(ids(&hits)[2], "tool");
        assert_eq!(hits[2].field, MessageSearchField::Tool);
        assert_eq!(hits[2].snippet, "cargo test");
        assert_eq!(hits[2].ranges, [(6, 10)]);
    }

    #[test]
    fn recency_breaks_relevance_ties() {
        let mut index = Index::default();
        let mut old = message("old", "deploy the worker");
        old.created_at = 0;
        let mut new = message("new", "deploy the worker");
        new.created_at = 90 * DAY;
        index.update_chat("c", &[old, new]);
        let options = SearchOptions {
            limit: 10,
            now_ms: 91 * DAY,
            ..Default::default()
        };
        assert_eq!(ids(&index.search("deploy", &options)), ["new", "old"]);
    }

    #[test]
    fn incremental_updates_reindex_in_place() {
        let mut index = Index::default();
        let mut streaming = message("a", "hello");
        assert!(index.update_chat("c", std::slice::from_ref(&streaming)));
        assert!(!index.update_chat("c", std::slice::from_ref(&streaming)));
        streaming.text.push_str(" wonderful world");
        assert!(index.update_chat("c", std::slice::from_ref(&streaming)));
        assert_eq!(index.docs.len(), 1, "a streamed entry keeps its slot");
        assert_eq!(ids(&index.search("wonder", &options(5))), ["a"]);
        // Dropped entries leave no postings behind.
        assert!(index.update_chat("c", &[message("b", "other")]));
        assert!(index.search("hello", &options(5)).is_empty());
        assert_eq!(index.message_count(), 1);
        assert!(!index.terms.contains_key("hello"));
        assert!(index.remove_chat("c"));
        assert_eq!(index.term_count(), 0);
        assert_eq!(index.total_len, 0);
    }

    #[test]
    fn per_chat_cap_and_filter() {
        let mut index = Index::default();
        index.update_chat("a", &[message("a1", "fish"), message("a2", "fish fish")]);
        index.update_chat("b", &[message("b1", "fish")]);
        let options = SearchOptions {
            limit: 10,
            per_chat: Some(1),
            ..Default::default()
        };
        let hits = index.search("fish", &options);
        assert_eq!(hits.len(), 2);
        let only_b = |chat: &str| chat == "b";
        let options = SearchOptions {
            limit: 10,
            chat_filter: Some(&only_b),
            ..Default::default()
        };
        assert_eq!(ids(&index.search("fish", &options)), ["b1"]);
    }

    #[test]
    fn snippet_collapses_whitespace_and_maps_ranges() {
        let text = format!("{}\n\nthe   needle\tis here", "lead ".repeat(30));
        let (snippet, ranges) = snippet(&text, &["needle".into()]).unwrap();
        assert!(snippet.starts_with('…'));
        assert!(!snippet.contains('\n') && !snippet.contains("  "));
        let (s, e) = ranges[0];
        assert_eq!(&snippet[s as usize..e as usize], "needle");
        let (short, ranges) = super::snippet("Needle first", &["need".into()]).unwrap();
        assert_eq!(short, "Needle first");
        assert_eq!(ranges, [(0, 4)]);
        assert!(super::snippet("nothing", &["zzz".into()]).is_none());
    }

    #[test]
    fn long_text_is_capped_on_a_char_boundary() {
        let mut index = Index::default();
        let text = format!("{}tail", "é".repeat(MAX_TEXT_BYTES));
        index.update_chat("c", &[message("m", &text)]);
        let doc = index.docs[0].as_ref().unwrap();
        assert!(doc.text.len() <= MAX_TEXT_BYTES);
        assert!(index.search("tail", &options(5)).is_empty());
    }
}
