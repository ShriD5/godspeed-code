//! `/clear`: the chat's next message starts a fresh provider session that
//! doesn't see the earlier messages ([`zeron_proto::FRESH_SESSION_OPTION`]).
//! The banner above the composer says so until that send, and dismissing it
//! keeps the current session.
use super::*;
use crate::settings::widgets::{self, ActionTone};

impl Composer {
    pub(super) fn arm_fresh_session(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        if !self
            .state
            .read(cx)
            .chat_host_supports(&chat_id, capabilities::FRESH_SESSION_V1)
        {
            self.failure = Some("Update the chat's engine to start a fresh session.".into());
            self.failure_key = Some(self.current_key.clone());
            return;
        }
        self.fresh_sessions.insert(chat_id);
    }

    pub(super) fn render_fresh_session_banner(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let chat_id = self.state.read(cx).selected_chat.clone()?;
        if !self.fresh_sessions.contains(&chat_id) {
            return None;
        }
        let keep = widgets::text_action(theme, ActionTone::Quiet, "Keep the current session")
            .id("fresh-session-keep")
            .tab_index(0)
            .role(gpui::Role::Button)
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.fresh_sessions.remove(&chat_id);
                cx.notify();
            }));
        Some(
            crate::motion::fade_in(
                "composer-fresh-session-banner",
                div()
                    .id("composer-fresh-session-banner")
                    .debug_selector(|| "composer-fresh-session-banner".into())
                    .mx(px(4.0))
                    .mt(px(6.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.text_muted.opacity(0.22))
                    .bg(theme.text_muted.opacity(0.06))
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(px(16.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child("Your next message starts a fresh session"),
                            )
                            .child(
                                div()
                                    .text_color(theme.text_muted)
                                    .whitespace_normal()
                                    .child(
                                        "The agent won't see the earlier messages in this chat.",
                                    ),
                            ),
                    )
                    .child(keep),
            )
            .into_any_element(),
        )
    }
}
