//! Settings → General → Time out unanswered questions: after a while, a
//! question nobody answered is parked in the inbox and the agent is told to
//! use its judgement, so unattended runs never stall. Off by default;
//! approvals never time out. Lives on the engine
//! (`GetQuestionTimeoutSettings`), which hosts the chats; chats can
//! override it from their sidebar menu.

use gpui::{Context, Entity, SharedString, Subscription, Task, div, prelude::*, px};
use zeron_proto::attention::QuestionTimeoutSettings;
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// The timeouts offered, in minutes.
pub(crate) const SPANS: [(u32, &str); 4] =
    [(5, "5 min"), (15, "15 min"), (30, "30 min"), (60, "1 hour")];
const DEFAULT_MINUTES: u32 = 15;

pub struct QuestionTimeoutCard {
    state: Entity<AppState>,
    settings: Loadable<QuestionTimeoutSettings>,
    error: Option<String>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl QuestionTimeoutCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            state,
            settings: Loadable::Idle,
            error: None,
            task: None,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    fn call(&mut self, save: Option<QuestionTimeoutSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_QUESTION_TIMEOUT_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (
                methods::GET_QUESTION_TIMEOUT_SETTINGS,
                serde_json::json!({}),
            ),
        };
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<QuestionTimeoutSettings>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |card, cx| {
                match result {
                    Ok(settings) => {
                        card.settings = Loadable::Ready(settings);
                        card.error = None;
                    }
                    Err(error) if matches!(card.settings, Loadable::Ready(_)) => {
                        card.error = Some(error);
                    }
                    Err(error) => card.settings = Loadable::Error(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn save_minutes(&mut self, minutes: Option<u32>, cx: &mut Context<Self>) {
        let Some(current) = self.settings.ready().cloned() else {
            return;
        };
        self.call(
            Some(QuestionTimeoutSettings {
                default_minutes: minutes,
                ..current
            }),
            cx,
        );
    }
}

impl Render for QuestionTimeoutCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let ready = self.settings.ready().cloned();
        let minutes = ready.as_ref().and_then(|settings| settings.default_minutes);
        let on = minutes.is_some();
        let toggle = widgets::toggle_switch(&theme, on, "question-timeout")
            .id("question-timeout-toggle")
            .flex_none()
            .role(gpui::Role::Switch)
            .aria_label("Time out unanswered questions")
            .aria_toggled(if on {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .when(ready.is_some(), |el| {
                el.cursor_pointer()
                    .tab_index(0)
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |card, _, _, cx| {
                        card.save_minutes((!on).then_some(DEFAULT_MINUTES), cx)
                    }))
            })
            .when(ready.is_none(), |el| el.opacity(0.5));
        let mut card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(160.0))
                        .child(widgets::row_title(&theme, "Time out unanswered questions"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![div()
                                .child(
                                    "Tell the agent to use its best judgement when a question waits too long, and keep it in the inbox. Approvals always wait for you. Threads can override this from their menu.",
                                )
                                .into_any_element()],
                        )),
                )
                .child(toggle),
        );
        if let Some(minutes) = minutes {
            let chips = SPANS.into_iter().map(|(span, label)| {
                let chip = if span == minutes {
                    widgets::badge_active(&theme, label)
                } else {
                    widgets::badge(&theme, label)
                        .cursor_pointer()
                        .hover(|style| style.text_color(theme.text))
                };
                chip.id(SharedString::from(format!("question-timeout-{span}m")))
                    .on_click(cx.listener(move |card, _, _, cx| card.save_minutes(Some(span), cx)))
            });
            card = card.child(
                widgets::card_row(&theme, false)
                    .child(div().flex_1().child(widgets::row_title(&theme, "After")))
                    .child(div().flex().flex_row().gap(px(6.0)).children(chips)),
            );
        }
        card
    }
}
