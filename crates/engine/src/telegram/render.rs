//! Text the bot sends: agent replies split to Telegram's message limit, and
//! approval / question cards. Agent replies go out as plain text (model
//! Markdown would trip Telegram's parser); cards are small HTML with every
//! interpolated value escaped.

use zeron_proto::{ApprovalDecision, ApprovalKind, ApprovalRequest, UserInputQuestion};

use super::api::{Button, Keyboard};
use super::callback::Callback;

/// Telegram's limit, counted in UTF-16 code units.
pub(crate) const MAX_MESSAGE: usize = 4096;
const MAX_COMMAND: usize = 2000;
const MAX_PATHS: usize = 10;
const MAX_PATH: usize = 200;
const MAX_DIFF_FILES: usize = 10;
const MAX_DIFF_PREVIEW_LINES: usize = 12;
const MAX_PREVIEW_LINE: usize = 100;

/// Split `text` into messages Telegram accepts, preferring to break at a
/// newline in the back half of each window.
pub(crate) fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut units = 0;
        let mut cut = rest.len();
        for (index, ch) in rest.char_indices() {
            if units + ch.len_utf16() > MAX_MESSAGE {
                cut = index;
                break;
            }
            units += ch.len_utf16();
        }
        if cut < rest.len()
            && let Some(newline) = rest[..cut].rfind('\n')
            && newline >= cut / 2
        {
            cut = newline + 1;
        }
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out
}

pub(crate) fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncated(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// The button label for a provider decision.
pub(crate) fn decision_label(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approve => "Allow once",
        ApprovalDecision::ApproveForSession => "Allow for session",
        ApprovalDecision::ApproveAlways => "Always allow",
        ApprovalDecision::Decline => "Deny",
        ApprovalDecision::DeclineAlways => "Always deny",
        ApprovalDecision::Cancel => "Deny and stop",
    }
}

/// The line appended to a card once it is answered.
pub(crate) fn outcome_line(question: &UserInputQuestion, label: &str) -> String {
    let decision = question
        .approval
        .as_ref()
        .and_then(|approval| approval.decision_for(label));
    match decision {
        Some(decision) if decision.approves() => format!("✅ {}", decision_label(decision)),
        Some(decision) => format!("❌ {}", decision_label(decision)),
        None => format!("💬 {}", escape_html(&truncated(label, 300))),
    }
}

/// Files touched by a unified diff with their added/removed line counts.
pub(crate) fn diff_stats(diff: &str) -> Vec<(String, usize, usize)> {
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.trim_start_matches("b/").trim();
            if path != "/dev/null" {
                files.push((path.to_string(), 0, 0));
            }
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("diff ") {
            continue;
        }
        let added = line.starts_with('+');
        if !added && !line.starts_with('-') {
            continue;
        }
        if files.is_empty() {
            files.push((String::new(), 0, 0));
        }
        let file = files.last_mut().expect("a file to count against");
        if added {
            file.1 += 1;
        } else {
            file.2 += 1;
        }
    }
    files
}

fn diff_block(diff: &str) -> String {
    let mut out = String::new();
    let stats = diff_stats(diff);
    for (path, added, removed) in stats.iter().take(MAX_DIFF_FILES) {
        let name = if path.is_empty() { "(file)" } else { path };
        out.push_str(&format!("{name}  +{added} −{removed}\n"));
    }
    if stats.len() > MAX_DIFF_FILES {
        out.push_str(&format!(
            "… and {} more files\n",
            stats.len() - MAX_DIFF_FILES
        ));
    }
    let preview: Vec<String> = diff
        .lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-'))
                && !l.starts_with("+++")
                && !l.starts_with("---")
        })
        .take(MAX_DIFF_PREVIEW_LINES)
        .map(|l| truncated(l, MAX_PREVIEW_LINE))
        .collect();
    if !preview.is_empty() {
        out.push('\n');
        out.push_str(&preview.join("\n"));
    }
    out
}

