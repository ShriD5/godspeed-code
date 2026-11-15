//! The composer side of prompt recall (Up/Down through sent prompts) and the
//! prompt stash (the stash shortcut, and the "Stashed" chip that restores or
//! deletes what it set aside).

use chrono::Utc;
use gpui::{AnyElement, Context, MouseButton, SharedString, Window, div, prelude::*, px};

use super::{Composer, StashPrompt, prompt_recall, prompt_stash};
use crate::icons::{self, icon};
use crate::motion;
use crate::popover;
use crate::theme::Theme;

impl Composer {
    /// Up or Down reached recall: show the next prompt along, if recall
    /// applies here (an established chat, no question or queue edit open).
    pub(super) fn step_prompt_recall(&mut self, back: bool, cx: &mut Context<Self>) {
        if self.wizard.is_some() || self.editing_queued.is_some() || self.current_key.is_empty() {
            return;
        }
        let entries = {
            let state = self.state.read(cx);
            if state.selected_chat.as_deref() != Some(self.current_key.as_str()) {
                return;
            }
            prompt_recall::history_entries(&state.transcript)
        };
        let current = self.input.read(cx).text().to_string();
        let Some(step) = prompt_recall::step(&entries, self.recall.as_ref(), &current, back) else {
            return;
        };
        let browsing = step.position.is_some();
        self.recall = step.position;
        self.input.update(cx, |input, cx| {
            input.set_text(step.text, cx);
            // Going back lands at the start so the next Up keeps going; going
            // forward lands at the end so the next Down does.
            if back {
                input.set_caret(0, cx);
            }
            input.recall_browsing = browsing;
        });
        cx.notify();
    }

    /// An edit to a recalled prompt makes it a draft: the arrows move the
    /// caret again.
    pub(super) fn forget_recall_on_edit(&mut self, cx: &mut Context<Self>) {
        if self
            .recall
            .as_ref()
            .is_some_and(|position| self.input.read(cx).text() != position.recalled)
        {
            self.end_recall(cx);
        }
    }

    pub(super) fn end_recall(&mut self, cx: &mut Context<Self>) {
        self.recall = None;
        if self.input.read(cx).recall_browsing {
            self.input
                .update(cx, |input, _| input.recall_browsing = false);
        }
    }

    /// The stash shortcut: set the draft's text and staged attachments aside
    /// and clear the composer. With nothing to stash the key goes on to the
    /// next binding (Save file shares Cmd+S).
    pub(super) fn stash_prompt_action(
        &mut self,
        _: &StashPrompt,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.stash_draft(cx) {
            cx.propagate();
        }
    }

    /// Whether there was a draft to stash.
    pub(super) fn stash_draft(&mut self, cx: &mut Context<Self>) -> bool {
        if self.wizard.is_some() || self.editing_queued.is_some() {
            return false;
        }
        let text = self.input.read(cx).text().to_string();
        let staged = self.staged().to_vec();
        if text.trim().is_empty() && staged.is_empty() {
            return false;
        }
        let key = self.current_key.clone();
        prompt_stash::update(cx, |stash| stash.push(&key, text, &staged));
        self.attachments.remove(&key);
        self.attachment_drafts.remove(&key);
        self.drafts.remove(&key);
        self.end_recall(cx);
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.sync_attachment_chips(cx);
        let count = prompt_stash::entries(&key, cx).len();
        crate::toast::info(cx, format!("Prompt stashed · {count} in this chat's stash"));
        cx.notify();
        true
    }

    /// Bring a stashed prompt back. Into an empty composer it returns as it
    /// was; into a draft it is appended, its chips read as plain labels and
    /// its attachments join as tiles, so no chip number can collide.
    pub(super) fn restore_stashed_prompt(&mut self, id: &str, cx: &mut Context<Self>) {
        let key = self.current_key.clone();
        let Some(restored) = prompt_stash::update(cx, |stash| stash.take(&key, id)) else {
            return;
        };
        let current = self.input.read(cx).text().to_string();
        let empty = current.trim().is_empty() && self.staged().is_empty();
        let (text, attachments) = if empty {
            (restored.text, restored.attachments)
        } else {
            let appended =
                zeron_proto::attachment_mentions::attachment_mention_prompt(&restored.text);
            let text = [current.trim_end().to_owned(), appended]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            let attachments = restored
                .attachments
                .into_iter()
                .map(|attachment| crate::attachments::StagedAttachment {
                    mention: None,
                    ..attachment
                })
                .collect();
            (text, attachments)
        };
        if let Some(highest) = attachments.iter().filter_map(|att| att.mention).max() {
            let draft = self.attachment_drafts.entry(key.clone()).or_default();
            draft.next = draft.next.max(highest);
        }
        if !attachments.is_empty() {
            self.attachments
                .entry(key.clone())
                .or_default()
                .extend(attachments);
        }
        self.end_recall(cx);
        self.input.update(cx, |input, cx| input.set_text(text, cx));
        self.sync_attachment_chips(cx);
        if restored.missing > 0 {
            self.show_error(
                if restored.missing == 1 {
                    "One stashed attachment could not be read back.".to_owned()
                } else {
                    format!(
                        "{} stashed attachments could not be read back.",
                        restored.missing
                    )
                },
                cx,
            );
        }
        if prompt_stash::entries(&key, cx).is_empty() {
            self.stash_menu_open = false;
        }
        self.focus_pending = true;
        cx.notify();
    }

