//! The body of an approval card — the agent's reason, the exact command,
//! where it runs, the files it touches and the edit's diff — shared by the
//! composer's approval card and the inbox, so a request reads the same
//! wherever it is answered.

use gpui::{AnyElement, ElementId, SharedString, div, prelude::*, px};
use zeron_proto::ApprovalRequest;

use crate::theme::Theme;

const COMMAND_MAX_HEIGHT: f32 = 12.0 * 18.0 + 16.0;
const DIFF_MAX_HEIGHT: f32 = 220.0;
const MAX_PATHS: usize = 6;
const MAX_DIFF_LINES: usize = 400;

/// Reason, command, cwd, files and diff, in that order. `key` prefixes the
/// element ids so several cards can share one view (the composer's card
/// uses `"approval"`).
pub(crate) fn approval_details(
    approval: &ApprovalRequest,
    theme: &Theme,
    key: &str,
) -> Vec<AnyElement> {
    let mono = theme.font_mono.clone();
    let mut out = Vec::new();
    if let Some(reason) = &approval.reason {
        out.push(
            div()
                .mt(px(4.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted.opacity(0.75))
                .child(SharedString::from(reason.clone()))
                .into_any_element(),
        );
    }
    // The command is what the user is approving, so all of it must be
    // readable in place: `w_full` + `min_w_0` give the text a definite
    // width to wrap against (gpui breaks mid-token when a pipeline or a
    // path has no space to break at) instead of running off the card's
    // edge, and a tall heredoc scrolls inside its own box rather than
    // shoving the decision buttons out of the window (T3 16197).
    if let Some(command) = &approval.command {
        out.push(
            div()
                .id(SharedString::from(format!("{key}-command")))
                .w_full()
                .min_w_0()
                .max_h(px(COMMAND_MAX_HEIGHT))
                .overflow_y_scroll()
                .mt(px(10.0))
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(10.0))
                .bg(crate::theme::ink(0.05))
                .font_family(mono.clone())
                .text_size(crate::typography::ui_rems(12.5))
                .line_height(px(18.0))
                .text_color(theme.text)
                .whitespace_normal()
                .child(SharedString::from(command.clone()))
                .into_any_element(),
        );
    }
    let meta = |label: &'static str, value: String| {
        div()
            .mt(px(6.0))
            .flex()
            .flex_row()
            .gap(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.65))
                    .child(SharedString::from(label)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(mono.clone())
                    .text_color(theme.text_muted)
                    .child(SharedString::from(value)),
            )
            .into_any_element()
    };
    if let Some(cwd) = &approval.cwd {
        out.push(meta("in", cwd.clone()));
    }
    if !approval.paths.is_empty() {
        let mut shown: Vec<String> = approval.paths.iter().take(MAX_PATHS).cloned().collect();
        if approval.paths.len() > MAX_PATHS {
            shown.push(format!("+{} more", approval.paths.len() - MAX_PATHS));
        }
        out.push(meta("files", shown.join("\n")));
    }
    if let Some(diff) = &approval.diff {
        let line_key = SharedString::from(format!("{key}-diff-line"));
        let lines = diff
            .lines()
            .take(MAX_DIFF_LINES)
            .enumerate()
            .map(|(ix, line)| {
                div()
                    .id(ElementId::from((line_key.clone(), ix)))
                    .text_color(diff_line_color(line, theme))
                    .child(SharedString::from(if line.is_empty() {
                        " ".to_owned()
                    } else {
                        line.to_owned()
                    }))
            });
        out.push(
            div()
                .id(SharedString::from(format!("{key}-diff")))
                .mt(px(10.0))
                .max_h(px(DIFF_MAX_HEIGHT))
                .overflow_y_scroll()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(10.0))
                .bg(crate::theme::ink(0.05))
                .font_family(mono)
                .text_size(crate::typography::ui_rems(12.0))
                .line_height(px(17.0))
                .children(lines)
                .into_any_element(),
        );
    }
    out
}

fn diff_line_color(line: &str, theme: &Theme) -> gpui::Hsla {
    if line.starts_with("+++") || line.starts_with("---") {
        theme.text_muted
    } else if line.starts_with('+') {
        theme.success
    } else if line.starts_with('-') {
        theme.danger
    } else if line.starts_with("@@") {
        theme.accent
    } else {
        theme.text.opacity(0.85)
    }
}
