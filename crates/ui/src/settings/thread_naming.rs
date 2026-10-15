//! Settings → General → Thread naming: which agent and model title new
//! threads, chosen with the composer's own model picker (title-bound mode).
//! The same model writes commit messages, pull request text and branch
//! names; "Remove agent credits" strips agent trailers from those.

use gpui::{AnyElement, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::registry::TitleSettings;
use zeron_rpc::methods;

use crate::pickers::{Pickers, TitleModelPicked};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub struct ThreadNamingCard {
    state: Entity<AppState>,
    settings: Loadable<TitleSettings>,
    picker: Entity<Pickers>,
    error: Option<String>,
    task: Option<Task<()>>,
    _picked: Subscription,
    _state: Subscription,
}

impl ThreadNamingCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let picker = {
            let state = state.clone();
            cx.new(|cx| Pickers::new_for_titles(state, TitleSettings::default(), cx))
        };
        let picked = cx.subscribe(
            &picker,
            |this: &mut Self, _, event: &TitleModelPicked, cx| {
                this.call(Some(event.0.clone()), cx);
            },
        );
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            state,
            settings: Loadable::Idle,
            picker,
            error: None,
            task: None,
            _picked: picked,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    /// `GetTitleSettings`, or `SetTitleSettings` when saving; either reply is
    /// the device's authoritative settings.
    fn call(&mut self, save: Option<TitleSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_TITLE_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (methods::GET_TITLE_SETTINGS, serde_json::json!({})),
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
                    serde_json::from_value::<TitleSettings>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |card, cx| {
                match result {
                    Ok(settings) => {
                        card.picker.update(cx, |picker, cx| {
                            picker.set_title_settings(settings.clone(), cx)
                        });
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

impl Render for ThreadNamingCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let follows_session = self
            .settings
            .ready()
            .is_some_and(|settings| settings.harness.is_none());
        let control: AnyElement = match &self.settings {
            Loadable::Ready(_) => div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .when(!follows_session, |row| {
                    row.child(
                        widgets::text_action(&theme, widgets::ActionTone::Quiet, "Reset")
                            .id("thread-naming-reset")
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .aria_label("Name threads with the session agent")
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .on_click(cx.listener(|card, _, _, cx| {
                                // Reset the model choice only.
                                let remove_agent_credits = card
                                    .settings
                                    .ready()
                                    .is_none_or(|settings| settings.remove_agent_credits);
                                card.call(
                                    Some(TitleSettings {
                                        remove_agent_credits,
                                        ..TitleSettings::default()
                                    }),
                                    cx,
                                )
                            })),
                    )
                })
                .child(self.picker.clone())
                .into_any_element(),
            Loadable::Error(error) => div()
                .text_color(theme.text_muted)
                .child(error.clone())
                .into_any_element(),
            Loadable::Idle | Loadable::Loading => div()
                .text_color(theme.text_muted)
                .child("Loading…")
                .into_any_element(),
        };
        let description = if follows_session {
            "Each thread is named by its own agent, which also writes commit messages and pull requests."
        } else {
            "A small model keeps titles, commit messages and pull requests fast and cheap."
        };
        let credits = self.settings.ready().cloned();
        let credits_on = credits
            .as_ref()
            .is_none_or(|settings| settings.remove_agent_credits);
        let credits_toggle = widgets::toggle_switch(&theme, credits_on, "remove-agent-credits")
            .id("remove-agent-credits-toggle")
            .flex_none()
            .role(gpui::Role::Switch)
            .aria_label("Remove agent credits")
            .aria_toggled(if credits_on {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .when_some(credits, |el, settings| {
                el.cursor_pointer()
                    .tab_index(0)
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |card, _, _, cx| {
                        card.call(
                            Some(TitleSettings {
                                remove_agent_credits: !settings.remove_agent_credits,
                                ..settings.clone()
                            }),
                            cx,
                        )
                    }))
            })
            .when(self.settings.ready().is_none(), |el| el.opacity(0.5));
        widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(&theme, "Thread naming"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div().child(description).into_any_element()],
                            )),
                    )
                    .child(control),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(&theme, "Remove agent credits"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child("Strip Co-Authored-By and \u{201C}Generated with\u{201D} lines from commit messages and pull requests.")
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(credits_toggle),
            )
    }
}
