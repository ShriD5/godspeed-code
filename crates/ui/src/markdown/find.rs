//! Find-in-thread highlighting for painted transcript text.
//!
//! The transcript resolves matches over its whole row model (virtualized
//! rows included) and publishes the needle plus the current match as
//! `(row id, occurrence within the row)`. Each painted text element then
//! washes its own occurrences; it knows which one is current by counting the
//! occurrences of the same row's elements painted before it this frame (a
//! row's elements paint together, in document order — see `selection.rs`).
//!
//! This module is the pure half (unit-tested; colors are plain values the
//! transcript resolves from its theme); the washes are painted from
//! `render.rs`.

use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq)]
pub struct FindHighlight {
    /// The painted transcript the find bar belongs to.
    pub surface: u64,
    pub needle: String,
    pub case_sensitive: bool,
    /// `(row id, occurrence index within the row)`.
    pub current: Option<(String, usize)>,
    /// Wash under every match, and under the current one.
    pub match_wash: gpui::Hsla,
    pub current_wash: gpui::Hsla,
}

fn state() -> &'static Mutex<Option<Arc<FindHighlight>>> {
    static STATE: OnceLock<Mutex<Option<Arc<FindHighlight>>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// Publish the highlight (an empty needle clears it).
pub fn set(highlight: FindHighlight) {
    *state().lock().unwrap() = (!highlight.needle.is_empty()).then(|| Arc::new(highlight));
}

/// Clear the highlight if `surface` owns it.
pub fn clear(surface: u64) {
    let mut guard = state().lock().unwrap();
    if guard.as_ref().is_some_and(|h| h.surface == surface) {
        *guard = None;
    }
}

/// The highlight painting on `surface`, if any.
pub fn active(surface: u64) -> Option<Arc<FindHighlight>> {
    state()
        .lock()
        .unwrap()
        .as_ref()
        .filter(|h| h.surface == surface)
        .cloned()
}

#[cfg(test)]
pub(crate) fn test_state_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The transcript row a text element's key belongs to: `{row}:{n}`
/// (markdown text), `{row}:u` (user bubble) or `{row}-code{n}-line{m}`
/// (code lines). Other keys (tool details, previews) are not searched.
pub fn row_of_key(key: &str) -> Option<&str> {
    if let Some(at) = key.rfind("-code") {
        let rest = &key[at + "-code".len()..];
        if let Some((code, line)) = rest.split_once("-line")
            && !code.is_empty()
            && code.bytes().all(|b| b.is_ascii_digit())
            && !line.is_empty()
            && line.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(&key[..at]);
        }
    }
    let (row, element) = key.rsplit_once(':')?;
    let markdown = !element.is_empty() && element.bytes().all(|b| b.is_ascii_digit());
    (markdown || element == "u").then_some(row)
}

/// Non-overlapping byte ranges of `needle` in `text`, left to right.
/// Case-insensitive matching lowercases per character (Unicode-aware) and
/// maps each match back to whole characters of the original text.
pub fn find_ranges(text: &str, needle: &str, case_sensitive: bool) -> Vec<Range<usize>> {
    if needle.is_empty() || text.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        return text
            .match_indices(needle)
            .map(|(at, found)| at..at + found.len())
            .collect();
    }
    if text.is_ascii() && needle.is_ascii() {
        let needle = needle.to_ascii_lowercase();
        let lowered = text.to_ascii_lowercase();
        return lowered
            .match_indices(&needle)
            .map(|(at, found)| at..at + found.len())
            .collect();
    }
    let needle: String = needle.chars().flat_map(char::to_lowercase).collect();
    // Lowered text plus, per lowered byte, the original char it came from.
    let mut lowered = String::with_capacity(text.len());
    let mut origin: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        for lower in c.to_lowercase() {
            let start = lowered.len();
            lowered.push(lower);
            origin.extend(std::iter::repeat_n((at, at + c.len_utf8()), lowered.len() - start));
        }
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (at, found) in lowered.match_indices(&needle) {
        let start = origin[at].0;
        let end = origin[at + found.len() - 1].1;
        // Two matches inside one expanded character collapse to one.
        if ranges.last().is_some_and(|last| last.end > start) {
            continue;
        }
        ranges.push(start..end);
    }
    ranges
}

