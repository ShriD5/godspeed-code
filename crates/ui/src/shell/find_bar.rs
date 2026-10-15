//! Find in thread (⌘F): a small bar over the chat's top-right corner. The
//! transcript owns matching and navigation (`transcript_find`); this is the
//! input, the count, the case toggle and the keys — Enter / Shift+Enter
//! step, Esc closes.
use super::*;

/// The fixed binding; skipped when a remapped app shortcut already uses it.
pub(super) const COMBO: &str = "mod-f";
const BAR_WIDTH: f32 = 360.0;

pub(super) struct FindBar {
    input: Entity<ComposerInput>,
    case_sensitive: bool,
    previous_focus: Option<FocusHandle>,
    focus_pending: bool,
    _events: Subscription,
}

/// The count shown beside the input.
pub(super) fn find_count_label(needle: &str, status: Option<(usize, usize)>) -> String {
    match status {
        _ if needle.is_empty() => String::new(),
        Some((_, 0)) | None => "No results".into(),
        Some((current, total)) => format!("{current} of {total}"),
    }
}

impl Shell {
    /// Open the bar (or, when open, select its text) for the focused chat.
    pub(super) fn open_find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.active_chat.is_empty() {
            return;
        }
        if let Some(bar) = &mut self.find_bar {
            bar.focus_pending = true;
            let input = bar.input.clone();
            input.update(cx, |input, cx| input.select_all_text(cx));
            cx.notify();
            return;
        }
        let input = cx.new(|cx| ComposerInput::with_context("Find in thread", "PaletteSearch", cx));
        let events = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.run_find(cx);
            }
        });
        self.find_bar = Some(FindBar {
            input,
            case_sensitive: false,
            previous_focus: window.focused(cx),
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn run_find(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = &self.find_bar else {
            return;
        };
        let needle = bar.input.read(cx).text().to_owned();
        let case_sensitive = bar.case_sensitive;
        self.transcript.update(cx, |transcript, cx| {
            transcript.set_find_query(&needle, case_sensitive, cx)
        });
        cx.notify();
    }

    fn toggle_find_case(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.find_bar {
            bar.case_sensitive = !bar.case_sensitive;
            self.run_find(cx);
        }
    }

    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.transcript
            .update(cx, |transcript, cx| transcript.find_step(forward, cx));
        cx.notify();
    }

    pub(super) fn close_find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.find_bar.take() else {
            return;
        };
        self.transcript
            .update(cx, |transcript, cx| transcript.close_find(cx));
        match bar.previous_focus {
            Some(focus) => window.focus(&focus, cx),
            None => self.focus_composer(cx),
        }
        cx.notify();
    }

    pub(super) fn render_find_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(self.route, Route::Chat) {
            return None;
        }
        let bar = self.find_bar.as_mut()?;
        if std::mem::take(&mut bar.focus_pending) {
            window.focus(&bar.input.focus_handle(cx), cx);
        }
        let input = bar.input.clone();
        let case_sensitive = bar.case_sensitive;
        let needle = input.read(cx).text().to_owned();
        let status = self.transcript.read(cx).find_status();
        let label = find_count_label(&needle, status);
        let has_matches = status.is_some_and(|(_, total)| total > 0);
        let theme = Theme::of(cx).for_popup();
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label(label)
                .hover(|style| style.bg(crate::theme::hairline(0.08)))
        };
        let case_toggle = button("find-case", "Match case")
            .aria_toggled(if case_sensitive {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(if case_sensitive {
                theme.text
            } else {
                theme.text_muted
            })
            .when(case_sensitive, |el| el.bg(crate::theme::hairline(0.12)))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_find_case(cx)))
            .child("Aa");
        let arrow = |id: &'static str, label: &'static str, glyph: &'static str, forward: bool| {
            button(id, label)
                .when(!has_matches, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| this.step_find(forward, cx)))
                .child(icon(glyph).size(px(14.0)).text_color(theme.text_muted))
        };
        let previous = arrow("find-previous", "Previous match", icons::ALT_ARROW_UP, false);
        let next = arrow("find-next", "Next match", icons::ALT_ARROW_DOWN, true);
        let close = button("find-close", "Close find")
            .on_click(cx.listener(|this, _, window, cx| this.close_find_bar(window, cx)))
            .child(icon(icons::CLOSE).size(px(14.0)).text_color(theme.text_muted));
        Some(
            div()
                .id("find-in-thread")
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + 6.0))
                .right(px(16.0))
                .w(px(BAR_WIDTH))
                .h(px(38.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.border)
                .when(!theme.is_frost(), |el| el.shadow_md())
                .bg(popover::surface_bg(&theme))
                .text_color(theme.text)
                .text_size(crate::typography::ui_rems(13.0))
                .occlude()
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "enter" => this.step_find(!event.keystroke.modifiers.shift, cx),
                        "escape" => this.close_find_bar(window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .child(
                    icon(icons::MAGNIFER)
                        .flex_none()
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().min_w_0().child(input))
                .when(!label.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(label),
                    )
                })
                .child(case_toggle)
                .child(previous)
                .child(next)
                .child(close)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::find_count_label;

    #[test]
    fn count_label_reads_naturally() {
        assert_eq!(find_count_label("", Some((0, 0))), "");
        assert_eq!(find_count_label("x", Some((0, 0))), "No results");
        assert_eq!(find_count_label("x", None), "No results");
        assert_eq!(find_count_label("x", Some((2, 17))), "2 of 17");
    }
}
