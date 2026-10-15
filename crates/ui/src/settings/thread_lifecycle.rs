//! Settings → General → Settle inactive threads: whether threads with no
//! activity for a while move to the sidebar's Settled shelf on their own.
//! Off by default. The setting lives on the engine
//! (`GetChatLifecycleSettings`), whose timer settles the chats it hosts.

use gpui::{Context, Entity, SharedString, Subscription, Task, div, prelude::*, px};
use zeron_proto::ChatLifecycleSettings;
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// The idle spans offered, in days.
const SPANS: [(u32, &str); 3] = [(1, "1 day"), (3, "3 days"), (7, "1 week")];
const DEFAULT_DAYS: u32 = 3;

pub struct ThreadLifecycleCard {
    state: Entity<AppState>,
    settings: Loadable<ChatLifecycleSettings>,
    error: Option<String>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl ThreadLifecycleCard {
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

    /// `GetChatLifecycleSettings`, or `SetChatLifecycleSettings` when
    /// saving; either reply is the engine's authoritative setting.
    fn call(&mut self, save: Option<ChatLifecycleSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_CHAT_LIFECYCLE_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (methods::GET_CHAT_LIFECYCLE_SETTINGS, serde_json::json!({})),
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
                    serde_json::from_value::<ChatLifecycleSettings>(v).map_err(|e| e.to_string())
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

    fn save_days(&mut self, days: Option<u32>, cx: &mut Context<Self>) {
        self.call(
            Some(ChatLifecycleSettings {
                auto_settle_after_days: days,
            }),
            cx,
        );
    }
}

impl Render for ThreadLifecycleCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let ready = self.settings.ready().copied();
        let days = ready.and_then(|settings| settings.auto_settle_after_days);
        let on = days.is_some();
        let toggle = widgets::toggle_switch(&theme, on, "auto-settle-threads")
            .id("auto-settle-threads-toggle")
            .flex_none()
            .role(gpui::Role::Switch)
            .aria_label("Settle inactive threads")
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
                        card.save_days((!on).then_some(DEFAULT_DAYS), cx)
                    }))
            })
            .when(ready.is_none(), |el| el.opacity(0.5));
        let mut card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(160.0))
                        .child(widgets::row_title(&theme, "Settle inactive threads"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![div()
                                .child(
                                    "Move threads with no activity for a while to Settled. Pinned, unread and running threads stay put, and new activity brings a thread back.",
                                )
                                .into_any_element()],
                        )),
                )
                .child(toggle),
        );
        if let Some(days) = days {
            let chips = SPANS.into_iter().map(|(span, label)| {
                let chip = if span == days {
                    widgets::badge_active(&theme, label)
                } else {
                    widgets::badge(&theme, label)
                        .cursor_pointer()
                        .hover(|style| style.text_color(theme.text))
                };
                chip.id(SharedString::from(format!("auto-settle-{span}d")))
                    .on_click(cx.listener(move |card, _, _, cx| card.save_days(Some(span), cx)))
            });
            card = card.child(
                widgets::card_row(&theme, false)
                    .child(
                        div()
                            .flex_1()
                            .child(widgets::row_title(&theme, "After no activity for")),
                    )
                    .child(div().flex().flex_row().gap(px(6.0)).children(chips)),
            );
        }
        card
    }
}
