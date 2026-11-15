//! Settings → General → Share anonymous usage stats: the switch for the
//! once-a-day anonymous ping (docs/PRIVACY.md). On by default. The setting
//! lives on the engine (`GetTelemetrySettings`), which sends the ping.
//! `GODSPEED_TELEMETRY=0` or `DO_NOT_TRACK=1` turn it off regardless; the
//! switch then shows off and says which variable did it.
//!
//! Also the one-time first-launch notice ([`show_notice_once`]).

use std::time::Duration;

use gpui::{App, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_proto::{PRIVACY_URL, TelemetrySettings, TelemetrySettingsUpdate};
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// The first-launch notice's copy.
pub const NOTICE: &str = "Godspeed sends an anonymous daily usage ping (version, how much you used it, token totals) so we know it's working. No prompts or code. Turn off in Settings.";

/// How long the notice stays up (paused while hovered or the window is in
/// the background).
const NOTICE_DURATION: Duration = Duration::from_secs(20);

pub struct TelemetryCard {
    state: Entity<AppState>,
    settings: Loadable<TelemetrySettings>,
    error: Option<String>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl TelemetryCard {
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

    /// `GetTelemetrySettings`, or `SetTelemetrySettings` when saving;
    /// either reply is the engine's authoritative setting.
    fn call(&mut self, save: Option<TelemetrySettingsUpdate>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(update) => (
                methods::SET_TELEMETRY_SETTINGS,
                serde_json::to_value(update).unwrap_or_default(),
            ),
            None => (methods::GET_TELEMETRY_SETTINGS, serde_json::json!({})),
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
                    serde_json::from_value::<TelemetrySettings>(v).map_err(|e| e.to_string())
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

impl Render for TelemetryCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let ready = self.settings.ready().cloned();
        let env_off = ready
            .as_ref()
            .and_then(|settings| settings.disabled_by_env.clone());
        let on = ready.as_ref().is_some_and(TelemetrySettings::sending);
        let switchable = ready.is_some() && env_off.is_none();
        let toggle = widgets::toggle_switch(&theme, on, "share-usage-stats")
            .id("share-usage-stats-toggle")
            .flex_none()
            .role(gpui::Role::Switch)
            .aria_label("Share anonymous usage stats")
            .aria_toggled(if on {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .when(switchable, |el| {
                el.cursor_pointer()
                    .tab_index(0)
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |card, _, _, cx| {
                        card.call(
                            Some(TelemetrySettingsUpdate {
                                enabled: Some(!on),
                                notice_shown: Some(true),
                            }),
                            cx,
                        )
                    }))
            })
            .when(!switchable, |el| el.opacity(0.5));
        let description = match &env_off {
            Some(var) => format!("Turned off by {var} in the environment. "),
            None => "A small daily ping: version, platform, how much you used Godspeed and token totals. No prompts, code, paths or account details. ".to_owned(),
        };
        let details = div()
            .id("telemetry-whats-sent")
            .cursor_pointer()
            .text_color(theme.text)
            .hover(|s| s.underline())
            .on_click(|_, _, cx| cx.open_url(PRIVACY_URL))
            .child("What's sent?");
        widgets::section(
            &theme,
            "Privacy",
            widgets::section_card(&theme).mt_0().child(
                widgets::card_row(&theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(&theme, "Share anonymous usage stats"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div().child(description).into_any_element(),
                                    details.into_any_element(),
                                ],
                            )),
                    )
                    .child(toggle),
            ),
        )
    }
}

/// Show the one-time first-launch notice if this engine hasn't shown it
/// yet, then record that it was shown. Non-blocking: a toast with "Turn
/// off" and "Details".
pub fn show_notice_once(engine: EngineHandle, cx: &mut Context<AppState>) {
    cx.spawn(async move |_, cx| {
        let Ok(value) = engine
            .client()
            .call(methods::GET_TELEMETRY_SETTINGS, serde_json::json!({}))
            .await
        else {
            return;
        };
        let Ok(settings) = serde_json::from_value::<TelemetrySettings>(value) else {
            return;
        };
        if !settings.notice_pending {
            return;
        }
        let mark_shown = TelemetrySettingsUpdate {
            enabled: None,
            notice_shown: Some(true),
        };
        let _ = engine
            .client()
            .call(
                methods::SET_TELEMETRY_SETTINGS,
                serde_json::to_value(mark_shown).unwrap_or_default(),
            )
            .await;
        // Already off (the environment, or a switch flipped elsewhere):
        // nothing is sent, so there is nothing to announce.
        if !settings.sending() {
            return;
        }
        let turn_off_engine = engine.clone();
        cx.update(|cx: &mut App| {
            crate::toast::notice(
                cx,
                NOTICE,
                vec![
                    crate::toast::ToastAction::new("Turn off", move |cx| {
                        turn_off(turn_off_engine.clone(), cx)
                    }),
                    crate::toast::ToastAction::new("Details", |cx| cx.open_url(PRIVACY_URL)),
                ],
                NOTICE_DURATION,
            );
        });
    })
    .detach();
}

fn turn_off(engine: EngineHandle, cx: &mut App) {
    cx.spawn(async move |cx| {
        let update = TelemetrySettingsUpdate {
            enabled: Some(false),
            notice_shown: Some(true),
        };
        let result = engine
            .client()
            .call(
                methods::SET_TELEMETRY_SETTINGS,
                serde_json::to_value(update).unwrap_or_default(),
            )
            .await;
        cx.update(|cx| match result {
            Ok(_) => crate::toast::success(cx, "Anonymous usage stats turned off"),
            Err(err) => crate::toast::error(cx, format!("Couldn't turn off usage stats: {err}")),
        });
    })
    .detach();
}