/// Washes for one painted element: every occurrence, flagged when it is the
/// current match. `earlier` counts the occurrences in the same row's
/// elements painted before this one (only called when the row holds the
/// current match).
pub fn washes(
    highlight: &FindHighlight,
    key: &str,
    text: &str,
    earlier: impl FnOnce(&str) -> usize,
) -> Vec<(Range<usize>, bool)> {
    let Some(row) = row_of_key(key) else {
        return Vec::new();
    };
    let ranges = find_ranges(text, &highlight.needle, highlight.case_sensitive);
    if ranges.is_empty() {
        return Vec::new();
    }
    let current = highlight
        .current
        .as_ref()
        .filter(|(current_row, _)| current_row == row)
        .map(|(_, occurrence)| *occurrence);
    let before = if current.is_some() { earlier(row) } else { 0 };
    ranges
        .into_iter()
        .enumerate()
        .map(|(ix, range)| (range, current == Some(before + ix)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found<'a>(text: &'a str, needle: &str, case_sensitive: bool) -> Vec<&'a str> {
        find_ranges(text, needle, case_sensitive)
            .into_iter()
            .map(|r| &text[r])
            .collect()
    }

    #[test]
    fn ascii_matching_respects_case_toggle() {
        assert_eq!(found("Foo foo FOO", "foo", false), ["Foo", "foo", "FOO"]);
        assert_eq!(found("Foo foo FOO", "foo", true), ["foo"]);
        assert_eq!(found("aaaa", "aa", false).len(), 2, "non-overlapping");
        assert!(found("abc", "", false).is_empty());
    }

    #[test]
    fn unicode_matching_maps_back_to_original_characters() {
        assert_eq!(found("École école", "ÉCOLE", false), ["École", "école"]);
        assert_eq!(found("Straße", "STRASSE", false), Vec::<&str>::new());
        // İ lowercases to two chars; a match ending inside it covers it whole.
        let text = "İstanbul";
        assert_eq!(found(text, "i̇stan", false), ["İstan"]);
        assert_eq!(found("検索と検索", "検索", false).len(), 2);
    }

    #[test]
    fn element_keys_resolve_to_rows() {
        assert_eq!(row_of_key("m1#b2:0"), Some("m1#b2"));
        assert_eq!(row_of_key("m1:u"), Some("m1"));
        assert_eq!(row_of_key("imp:abc#0:12"), Some("imp:abc#0"));
        assert_eq!(row_of_key("m1#b2-code0-line14"), Some("m1#b2"));
        assert_eq!(row_of_key("m1#b2#d0-3"), None);
        assert_eq!(row_of_key("m1-codex:x"), None);
    }

    #[test]
    fn washes_flag_the_current_occurrence_across_elements() {
        let highlight = FindHighlight {
            surface: 1,
            needle: "cat".into(),
            case_sensitive: false,
            current: Some(("row".into(), 3)),
            match_wash: gpui::transparent_black(),
            current_wash: gpui::transparent_black(),
        };
        // Two occurrences were painted earlier in this row.
        let washes = washes(&highlight, "row:1", "Cat and cat", |row| {
            assert_eq!(row, "row");
            2
        });
        assert_eq!(washes, vec![(0..3, false), (8..11, true)]);
        // Other rows never consult the counter and have no current match.
        let other = super::washes(&highlight, "other:0", "cat", |_| unreachable!());
        assert_eq!(other, vec![(0..3, false)]);
        assert!(super::washes(&highlight, "row#d0-1", "cat", |_| 0).is_empty());
    }

    #[test]
    fn published_state_is_per_surface() {
        let _lock = test_state_lock();
        set(FindHighlight {
            surface: 9,
            needle: "x".into(),
            case_sensitive: false,
            current: None,
            match_wash: gpui::transparent_black(),
            current_wash: gpui::transparent_black(),
        });
        assert!(active(9).is_some());
        assert!(active(8).is_none());
        clear(8);
        assert!(active(9).is_some());
        clear(9);
        assert!(active(9).is_none());
        set(FindHighlight {
            surface: 9,
            needle: String::new(),
            case_sensitive: false,
            current: None,
            match_wash: gpui::transparent_black(),
            current_wash: gpui::transparent_black(),
        });
        assert!(active(9).is_none(), "an empty needle publishes nothing");
    }
}
