//! Prompt recall: Up in an empty composer brings back the prompts you sent in
//! this chat, newest first, and Down walks forward again, like a shell's
//! history. The history is the chat's own transcript, so it survives restarts
//! and follows the chat to other devices; recall itself stays in the composer.

use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};

/// Prompts another chat sent through the MCP tools open with this; they are
/// not something the user typed.
const CHAT_MESSAGE_PREFIX: &str = "[Message from Godspeed chat";

/// Where recall stands: which entry is showing, and its text, so an edit (or
/// a transcript that shifted underneath) is noticed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecallPosition {
    pub index: usize,
    pub recalled: String,
}

/// One step: the position after it (`None` = back at the empty draft) and
/// the text the composer should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecallStep {
    pub position: Option<RecallPosition>,
    pub text: String,
}

/// What a sent user message reads as in the composer: its text without the
/// attachment list the send appended, and with chips that would point at
/// attachments the draft no longer holds turned back into their labels.
/// Empty for attachment-only sends.
pub(crate) fn recallable_prompt(content: &str) -> String {
    let parsed = crate::attachments::parse_user_message_images(content);
    zeron_proto::attachment_mentions::attachment_mention_prompt(&parsed.text)
        .trim()
        .to_owned()
}

/// The prompts the user sent in a transcript, oldest first. Repeats in a row
/// collapse into one entry, as a shell's history does.
pub(crate) fn history_entries(transcript: &[SessionMessageEntry]) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for entry in transcript {
        if entry.role != MessageRole::User {
            continue;
        }
        let content: String = entry
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if content.trim_start().starts_with(CHAT_MESSAGE_PREFIX) {
            continue;
        }
        let prompt = recallable_prompt(&content);
        if prompt.is_empty() || entries.last() == Some(&prompt) {
            continue;
        }
        entries.push(prompt);
    }
    entries
}

/// Step through `entries` from `position`. `back` goes to older prompts.
/// Recall only starts from an empty draft and only continues while the
/// recalled text is untouched; anything else returns `None` and the key
/// keeps its ordinary meaning.
pub(crate) fn step(
    entries: &[String],
    position: Option<&RecallPosition>,
    current: &str,
    back: bool,
) -> Option<RecallStep> {
    let active = position
        .filter(|position| position.recalled == current)
        .and_then(|position| {
            (entries.get(position.index) == Some(&position.recalled))
                .then_some(position.index)
                .or_else(|| entries.iter().rposition(|entry| *entry == position.recalled))
        });
    let target = match (active, back) {
        (None, true) if !current.trim().is_empty() => return None,
        (None, true) => entries.len().checked_sub(1)?,
        (Some(index), true) => index.checked_sub(1)?,
        (None, false) => return None,
        (Some(index), false) if index + 1 >= entries.len() => {
            return Some(RecallStep {
                position: None,
                text: String::new(),
            });
        }
        (Some(index), false) => index + 1,
    };
    let text = entries[target].clone();
    Some(RecallStep {
        position: Some(RecallPosition {
            index: target,
            recalled: text.clone(),
        }),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: format!("{id}-t"),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
            turn_stats: None,
        }
    }

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|entry| (*entry).to_owned()).collect()
    }

    #[test]
    fn history_keeps_what_the_user_typed() {
        let with_image = crate::attachments::with_attachments(
            "what color is this?",
            &["/data/uploads/ab12-red.png".to_string()],
        );
        let image_only = crate::attachments::with_attachments("", &["/a/p.png".to_string()]);
        let chip = format!(
            "compare {} with the logs",
            zeron_proto::attachment_mentions::attachment_mention_link(1, None)
        );
        let transcript = vec![
            message("1", MessageRole::User, "fix the build"),
            message("2", MessageRole::Assistant, "done"),
            message("3", MessageRole::User, "fix the build"),
            message("4", MessageRole::User, &with_image),
            message("5", MessageRole::User, &image_only),
            message(
                "6",
                MessageRole::User,
                "[Message from Godspeed chat Beta (chat-bet)]\nhello",
            ),
            message("7", MessageRole::User, &chip),
            message("8", MessageRole::User, "   "),
        ];
        let history = history_entries(&transcript);
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(history[0], "fix the build");
        assert_eq!(history[1], "what color is this?");
        assert!(history[2].starts_with("compare "));
        assert!(!history[2].contains("]("), "chips read as labels: {}", history[2]);
    }

    #[test]
    fn up_walks_back_and_down_walks_forward_to_an_empty_draft() {
        let history = entries(&["one", "two", "three"]);
        let first = step(&history, None, "", true).unwrap();
        assert_eq!(first.text, "three");
        let second = step(&history, first.position.as_ref(), "three", true).unwrap();
        assert_eq!(second.text, "two");
        let third = step(&history, second.position.as_ref(), "two", true).unwrap();
        assert_eq!(third.text, "one");
        // The oldest prompt is the end of the line.
        assert_eq!(step(&history, third.position.as_ref(), "one", true), None);
        let forward = step(&history, third.position.as_ref(), "one", false).unwrap();
        assert_eq!(forward.text, "two");
        let newest = step(&history, forward.position.as_ref(), "two", false).unwrap();
        let empty = step(&history, newest.position.as_ref(), "three", false).unwrap();
        assert_eq!(empty, RecallStep { position: None, text: String::new() });
    }

    #[test]
    fn a_draft_or_an_edit_keeps_the_arrows_for_the_caret() {
        let history = entries(&["one", "two"]);
        assert_eq!(step(&history, None, "half typed", true), None);
        assert_eq!(step(&history, None, "", false), None, "Down starts nothing");
        let recalled = step(&history, None, "", true).unwrap();
        // Edited after recall: no longer browsing.
        assert_eq!(
            step(&history, recalled.position.as_ref(), "two, edited", true),
            None
        );
        assert_eq!(step(&[], None, "", true), None, "nothing sent yet");
    }

    #[test]
    fn a_transcript_that_grew_meanwhile_keeps_the_place_by_text() {
        let before = entries(&["one", "two"]);
        let at_two = step(&before, None, "", true).unwrap();
        let grown = entries(&["zero", "one", "two", "three"]);
        let older = step(&grown, at_two.position.as_ref(), "two", true).unwrap();
        assert_eq!(older.text, "one");
    }
}
