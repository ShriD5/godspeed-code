//! The chat row menu's Question timeout submenu: this thread's override of
//! the device default (Settings → General), set on the engine hosting it.

use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};
use zeron_proto::attention::{ChatQuestionTimeout, QuestionTimeoutSettings};
use zeron_rpc::methods;

use super::{CHAT_MENU_ROW_QUESTION_TIMEOUT, ChatMenuPage, Shell};
use crate::icons::{self, icon};
use crate::popover;
use crate::settings::question_timeout::SPANS;
use crate::theme::Theme;

/// One submenu choice: `None` follows the device default.
type Choice = Option<ChatQuestionTimeout>;

pub(super) fn choice_label(choice: Choice, default_minutes: Option<u32>) -> String {
    match choice {
        None => match default_minutes {
            Some(minutes) => format!("Default ({})", minutes_label(minutes)),
            None => "Default (off)".into(),
        },
        Some(ChatQuestionTimeout::Off) => "Never".into(),
        Some(ChatQuestionTimeout::After { minutes }) => minutes_label(minutes),
    }
}

fn minutes_label(minutes: u32) -> String {
    SPANS
        .iter()
        .find(|(span, _)| *span == minutes)
        .map(|(_, label)| label.to_string())
        .unwrap_or_else(|| format!("{minutes} min"))
}

impl Shell {
    /// The Question timeout trigger, for chats this device hosts (the
    /// setting lives on the hosting engine).
    pub(super) fn render_question_timeout_row(
        &mut self,
        menu: gpui::Div,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let local = {
            let state = self.state.read(cx);
            state.chats.iter().any(|chat| {
                chat.id == chat_id
                    && !chat.archived
                    && Some(&chat.device_id) == state.local_device_id.as_ref()
            })
        };
        if !local {
            return menu;
        }
        if self.question_timeouts.is_none() {
            self.load_question_timeouts(cx);
        }
        let trigger = self.render_chat_submenu_trigger(
            chat_id,
            ChatMenuPage::QuestionTimeout,
            icons::CLOCK_CIRCLE,
            "Question timeout",
            theme,
            cx,
        );
        menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_QUESTION_TIMEOUT, trigger, cx))
    }

    fn load_question_timeouts(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.question_timeouts = Some(QuestionTimeoutSettings::default());
        cx.spawn(async move |this, cx| {
            let settings = engine
                .client()
                .call(
                    methods::GET_QUESTION_TIMEOUT_SETTINGS,
                    serde_json::json!({}),
                )
                .await
                .ok()
                .and_then(|v| serde_json::from_value::<QuestionTimeoutSettings>(v).ok());
            this.update(cx, |shell, cx| {
                if let Some(settings) = settings {
                    shell.question_timeouts = Some(settings);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn set_chat_question_timeout(
        &mut self,
        chat_id: String,
        choice: Choice,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({ "chatId": chat_id, "timeout": choice });
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_CHAT_QUESTION_TIMEOUT, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<QuestionTimeoutSettings>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |shell, cx| match result {
                Ok(settings) => {
                    shell.question_timeouts = Some(settings);
                    cx.notify();
                }
                Err(err) => shell.show_notice(crate::toast::ToastKind::Error, err, cx),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_question_timeout_submenu(
        &mut self,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = self.question_timeouts.clone().unwrap_or_default();
        let current = settings.chats.get(chat_id).copied();
        let choices: Vec<Choice> = std::iter::once(None)
            .chain(std::iter::once(Some(ChatQuestionTimeout::Off)))
            .chain(
                SPANS
                    .iter()
                    .map(|(minutes, _)| Some(ChatQuestionTimeout::After { minutes: *minutes })),
            )
            .collect();
        let mut card = popover::popover_card(theme)
            .w(px(200.0))
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (ix, choice) in choices.into_iter().enumerate() {
            let chosen = choice == current;
            let id = chat_id.to_owned();
            card = card.child(
                popover::menu_row(
                    theme,
                    chosen,
                    format!("chat-question-timeout-{ix}-{chat_id}"),
                )
                .id(("chat-question-timeout", ix))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_chat_question_timeout(id.clone(), choice, cx)
                }))
                .child(div().flex_1().child(SharedString::from(choice_label(
                    choice,
                    settings.default_minutes,
                ))))
                .when(chosen, |el| {
                    el.child(
                        icon(icons::CHECK)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                }),
            );
        }
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_name_the_default_and_the_spans() {
        assert_eq!(choice_label(None, None), "Default (off)");
        assert_eq!(choice_label(None, Some(15)), "Default (15 min)");
        assert_eq!(
            choice_label(Some(ChatQuestionTimeout::Off), Some(15)),
            "Never"
        );
        assert_eq!(
            choice_label(Some(ChatQuestionTimeout::After { minutes: 60 }), None),
            "1 hour"
        );
        assert_eq!(
            choice_label(Some(ChatQuestionTimeout::After { minutes: 7 }), None),
            "7 min"
        );
    }
}
