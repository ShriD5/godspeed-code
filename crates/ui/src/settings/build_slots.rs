//! Settings → General → Builds: "Share build slots across agents" (one
//! jobserver for every agent and terminal, with its slot count) and "Use
//! sccache when available". The setting lives on the engine
//! (`GetBuildSlotsSettings`), which runs the pool.

use gpui::{Context, Entity, SharedString, Subscription, Task, div, prelude::*, px};
use zeron_proto::{BuildSlotsSettings, BuildSlotsSettingsUpdate, MAX_BUILD_SLOTS};
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// The share row's explanation.
pub const SHARE_HELP: &str = "Parallel agents draw compile jobs (cargo, make, ninja) from one pool, so several builds at once don't swamp memory. One build alone runs as fast as before.";

/// The sccache row's explanation when sccache isn't installed.
pub const SCCACHE_MISSING_HELP: &str = "Worktrees of one Rust repo reuse compiled dependencies. Install with brew install sccache; Godspeed picks it up on its own.";

/// The sccache row's explanation when it is.
pub const SCCACHE_HELP: &str = "Worktrees of one Rust repo reuse compiled dependencies (sets RUSTC_WRAPPER for agents and terminals in Rust projects).";

/// The slot count after one press of − or +, within 1..=MAX.
pub fn stepped(current: u32, up: bool) -> u32 {
    if up {
        (current + 1).min(MAX_BUILD_SLOTS)
    } else {
        current.saturating_sub(1).max(1)
    }
}

/// The share row's explanation for this state.
pub fn share_description(settings: &BuildSlotsSettings) -> String {
    if !settings.supported {
        return "Not available on this platform yet.".to_owned();
    }
    match &settings.paused_by {
        Some(var) => {
            format!("{var} is set in your environment, so your own build parallelism wins.")
        }
        None => SHARE_HELP.to_owned(),
    }
}

/// The sccache row's explanation for this state.
pub fn sccache_description(settings: &BuildSlotsSettings) -> String {
    match (&settings.sccache_path, &settings.sccache_paused_by) {
        (_, Some(var)) => {
            format!("{var} is set in your environment, so your own rustc wrapper wins.")
        }
        (Some(_), None) => SCCACHE_HELP.to_owned(),
        (None, None) => SCCACHE_MISSING_HELP.to_owned(),
    }
}

pub struct BuildSlotsCard {
    state: Entity<AppState>,
    settings: Loadable<BuildSlotsSettings>,
    error: Option<String>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl BuildSlotsCard {
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

    /// `GetBuildSlotsSettings`, or `SetBuildSlotsSettings` when saving;
    /// either reply is the engine's authoritative setting.
    fn call(&mut self, save: Option<BuildSlotsSettingsUpdate>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(update) => (
                methods::SET_BUILD_SLOTS_SETTINGS,
                serde_json::to_value(update).unwrap_or_default(),
            ),
            None => (methods::GET_BUILD_SLOTS_SETTINGS, serde_json::json!({})),
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
                    serde_json::from_value::<BuildSlotsSettings>(v).map_err(|e| e.to_string())
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

fn switch(
    theme: &Theme,
    on: bool,
    enabled: bool,
    key: &'static str,
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    widgets::toggle_switch(theme, on, key)
        .id(SharedString::from(format!("{key}-toggle")))
        .flex_none()
        .role(gpui::Role::Switch)
        .aria_label(label)
        .aria_toggled(if on {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .when(!enabled, |el| el.opacity(0.5))
}

fn text_row(theme: &Theme, title: &'static str, description: String) -> gpui::Div {
    div()
        .flex_1()
        .min_w(px(160.0))
        .child(widgets::row_title(theme, title))
        .child(widgets::meta_line(
            theme,
            vec![div().child(description).into_any_element()],
        ))
}

impl Render for BuildSlotsCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let ready = self.settings.ready().cloned();
        let share_on = ready.as_ref().is_some_and(|s| s.share_slots);
        let share_enabled = ready.as_ref().is_some_and(|s| s.supported);
        let sccache_on = ready.as_ref().is_some_and(|s| s.use_sccache);
        let share_toggle = switch(
            &theme,
            share_on,
            share_enabled,
            "share-build-slots",
            "Share build slots across agents",
        )
        .when(share_enabled, |el| {
            el.cursor_pointer()
                .tab_index(0)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |card, _, _, cx| {
                    card.call(
                        Some(BuildSlotsSettingsUpdate {
                            share_slots: Some(!share_on),
                            ..Default::default()
                        }),
                        cx,
                    )
                }))
        });
        let sccache_toggle = switch(
            &theme,
            sccache_on,
            ready.is_some(),
            "use-sccache",
            "Use sccache when available",
        )
        .when(ready.is_some(), |el| {
            el.cursor_pointer()
                .tab_index(0)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |card, _, _, cx| {
                    card.call(
                        Some(BuildSlotsSettingsUpdate {
                            use_sccache: Some(!sccache_on),
                            ..Default::default()
                        }),
                        cx,
                    )
                }))
        });
        let share_text = ready
            .as_ref()
            .map(share_description)
            .unwrap_or_else(|| SHARE_HELP.to_owned());
        let sccache_text = ready
            .as_ref()
            .map(sccache_description)
            .unwrap_or_else(|| SCCACHE_MISSING_HELP.to_owned());