    pub(super) fn delete_stashed_prompt(&mut self, id: &str, cx: &mut Context<Self>) {
        let key = self.current_key.clone();
        prompt_stash::update(cx, |stash| stash.delete(&key, id));
        if prompt_stash::entries(&key, cx).is_empty() {
            self.stash_menu_open = false;
        }
        cx.notify();
    }

    /// The "Stashed (N)" chip above the composer, and its menu while open.
    pub(super) fn render_stash_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.wizard.is_some() || self.editing_queued.is_some() {
            return None;
        }
        let entries = prompt_stash::entries(&self.current_key, cx);
        if entries.is_empty() {
            return None;
        }
        let open = self.stash_menu_open;
        let fade = "composer-stash-chip";
        let menu = open.then(|| self.render_stash_menu(&entries, theme, cx));
        Some(
            div()
                .absolute()
                .top(px(-26.0))
                .left(px(14.0))
                .child(
                    div()
                        .id(fade)
                        .debug_selector(|| "composer-stash-chip".into())
                        .role(gpui::Role::Button)
                        .aria_label("Stashed prompts")
                        .tab_index(0)
                        .focus_visible(|style| style.border_2().border_color(theme.accent))
                        .relative()
                        .h(px(20.0))
                        .px(px(8.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .rounded_full()
                        .cursor_pointer()
                        .bg(if open {
                            theme.glass_hover()
                        } else {
                            motion::hover_blend(
                                fade,
                                theme.glass_hover().opacity(0.6),
                                theme.glass_hover(),
                            )
                        })
                        .on_hover(motion::hover_listener(fade))
                        .text_size(crate::typography::ui_rems(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.stash_menu_open = !this.stash_menu_open;
                            cx.notify();
                        }))
                        .child(SharedString::from(format!("Stashed ({})", entries.len())))
                        .children(menu),
                )
                .into_any_element(),
        )
    }

