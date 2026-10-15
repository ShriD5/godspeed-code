//! Settings → General → Usage limits: whether a chat that hits its plan's
//! usage limit resumes on its own once the window resets. The default lives
//! on the engine (`GetLimitRecoverySettings`), which runs the timer — on a
//! headless host too — for the chats it hosts.

use gpui::{Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_proto::LimitRecoverySettings;
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub struct UsageLimitsCard {
    state: Entity<AppState>,
    settings: Loadable<LimitRecoverySettings>,
    error: Option<String>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl UsageLimitsCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Settings can open before the engine connects: load once it does.
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

    /// `GetLimitRecoverySettings`, or `SetLimitRecoverySettings` when saving;
    /// either reply is the engine's authoritative setting.
    fn call(&mut self, save: Option<LimitRecoverySettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_LIMIT_RECOVERY_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (methods::GET_LIMIT_RECOVERY_SETTINGS, serde_json::json!({})),
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
                    serde_json::from_value::<LimitRecoverySettings>(v).map_err(|e| e.to_string())
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
}

impl Render for UsageLimitsCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let ready = self.settings.ready().copied();
        let on = ready.is_some_and(|settings| settings.auto_resume);
        let toggle = widgets::toggle_switch(&theme, on, "auto-resume-limited")
            .id("auto-resume-limited-toggle")
            .flex_none()
            .role(gpui::Role::Switch)
            .aria_label("Resume limited chats at reset")
            .aria_toggled(if on {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .when_some(ready, |el, settings| {
                el.cursor_pointer()
                    .tab_index(0)
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |card, _, _, cx| {
                        card.call(
                            Some(LimitRecoverySettings {
                                auto_resume: !settings.auto_resume,
                            }),
                            cx,
                        )
                    }))
            })
            .when(ready.is_none(), |el| el.opacity(0.5));
        widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(160.0))
                        .child(widgets::row_title(&theme, "Resume limited chats at reset"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![div()
                                .child(
                                    "When a chat hits its plan's usage limit, continue it on the same account once the limit resets. Off, the chat asks first.",
                                )
                                .into_any_element()],
                        )),
                )
                .child(toggle),
        )
    }
}