fn approval_body(approval: &ApprovalRequest) -> String {
    let mut out = format!(
        "<b>{}</b>: {}",
        escape_html(approval.kind.label()),
        escape_html(&truncated(&approval.title, 300))
    );
    if let Some(command) = &approval.command {
        out.push_str(&format!(
            "\n<pre>{}</pre>",
            escape_html(&truncated(command, MAX_COMMAND))
        ));
    }
    if let Some(cwd) = &approval.cwd {
        out.push_str(&format!(
            "\nin <code>{}</code>",
            escape_html(&truncated(cwd, MAX_PATH))
        ));
    }
    let show_paths = approval.kind != ApprovalKind::FileChange || approval.diff.is_none();
    if show_paths && !approval.paths.is_empty() {
        let mut paths: Vec<String> = approval
            .paths
            .iter()
            .take(MAX_PATHS)
            .map(|p| format!("<code>{}</code>", escape_html(&truncated(p, MAX_PATH))))
            .collect();
        if approval.paths.len() > MAX_PATHS {
            paths.push(format!("and {} more", approval.paths.len() - MAX_PATHS));
        }
        out.push_str(&format!("\nFiles: {}", paths.join(", ")));
    }
    if let Some(diff) = &approval.diff {
        out.push_str(&format!("\n<pre>{}</pre>", escape_html(&diff_block(diff))));
    }
    if let Some(reason) = &approval.reason {
        out.push_str(&format!(
            "\nReason: {}",
            escape_html(&truncated(reason, 400))
        ));
    }
    out
}

/// The card for question `index` of `total` in one input request.
pub(crate) fn prompt_card(
    chat_title: &str,
    question: &UserInputQuestion,
    index: usize,
    total: usize,
) -> String {
    let mut out = match &question.approval {
        Some(approval) => format!(
            "🔐 <b>Approval needed</b> · {}\n{}",
            escape_html(chat_title),
            approval_body(approval)
        ),
        None => {
            let mut card = format!(
                "❓ <b>{}</b> · {}\n{}",
                escape_html(&question.header),
                escape_html(chat_title),
                escape_html(&truncated(&question.question, 2000))
            );
            if question.options.is_empty() {
                card.push_str("\n\nReply to this message with your answer.");
            } else {
                card.push_str("\n\nOr reply to this message in your own words.");
            }
            card
        }
    };
    if total > 1 {
        out.push_str(&format!("\n({} of {total})", index + 1));
    }
    out
}

/// Buttons for one question: approvals in two rows (allow / deny), plain
/// options one per row.
pub(crate) fn prompt_keyboard(
    prompt_id: i64,
    index: usize,
    question: &UserInputQuestion,
) -> Keyboard {
    let button = |option: usize, label: &str| {
        Button::new(
            label,
            Callback::Answer {
                prompt: prompt_id,
                question: index,
                option,
            }
            .encode(),
        )
    };
    match &question.approval {
        Some(approval) => {
            let (allow, deny): (Vec<_>, Vec<_>) = approval
                .decisions
                .iter()
                .enumerate()
                .partition(|(_, option)| option.decision.approves());
            [allow, deny]
                .into_iter()
                .filter(|row| !row.is_empty())
                .map(|row| {
                    row.into_iter()
                        .map(|(option, choice)| button(option, decision_label(choice.decision)))
                        .collect()
                })
                .collect()
        }
        None => question
            .options
            .iter()
            .enumerate()
            .map(|(option, label)| vec![button(option, &truncated(label, 60))])
            .collect(),
    }
}