    fn render_stash_menu(
        &self,
        entries: &[prompt_stash::StashedPrompt],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let mut card = popover::popover_card(theme)
            .w(px(300.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.stash_menu_open = false;
                cx.notify();
            }));
        // Newest first, like the stash itself reads.
        for entry in entries.iter().rev() {
            let restore_id = entry.id.clone();
            let delete_id = entry.id.clone();
            let row_key = format!("composer-stash-{}", entry.id);
            let delete_key = format!("{row_key}-delete");
            card = card.child(
                popover::menu_row(theme, false, row_key.clone())
                    .id(SharedString::from(row_key.clone()))
                    .debug_selector({
                        let row_key = row_key.clone();
                        move || row_key.clone()
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.restore_stashed_prompt(&restore_id, cx)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(SharedString::from(entry.snippet())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(crate::state::format_time_ago(
                                entry.stashed_at,
                                now,
                            ))),
                    )
                    .child(
                        div()
                            .id(SharedString::from(delete_key.clone()))
                            .debug_selector(move || delete_key.clone())
                            .role(gpui::Role::Button)
                            .aria_label("Delete stashed prompt")
                            .flex_none()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .hover(|style| style.bg(theme.glass_hover()))
                            .tooltip(crate::settings::widgets::text_tooltip("Delete"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.delete_stashed_prompt(&delete_id, cx)
                            }))
                            .child(
                                icon(icons::CLOSE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            );
        }
        popover::anchored_menu_above("composer-stash-menu", card.into_any_element(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::composer_focus_window;
    use super::super::{ComposerInput, Up};
    use super::*;
    use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};

    fn sent(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
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

    fn open_chat(composer: &mut Composer, cx: &mut Context<Composer>) {
        composer.state.update(cx, |state, cx| {
            state.select_chat(Some("chat".into()), cx);
            state.apply_transcript(vec![sent("1", "first prompt"), sent("2", "second prompt")]);
            cx.notify();
        });
    }

    fn text(composer: &Composer, cx: &gpui::App) -> String {
        composer.input.read(cx).text().to_string()
    }

    #[gpui::test]
    fn up_at_the_start_of_an_empty_draft_recalls_sent_prompts(cx: &mut gpui::TestAppContext) {
        let (_dir, window) = composer_focus_window(cx);
        window
            .update(cx, |composer, _, cx| open_chat(composer, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |composer, window, cx| {
                assert_eq!(composer.current_key, "chat");
                composer
                    .input
                    .update(cx, |input, cx| input.up(&Up, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |composer, window, cx| {
                assert_eq!(text(composer, cx), "second prompt");
                assert!(composer.input.read(cx).recall_browsing);
                composer
                    .input
                    .update(cx, |input, cx| input.up(&Up, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |composer, _, cx| {
                assert_eq!(text(composer, cx), "first prompt");
                composer.step_prompt_recall(false, cx);
                assert_eq!(text(composer, cx), "second prompt");
                composer.step_prompt_recall(false, cx);
                assert_eq!(text(composer, cx), "", "past the newest: the empty draft");
                assert!(!composer.input.read(cx).recall_browsing);

                // A typed draft keeps Up for the caret.
                composer
                    .input
                    .update(cx, |input, cx| input.set_text("my own words", cx));
                composer.step_prompt_recall(true, cx);
                assert_eq!(text(composer, cx), "my own words");

                // Editing a recalled prompt ends browsing.
                composer
                    .input
                    .update(cx, |input, cx| input.set_text("", cx));
                composer.step_prompt_recall(true, cx);
                composer
                    .input
                    .update(cx, |input, cx| input.set_text("second prompt!", cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |composer, _, cx| {
                assert!(composer.recall.is_none());
                assert!(!composer.input.read(cx).recall_browsing);
            })
            .unwrap();
    }

    #[gpui::test]
    fn up_only_reaches_recall_at_the_start(cx: &mut gpui::TestAppContext) {
        let (_dir, window) = composer_focus_window(cx);
        window
            .update(cx, |composer, _, cx| {
                composer.input.update(cx, |input: &mut ComposerInput, cx| {
                    assert!(input.recall_step_applies(true), "empty draft");
                    assert!(!input.recall_step_applies(false), "Down starts nothing");
                    input.set_text("draft", cx);
                    assert!(!input.recall_step_applies(true), "caret at the end");
                    input.set_caret(0, cx);
                    assert!(input.recall_step_applies(true));
                    input.recall_browsing = true;
                    input.set_caret(5, cx);
                    assert!(input.recall_step_applies(false), "browsing, at the end");
                });
            })
            .unwrap();
    }

    #[gpui::test]
    fn stash_clears_the_draft_persists_and_restores(cx: &mut gpui::TestAppContext) {
        let (dir, window) = composer_focus_window(cx);
        window
            .update(cx, |composer, _, cx| {
                open_chat(composer, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |composer, _, cx| {
                assert!(!composer.stash_draft(cx), "nothing to stash: the key passes on");
                composer
                    .input
                    .update(cx, |input, cx| input.set_text("half a thought", cx));
                composer.attachments.insert(
                    "chat".into(),
                    vec![crate::attachments::stage_pasted_text("notes".into()).unwrap()],
                );
                assert!(composer.stash_draft(cx));
                assert_eq!(text(composer, cx), "");
                assert!(composer.staged().is_empty());
                let entries = prompt_stash::entries("chat", cx);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].attachments.len(), 1);
            })
            .unwrap();
        // On disk, for the next launch.
        let reopened = prompt_stash::PromptStash::load(Some(dir.path().into()));
        assert_eq!(reopened.entries("chat").len(), 1);
        assert_eq!(reopened.entries("chat")[0].text, "half a thought");

        window
            .update(cx, |composer, _, cx| {
                composer
                    .input
                    .update(cx, |input, cx| input.set_text("newer draft", cx));
                assert!(composer.stash_draft(cx));
                let entries = prompt_stash::entries("chat", cx);
                assert_eq!(entries.len(), 2);
                // Restoring into an empty composer brings it back as it was.
                composer.restore_stashed_prompt(&entries[0].id, cx);
                assert_eq!(text(composer, cx), "half a thought");
                assert_eq!(composer.staged().len(), 1);
                // Into a draft it is appended.
                composer.restore_stashed_prompt(&entries[1].id, cx);
                assert_eq!(text(composer, cx), "half a thought\n\nnewer draft");
                assert!(prompt_stash::entries("chat", cx).is_empty());
                assert!(!composer.stash_menu_open);
            })
            .unwrap();
        assert!(
            prompt_stash::PromptStash::load(Some(dir.path().into()))
                .entries("chat")
                .is_empty()
        );
    }

    #[gpui::test]
    fn deleting_a_stashed_prompt_and_purging_a_chat(cx: &mut gpui::TestAppContext) {
        let (_dir, window) = composer_focus_window(cx);
        window
            .update(cx, |composer, _, cx| open_chat(composer, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |composer, _, cx| {
                for draft in ["one", "two"] {
                    composer
                        .input
                        .update(cx, |input, cx| input.set_text(draft, cx));
                    composer.stash_draft(cx);
                }
                let first = prompt_stash::entries("chat", cx)[0].id.clone();
                composer.delete_stashed_prompt(&first, cx);
                assert_eq!(prompt_stash::entries("chat", cx).len(), 1);
                composer.purge_chat("chat", cx);
                assert!(prompt_stash::entries("chat", cx).is_empty());
            })
            .unwrap();
    }
}
