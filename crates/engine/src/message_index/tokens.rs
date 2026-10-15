//! Tokenizing and match finding shared by indexing, queries and snippets.
//!
//! A token is a run of alphanumeric characters (Unicode-aware), so
//! `workspace_search.rs` yields `workspace`, `search`, `rs`. Scripts written
//! without spaces (Han, kana) give one token per character, which keeps
//! prefix matching useful there. Tokens are compared lowercased
//! (`char::to_lowercase`, so `ÉCOLE` matches `école`); no accent folding.

/// Longer tokens (hashes, base64) are indexed by their first bytes only.
pub(crate) const MAX_TOKEN_BYTES: usize = 32;

/// Characters that form a token on their own.
fn standalone(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF       // Hiragana, Katakana
        | 0x3400..=0x4DBF     // CJK Extension A
        | 0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0xF900..=0xFAFF     // CJK Compatibility Ideographs
        | 0x20000..=0x2FFFF)  // CJK Extensions B+
}

/// Byte ranges of the tokens of `text`, in order.
pub(crate) fn token_ranges(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        loop {
            let (start, c) = chars.next()?;
            if standalone(c) {
                return Some((start, start + c.len_utf8()));
            }
            if !c.is_alphanumeric() {
                continue;
            }
            let mut end = start + c.len_utf8();
            while let Some(&(at, next)) = chars.peek() {
                if !next.is_alphanumeric() || standalone(next) {
                    break;
                }
                end = at + next.len_utf8();
                chars.next();
            }
            return Some((start, end));
        }
    })
}

/// The lowercased index form of one token, capped at [`MAX_TOKEN_BYTES`].
pub(crate) fn normalize(token: &str, out: &mut String) {
    out.clear();
    for c in token.chars() {
        for lower in c.to_lowercase() {
            if out.len() + lower.len_utf8() > MAX_TOKEN_BYTES {
                return;
            }
            out.push(lower);
        }
    }
}

/// Lowercased, de-duplicated query terms in query order.
pub(crate) fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut term = String::new();
    for (start, end) in token_ranges(query) {
        normalize(&query[start..end], &mut term);
        if !term.is_empty() && !terms.contains(&term) {
            terms.push(term.clone());
        }
    }
    terms
}

/// Byte ranges in `text` where a token starts with one of `terms` (already
/// lowercased): each range covers the matched prefix in the ORIGINAL text,
/// so `Ünïcode` highlights correctly for the term `ün`.
pub(crate) fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if terms.is_empty() {
        return ranges;
    }
    let mut lowered = String::new();
    for (start, end) in token_ranges(text) {
        let token = &text[start..end];
        // The longest term this token starts with, measured in original bytes.
        let mut best: Option<usize> = None;
        for term in terms {
            if let Some(len) = prefix_len(token, term, &mut lowered) {
                best = Some(best.map_or(len, |b| b.max(len)));
            }
        }
        if let Some(len) = best {
            ranges.push((start, start + len));
        }
    }
    ranges
}

/// The original-byte length of `token`'s prefix whose lowercase form starts
/// with `term`, if any.
fn prefix_len(token: &str, term: &str, lowered: &mut String) -> Option<usize> {
    lowered.clear();
    for (at, c) in token.char_indices() {
        for lower in c.to_lowercase() {
            lowered.push(lower);
        }
        if lowered.len() >= term.len() {
            return lowered.starts_with(term).then_some(at + c.len_utf8());
        }
        if !term.starts_with(lowered.as_str()) {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<&str> {
        token_ranges(text).map(|(s, e)| &text[s..e]).collect()
    }

    #[test]
    fn splits_on_punctuation_and_keeps_unicode_words() {
        assert_eq!(
            tokens("fix workspace_search.rs (café, naïve) v2!"),
            ["fix", "workspace", "search", "rs", "café", "naïve", "v2"]
        );
        assert_eq!(tokens("Привет мир"), ["Привет", "мир"]);
        assert_eq!(tokens("検索する"), ["検", "索", "す", "る"]);
        assert!(tokens("  -- ").is_empty());
    }

    #[test]
    fn normalize_lowercases_and_caps() {
        let mut out = String::new();
        normalize("ÉCOLE", &mut out);
        assert_eq!(out, "école");
        normalize(&"A".repeat(80), &mut out);
        assert_eq!(out.len(), MAX_TOKEN_BYTES);
        // A multi-byte character never splits at the cap.
        normalize(&"é".repeat(40), &mut out);
        assert!(out.len() <= MAX_TOKEN_BYTES && out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn query_terms_dedupe_in_order() {
        assert_eq!(query_terms("Cargo cargo TEST"), ["cargo", "test"]);
        assert!(query_terms(" ,.; ").is_empty());
    }

    #[test]
    fn match_ranges_cover_original_prefixes() {
        let text = "Ünïcode and UNICODE, unrelated";
        let ranges = match_ranges(text, &["ün".into()]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "Ün");
        let ranges = match_ranges(text, &["uni".into(), "unrel".into()]);
        let found: Vec<_> = ranges.iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(found, ["UNI", "unrel"]);
        // Mid-word text is not a prefix match.
        assert!(match_ranges("prefix", &["fix".into()]).is_empty());
    }

    #[test]
    fn longest_term_wins_within_a_token() {
        let ranges = match_ranges("searching", &["se".into(), "search".into()]);
        assert_eq!(ranges, [(0, 6)]);
    }
}