/// The answer label button `option` stands for.
pub(crate) fn option_label(question: &UserInputQuestion, option: usize) -> Option<String> {
    match &question.approval {
        Some(approval) => approval.decisions.get(option).map(|o| o.label.clone()),
        None => question.options.get(option).cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk_and_empty_is_none() {
        assert_eq!(chunks("hello"), vec!["hello".to_string()]);
        assert!(chunks("").is_empty());
    }

    #[test]
    fn long_text_splits_at_the_limit_and_keeps_every_char() {
        let text = "x".repeat(MAX_MESSAGE * 2 + 10);
        let parts = chunks(&text);
        assert_eq!(parts.len(), 3);
        assert!(
            parts
                .iter()
                .all(|p| p.encode_utf16().count() <= MAX_MESSAGE)
        );
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn splits_prefer_newlines_and_count_utf16_units() {
        let line = format!("{}\n", "a".repeat(3000));
        let text = format!("{line}{line}");
        let parts = chunks(&text);
        assert_eq!(parts, vec![line.clone(), line.clone()]);

        let emoji = "😀".repeat(MAX_MESSAGE);
        let parts = chunks(&emoji);
        assert!(
            parts
                .iter()
                .all(|p| p.encode_utf16().count() <= MAX_MESSAGE)
        );
        assert_eq!(parts.len(), 2);
        assert_eq!(parts.concat(), emoji);
    }

    #[test]
    fn approval_cards_show_the_exact_command_escaped() {
        let mut approval = ApprovalRequest::new(ApprovalKind::Command, "Clean <build>");
        approval.command = Some("rm -rf build/ && echo '<done>'".into());
        approval.cwd = Some("/work/app".into());
        let question = approval.into_question("q1");
        let card = prompt_card("Fix CI", &question, 0, 1);
        assert!(
            card.contains("<pre>rm -rf build/ &amp;&amp; echo '&lt;done&gt;'</pre>"),
            "{card}"
        );
        assert!(card.contains("in <code>/work/app</code>"));
        assert!(card.contains("Clean &lt;build&gt;"));
        let keyboard = prompt_keyboard(7, 0, &question);
        let labels: Vec<Vec<&str>> = keyboard
            .iter()
            .map(|row| row.iter().map(|b| b.text.as_str()).collect())
            .collect();
        assert_eq!(
            labels,
            vec![vec!["Allow once", "Allow for session"], vec!["Deny"]]
        );
        assert_eq!(keyboard[1][0].callback_data, "a:7:0:2");
        assert_eq!(option_label(&question, 2).as_deref(), Some("Decline"));
        assert_eq!(outcome_line(&question, "Decline"), "❌ Deny");
        assert_eq!(outcome_line(&question, "Approve"), "✅ Allow once");
    }

    #[test]
    fn edit_approvals_summarize_the_diff() {
        let mut approval = ApprovalRequest::new(ApprovalKind::FileChange, "Edit files");
        approval.paths = vec!["src/lib.rs".into()];
        approval.diff = Some(
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n context\n"
                .into(),
        );
        assert_eq!(
            diff_stats(approval.diff.as_deref().unwrap()),
            vec![("src/lib.rs".to_string(), 2, 1)]
        );
        let card = prompt_card("Refactor", &approval.into_question("q"), 0, 1);
        assert!(card.contains("src/lib.rs  +2 −1"), "{card}");
        assert!(card.contains("+more"), "{card}");
    }

    #[test]
    fn questions_list_options_and_invite_free_text() {
        let question = UserInputQuestion {
            id: "q".into(),
            header: "Pick one".into(),
            question: "Which database?".into(),
            options: vec!["Postgres".into(), "SQLite".into()],
            multi_select: false,
            prefill: None,
            multiline: false,
            approval: None,
        };
        let card = prompt_card("Plan", &question, 1, 2);
        assert!(
            card.contains("Which database?") && card.contains("(2 of 2)"),
            "{card}"
        );
        let keyboard = prompt_keyboard(3, 1, &question);
        assert_eq!(keyboard.len(), 2);
        assert_eq!(keyboard[1][0].callback_data, "a:3:1:1");
        assert_eq!(outcome_line(&question, "SQLite"), "💬 SQLite");
    }
}