        let mut card = widgets::section_card(&theme).mt_0().child(
            widgets::card_row(&theme, true)
                .child(text_row(
                    &theme,
                    "Share build slots across agents",
                    share_text,
                ))
                .child(share_toggle),
        );
        if let Some(settings) = ready.as_ref().filter(|s| s.share_slots && s.supported) {
            let slots = settings.effective_slots();
            let automatic = settings.slots == 0;
            let step = |up: bool| {
                let label = if up { "+" } else { "−" };
                let id = if up {
                    "build-slots-more"
                } else {
                    "build-slots-fewer"
                };
                let at_edge = if up {
                    slots >= MAX_BUILD_SLOTS
                } else {
                    slots <= 1
                };
                widgets::badge(&theme, label)
                    .id(id)
                    .role(gpui::Role::Button)
                    .aria_label(if up {
                        "More build slots"
                    } else {
                        "Fewer build slots"
                    })
                    .when(!at_edge, |el| {
                        el.cursor_pointer()
                            .hover(|style| style.text_color(theme.text))
                            .on_click(cx.listener(move |card, _, _, cx| {
                                card.call(
                                    Some(BuildSlotsSettingsUpdate {
                                        slots: Some(stepped(slots, up)),
                                        ..Default::default()
                                    }),
                                    cx,
                                )
                            }))
                    })
                    .when(at_edge, |el| el.opacity(0.5))
            };
            let auto_chip = if automatic {
                widgets::badge_active(&theme, format!("Auto · {}", settings.default_slots))
                    .id("build-slots-auto")
            } else {
                widgets::badge(&theme, format!("Auto · {}", settings.default_slots))
                    .id("build-slots-auto")
                    .cursor_pointer()
                    .hover(|style| style.text_color(theme.text))
                    .on_click(cx.listener(|card, _, _, cx| {
                        card.call(
                            Some(BuildSlotsSettingsUpdate {
                                slots: Some(0),
                                ..Default::default()
                            }),
                            cx,
                        )
                    }))
            };
            card = card.child(
                widgets::card_row(&theme, false)
                    .child(text_row(
                        &theme,
                        "Slots",
                        "Compile jobs all agents share. Auto is this machine's core count, what one build uses on its own.".to_owned(),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(auto_chip)
                            .child(step(false))
                            .child(
                                div()
                                    .min_w(px(22.0))
                                    .flex()
                                    .justify_center()
                                    .text_color(theme.text)
                                    .child(slots.to_string()),
                            )
                            .child(step(true)),
                    ),
            );
        }
        card = card.child(
            widgets::card_row(&theme, false)
                .child(text_row(&theme, "Use sccache when available", sccache_text))
                .child(sccache_toggle),
        );
        widgets::section(&theme, "Builds", card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> BuildSlotsSettings {
        BuildSlotsSettings {
            share_slots: true,
            slots: 0,
            use_sccache: true,
            default_slots: 8,
            supported: true,
            active: true,
            paused_by: None,
            sccache_path: None,
            sccache_paused_by: None,
        }
    }

    #[test]
    fn steps_stay_in_range() {
        assert_eq!(stepped(8, true), 9);
        assert_eq!(stepped(8, false), 7);
        assert_eq!(stepped(1, false), 1);
        assert_eq!(stepped(MAX_BUILD_SLOTS, true), MAX_BUILD_SLOTS);
    }

    #[test]
    fn descriptions_explain_the_state() {
        let mut s = settings();
        assert_eq!(share_description(&s), SHARE_HELP);
        assert!(sccache_description(&s).contains("brew install sccache"));
        s.sccache_path = Some("/opt/homebrew/bin/sccache".into());
        assert_eq!(sccache_description(&s), SCCACHE_HELP);
        s.paused_by = Some("CARGO_BUILD_JOBS".into());
        assert!(share_description(&s).starts_with("CARGO_BUILD_JOBS is set"));
        s.sccache_paused_by = Some("RUSTC_WRAPPER".into());
        assert!(sccache_description(&s).starts_with("RUSTC_WRAPPER is set"));
        s.supported = false;
        assert!(share_description(&s).contains("Not available"));
    }

    #[test]
    fn defaults_round_trip_from_the_engine() {
        let json = serde_json::json!({"shareSlots": true, "useSccache": true});
        let parsed: BuildSlotsSettings = serde_json::from_value(json).unwrap();
        assert!(parsed.share_slots && parsed.use_sccache);
        assert_eq!(parsed.slots, 0);
        assert_eq!(
            parsed.effective_slots(),
            1,
            "no default reported yet: at least one slot"
        );
    }
}
