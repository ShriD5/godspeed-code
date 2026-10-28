//! A passage quoted out of the transcript and staged on the composer.
//!
//! Selecting text in a reply offers Ask. The quote stays out of the input and
//! rides the next prompt as a block the agent can see; [`extract_badge`] lifts
//! that same block back out so the sent bubble shows a pill.

use crate::badges::{BadgeDetail, MessageBadge};

/// One quoted passage waiting to be sent with the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub id: String,
    pub quote: String,
}

/// A selection longer than this is clipped. The agent still sees where the
/// passage starts; the composer does not have to swallow a whole transcript.
const MAX_QUOTE_BYTES: usize = 4_000;

pub const QUOTE_ONLY_TEXT: &str = "About this passage:";

pub const PASSAGE_HEADER: &str = "The user is referring to this passage from the conversation:";
pub const PASSAGES_HEADER: &str = "The user is referring to these passages from the conversation:";

impl Citation {
    pub fn new(quote: impl AsRef<str>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            quote: clip_quote(quote.as_ref()),
        }
    }
}

/// Trim a selection and cap it on a char boundary.
pub fn clip_quote(quote: &str) -> String {
    let quote = quote.trim();
    if quote.len() <= MAX_QUOTE_BYTES {
        return quote.to_string();
    }
    let mut end = MAX_QUOTE_BYTES;
    while end > 0 && !quote.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", quote[..end].trim_end())
}

/// One line for the composer chip. Newlines in the quote are pauses, not a
/// second row.
pub fn preview(quote: &str) -> String {
    let flat = quote.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 180;
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let end = flat
        .char_indices()
        .nth(MAX_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(flat.len());
    format!("{}…", flat[..end].trim_end())
}

pub fn chip_label(count: usize) -> String {
    if count == 1 {
        "1 passage".to_string()
    } else {
        format!("{count} passages")
    }
}

/// Append the quotes the agent should answer. An empty draft gets a short
/// lead-in so the message is not only a quote block.
pub fn with_citations(text: &str, citations: &[Citation]) -> String {
    let citations: Vec<_> = citations
        .iter()
        .filter(|citation| !citation.quote.is_empty())
        .collect();
    if citations.is_empty() {
        return text.to_string();
    }
    let body = if text.trim().is_empty() {
        QUOTE_ONLY_TEXT
    } else {
        text
    };
    let header = if citations.len() == 1 {
        PASSAGE_HEADER
    } else {
        PASSAGES_HEADER
    };
    let quoted = citations
        .iter()
        .map(|citation| quote_block(&citation.quote))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{body}\n\n{header}\n{quoted}")
}

fn quote_block(quote: &str) -> String {
    quote
        .lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`crate::badges::Extractor`] for the quote block. Matched only as a whole
/// trailing block, so a prompt that happens to mention the header stays put.
pub fn extract_badge(text: &str) -> Option<(String, MessageBadge)> {
    let markers = [
        format!("\n\n{PASSAGE_HEADER}\n"),
        format!("\n\n{PASSAGES_HEADER}\n"),
    ];
    let (at, marker) = markers
        .iter()
        .filter_map(|marker| text.rfind(marker.as_str()).map(|at| (at, marker)))
        .max_by_key(|(at, _)| *at)?;
    let block = &text[at + marker.len()..];
    let passages = unquote(block)?;
    if passages.is_empty() {
        return None;
    }
    let details = passages
        .iter()
        .map(|passage| BadgeDetail {
            location: "Conversation".into(),
            tag: None,
            body: passage.clone().into(),
        })
        .collect();
    Some((
        text[..at].to_string(),
        MessageBadge {
            icon: crate::icons::CHAT_ROUND_LINE,
            label: chip_label(passages.len()).into(),
            details,
        },
    ))
}

/// `None` when any non-empty line is not a quote. Blank lines separate passages.
fn unquote(block: &str) -> Option<Vec<String>> {
    if block.is_empty() {
        return None;
    }
    let mut passages = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in block.lines() {
        if line.is_empty() {
            if !current.is_empty() {
                passages.push(current.join("\n"));
                current.clear();
            }
            continue;
        }
        let rest = line.strip_prefix('>')?;
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        current.push(rest.to_string());
    }
    if !current.is_empty() {
        passages.push(current.join("\n"));
    }
    (!passages.is_empty()).then_some(passages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{CommentSide, ReviewComment, with_comments};

    #[test]
    fn empty_set_leaves_the_prompt_untouched() {
        assert_eq!(with_citations("ship it", &[]), "ship it");
    }

    #[test]
    fn a_question_keeps_its_words_and_quotes_the_passage() {
        let staged = vec![Citation {
            id: "1".into(),
            quote: "the exact sentence".into(),
        }];
        let out = with_citations("what did you mean?", &staged);
        assert!(out.starts_with("what did you mean?\n\n"));
        assert!(out.contains(PASSAGE_HEADER));
        assert!(out.contains("> the exact sentence"));
        let (text, badge) = extract_badge(&out).unwrap();
        assert_eq!(text, "what did you mean?");
        assert_eq!(badge.label.as_ref(), "1 passage");
        assert_eq!(badge.details[0].body.as_ref(), "the exact sentence");
    }

    #[test]
    fn several_passages_stay_separate() {
        let staged = vec![
            Citation {
                id: "1".into(),
                quote: "first line".into(),
            },
            Citation {
                id: "2".into(),
                quote: "second\nline".into(),
            },
        ];
        let out = with_citations("both", &staged);
        assert!(out.contains(PASSAGES_HEADER));
        assert!(out.contains("> first line\n\n> second\n> line"));
        let (text, badge) = extract_badge(&out).unwrap();
        assert_eq!(text, "both");
        assert_eq!(badge.details.len(), 2);
        assert_eq!(badge.details[1].body.as_ref(), "second\nline");
    }

    #[test]
    fn quote_only_send_gets_a_lead_in() {
        let staged = vec![Citation::new("hello")];
        assert!(with_citations("  ", &staged).starts_with(QUOTE_ONLY_TEXT));
    }

    #[test]
    fn a_header_mentioned_in_the_question_is_left_alone() {
        let text = format!("see {PASSAGE_HEADER} and then keep writing");
        assert!(extract_badge(&text).is_none());
    }

    #[test]
    fn quotes_and_review_comments_both_leave_the_question() {
        let text = with_citations(
            &with_comments(
                "look",
                &[ReviewComment::new("a.rs", CommentSide::New, 3, "fix")],
            ),
            &[Citation::new("hello")],
        );
        let (text, badges) = crate::badges::split(&text);
        assert_eq!(text, "look");
        assert_eq!(badges.len(), 2);
    }

    #[test]
    fn long_quotes_clip_on_a_char_boundary() {
        let quote = format!("héllo{}", "x".repeat(MAX_QUOTE_BYTES));
        let clipped = clip_quote(&quote);
        assert!(clipped.ends_with('…'));
        assert!(clipped.len() <= MAX_QUOTE_BYTES + "…".len());
        assert!(clipped.is_char_boundary(clipped.len()));
    }

    #[test]
    fn preview_flattens_lines() {
        assert_eq!(preview("one\n  two"), "one two");
    }
}
