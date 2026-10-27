//! Settings → Zoomies → Telegram: the bot token (typed or pasted into a
//! field that never shows it), the connection, a one-time link code, the
//! linked chats, and the defaults newly linked chats start with. Everything
//! runs on the engine (`zeron_proto::telegram`), so a headless engine serves
//! the bot too.

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use gpui_tokio::Tokio;
use serde_json::json;

use zeron_proto::telegram::{TelegramConnection, TelegramDefaults, TelegramStatus};
use zeron_proto::{AccessMode, HarnessId};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const TOKEN_PLACEHOLDER: &str = "Paste the bot token from @BotFather";

pub(crate) fn connection_label(connection: &TelegramConnection, token_set: bool) -> String {
    match connection {
        TelegramConnection::Off if token_set => "Starting…".into(),
        TelegramConnection::Off => "No bot token yet".into(),
        TelegramConnection::Connecting => "Connecting…".into(),
        TelegramConnection::Connected { bot_username } => format!("Connected as @{bot_username}"),
        TelegramConnection::Error { message } => message.clone(),
    }
}

/// What the token field shows instead of the characters typed into it.
pub(crate) fn masked_placeholder(pending_chars: usize) -> String {
    if pending_chars == 0 {
        TOKEN_PLACEHOLDER.into()
    } else {
        format!(
            "{} ({pending_chars} characters). Save to connect.",
            "•".repeat(8)
        )
    }
}

pub struct TelegramSection {
    state: Entity<AppState>,
    status: Option<TelegramStatus>,
    harnesses: Vec<(HarnessId, String)>,
    /// `(id, label)` for the defaults' provider.
    models: Vec<(String, String)>,
    models_for: Option<HarnessId>,
    token_input: Entity<ComposerInput>,
    /// What was typed into the token field, kept out of the field itself.
    pending_token: String,
    error: Option<SharedString>,
    provider_select: widgets::SelectState,
    model_select: widgets::SelectState,
    project_select: widgets::SelectState,
    access_select: widgets::SelectState,
    watch_task: Option<Task<()>>,
    harness_task: Option<Task<()>>,
    models_task: Option<Task<()>>,
    task: Option<Task<()>>,
    _token_events: Subscription,
}

impl TelegramSection {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let token_input = cx.new(|cx| ComposerInput::new(TOKEN_PLACEHOLDER, cx).with_single_line());
        let token_events =
            cx.subscribe(
                &token_input,
                |this: &mut Self, input, event, cx| match event {
                    ComposerInputEvent::Edited => {
                        let typed = input.read(cx).text().trim().to_string();
                        if typed.is_empty() {
                            return;
                        }
                        this.pending_token.push_str(&typed);
                        let placeholder = masked_placeholder(this.pending_token.chars().count());
                        input.update(cx, |input, cx| {
                            input.set_text("", cx);
                            input.set_placeholder(placeholder, cx);
                        });
                        cx.notify();
                    }
                    ComposerInputEvent::Submitted => this.save_token(cx),
                    _ => {}
                },
            );
        let mut section = Self {
            state,
            status: None,
            harnesses: Vec::new(),
            models: Vec::new(),
            models_for: None,
            token_input,
            pending_token: String::new(),
            error: None,
            provider_select: widgets::SelectState::default(),
            model_select: widgets::SelectState::default(),
            project_select: widgets::SelectState::default(),
            access_select: widgets::SelectState::default(),
            watch_task: None,
            harness_task: None,
            models_task: None,
            task: None,
            _token_events: token_events,
        };
        section.start_watch(cx);
        section
    }

    fn start_watch(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TelegramStatus>();
        let feed = Tokio::spawn(cx, async move {
            let mut items = engine
                .client()
                .subscribe(methods::WATCH_TELEGRAM, json!({}))
                .await
                .map_err(|e| e.to_string())?;
            while let Some(item) = items.recv().await {
                let Ok(status) = serde_json::from_value::<TelegramStatus>(item) else {
                    continue;
                };
                if tx.send(status).is_err() {
                    break;
                }
            }
            Ok::<(), String>(())
        });
        self.load_harnesses(cx);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            while let Some(status) = rx.recv().await {
                if this
                    .update(cx, |section, cx| {
                        section.status = Some(status);
                        section.load_models(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            if let Ok(Err(err)) = feed.await {
                this.update(cx, |section, cx| {
                    section.error = Some(format!("Telegram unavailable: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn load_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.harness_task = Some(cx.spawn(async move |this, cx| {
            let Ok(value) = engine
                .client()
                .call(methods::LIST_HARNESSES, json!({}))
                .await
            else {
                return;
            };
            let harnesses: Vec<(HarnessId, String)> = value
                .as_array()
                .into_iter()
                .flatten()
                .filter(|h| h["installed"].as_bool().unwrap_or(true))
                .filter(|h| h["enabled"].as_bool() != Some(false))
                .filter_map(|h| {
                    let id: HarnessId = serde_json::from_value(h["id"].clone()).ok()?;
                    (id != HarnessId::Mock).then(|| (id, h["name"].as_str().unwrap_or("").into()))
                })
                .collect();
            this.update(cx, |section, cx| {
                section.harnesses = harnesses;
                section.load_models(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn defaults(&self) -> TelegramDefaults {
        self.status
            .as_ref()
            .map(|s| s.defaults.clone())
            .unwrap_or_default()
    }

    fn effective_harness(&self) -> Option<HarnessId> {
        self.defaults()
            .harness
            .or_else(|| self.harnesses.first().map(|(id, _)| *id))
    }

    /// Fetch the model list when the defaults' provider changed.
    fn load_models(&mut self, cx: &mut Context<Self>) {
        let Some(harness) = self.effective_harness() else {
            return;
        };
        if self.models_for == Some(harness) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.models_for = Some(harness);
        self.models.clear();
        self.models_task = Some(cx.spawn(async move |this, cx| {
            let models: Vec<(String, String)> = engine
                .client()
                .call(methods::LIST_MODELS, json!({ "harness": harness }))
                .await
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_string();
                    let label = m["label"].as_str().unwrap_or(&id).to_string();
                    Some((id, label))
                })
                .collect();
            this.update(cx, |section, cx| {
                if section.models_for == Some(harness) {
                    section.models = models;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |section, cx| {
                if let Err(err) = result {
                    section.error = Some(err.to_string().into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn reset_token_field(&mut self, cx: &mut Context<Self>) {
        self.pending_token.clear();
        self.token_input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder(TOKEN_PLACEHOLDER, cx);
        });
    }

    fn save_token(&mut self, cx: &mut Context<Self>) {
        if self.pending_token.is_empty() {
            return;
        }
        let token = std::mem::take(&mut self.pending_token);
        self.reset_token_field(cx);
        self.call(methods::SET_TELEGRAM_TOKEN, json!({ "token": token }), cx);
    }

    fn set_defaults(&mut self, defaults: TelegramDefaults, cx: &mut Context<Self>) {
        self.call(
            methods::SET_TELEGRAM_DEFAULTS,
            json!({ "defaults": defaults }),
            cx,
        );
    }

    fn render_connection(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let token_set = self.status.as_ref().is_some_and(|s| s.token_set);
        let connection = self
            .status
            .as_ref()
            .map(|s| s.connection.clone())
            .unwrap_or_default();
        let connected = matches!(connection, TelegramConnection::Connected { .. });
        let label = connection_label(&connection, token_set);
        let badge = if connected {
            widgets::badge_active(theme, "Connected")
        } else if matches!(connection, TelegramConnection::Error { .. }) {
            widgets::badge(theme, "Error")
        } else {
            widgets::badge(theme, if token_set { "Connecting" } else { "Off" })
        };
        let pending = !self.pending_token.is_empty();
        let token_field = div().flex_1().min_w_0().child(popover::dialog_field(
            self.token_input.clone().into_any_element(),
        ));
        widgets::section_card(theme)
            .mt(px(0.0))
            .child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, crate::icons::CHAT_ROUND_LINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Telegram bot"))
                            .child(widgets::meta_line(
                                theme,
                                vec![SharedString::from(label).into_any_element()],
                            )),
                    )
                    .child(badge),
            )
            .child(
                widgets::card_row(theme, false)
                    .child(token_field)
                    .when(pending, |row| {
                        row.child(
                            widgets::text_action(theme, widgets::ActionTone::Quiet, "Discard")
                                .id("telegram-token-discard")
                                .on_click(cx.listener(|section, _, _, cx| {
                                    section.reset_token_field(cx);
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Solid, "Save")
                            .id("telegram-token-save")
                            .on_click(cx.listener(|section, _, _, cx| section.save_token(cx))),
                    )
                    .when(token_set, |row| {
                        row.child(
                            widgets::text_action(theme, widgets::ActionTone::Quiet, "Clear")
                                .id("telegram-token-clear")
                                .on_click(cx.listener(|section, _, _, cx| {
                                    section.call(methods::CLEAR_TELEGRAM_TOKEN, json!({}), cx);
                                })),
                        )
                    }),
            )
            .child(
                widgets::card_row(theme, false).child(widgets::page_subtitle(
                    theme,
                    "In Telegram, message @BotFather, send /newbot and paste the token it gives you. The token stays in this device's engine.",
                )),
            )
            .into_any_element()
    }

    fn render_linking(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let status = self.status.clone().unwrap_or_default();
        let bot = match &status.connection {
            TelegramConnection::Connected { bot_username } => format!("@{bot_username}"),
            _ => "your bot".into(),
        };
        let mut card = widgets::section_card(theme).mt(px(0.0));
        let live_code = status
            .link_code
            .as_ref()
            .filter(|code| code.expires_at > chrono::Utc::now());
        let code_row = match live_code {
            Some(code) => {
                let copy = code.code.clone();
                let expires = code
                    .expires_at
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string();
                widgets::card_row(theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                widgets::row_title(theme, code.code.clone())
                                    .font_family(theme.font_mono.clone()),
                            )
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    SharedString::from(format!(
                                        "Send /link {} to {bot} before {expires}",
                                        code.code
                                    ))
                                    .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Quiet, "Copy")
                            .id("telegram-code-copy")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(format!(
                                    "/link {copy}"
                                )));
                            })),
                    )
            }
            None => widgets::card_row(theme, true)
                .child(div().flex_1().min_w_0().child(widgets::page_subtitle(
                    theme,
                    "Link a Telegram chat with a one-time code. It works for 10 minutes.",
                )))
                .child(
                    widgets::text_action(theme, widgets::ActionTone::Filled, "Get link code")
                        .id("telegram-code-new")
                        .on_click(cx.listener(|section, _, _, cx| {
                            section.call(methods::CREATE_TELEGRAM_LINK_CODE, json!({}), cx);
                        })),
                ),
        };
        card = card.child(code_row);
        let now = chrono::Utc::now();
        for (ix, chat) in status.linked_chats.iter().enumerate() {
            let chat_id = chat.chat_id;
            card = card.child(
                widgets::card_row(theme, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, chat.label.clone()))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    SharedString::from(format!(
                                        "Linked {}",
                                        crate::state::format_time_ago(chat.linked_at, now)
                                    ))
                                    .into_any_element(),
                                    SharedString::from(chat.timezone.clone()).into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Quiet, "Unlink")
                            .id(("telegram-unlink", ix))
                            .on_click(cx.listener(move |section, _, _, cx| {
                                section.call(
                                    methods::UNLINK_TELEGRAM_CHAT,
                                    json!({ "chatId": chat_id }),
                                    cx,
                                );
                            })),
                    ),
            );
        }
        card.into_any_element()
    }

    fn render_defaults(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let defaults = self.defaults();
        let harness = self.effective_harness();
        let field = |label: &str, control: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(widgets::field_label(theme, label.to_string()))
                .child(control)
        };

        let harness_ids: Vec<HarnessId> = self.harnesses.iter().map(|(id, _)| *id).collect();
        let provider_ix = self
            .harnesses
            .iter()
            .position(|(id, _)| Some(*id) == harness)
            .unwrap_or(0);
        let base = defaults.clone();
        let provider = widgets::select("telegram-provider", "Provider", theme, |s: &mut Self| {
            &mut s.provider_select
        })
        .options(
            self.harnesses
                .iter()
                .map(|(_, name)| widgets::SelectOption::new(name.clone())),
            provider_ix,
        )
        .width(200.0)
        .open_rightward()
        .on_select(move |section, ix, _, cx| {
            let mut defaults = base.clone();
            let picked = harness_ids.get(ix).copied();
            if picked != defaults.harness {
                defaults.model = None;
            }
            defaults.harness = picked;
            defaults.access_mode = defaults.access_mode.filter(|mode| {
                picked.is_some_and(|h| zeron_proto::access_mode_supported(h, *mode))
            });
            section.set_defaults(defaults, cx);
        })
        .render(&self.provider_select, cx);

        let model_ids: Vec<Option<String>> = std::iter::once(None)
            .chain(self.models.iter().map(|(id, _)| Some(id.clone())))
            .collect();
        let model_ix = model_ids
            .iter()
            .position(|id| *id == defaults.model)
            .unwrap_or(0);
        let base = defaults.clone();
        let model = widgets::select("telegram-model", "Model", theme, |s: &mut Self| {
            &mut s.model_select
        })
        .options(
            std::iter::once(widgets::SelectOption::new("Default")).chain(
                self.models
                    .iter()
                    .map(|(_, label)| widgets::SelectOption::new(label.clone())),
            ),
            model_ix,
        )
        .width(200.0)
        .open_rightward()
        .on_select(move |section, ix, _, cx| {
            let mut defaults = base.clone();
            defaults.harness = defaults.harness.or(harness);
            defaults.model = model_ids.get(ix).cloned().flatten();
            section.set_defaults(defaults, cx);
        })
        .render(&self.model_select, cx);

        let local = self.state.read(cx).local_device_id.clone();
        let spaces: Vec<(Option<String>, String)> =
            std::iter::once((None, "No project".to_string()))
                .chain(
                    self.state
                        .read(cx)
                        .spaces
                        .iter()
                        .filter(|s| Some(&s.device_id) == local.as_ref())
                        .map(|s| (Some(s.id.clone()), s.display_name().to_string())),
                )
                .collect();
        let project_ix = spaces
            .iter()
            .position(|(id, _)| *id == defaults.space_id)
            .unwrap_or(0);
        let space_ids: Vec<Option<String>> = spaces.iter().map(|(id, _)| id.clone()).collect();
        let base = defaults.clone();
        let project = widgets::select("telegram-project", "Project", theme, |s: &mut Self| {
            &mut s.project_select
        })
        .options(
            spaces
                .iter()
                .map(|(_, name)| widgets::SelectOption::new(name.clone())),
            project_ix,
        )
        .width(200.0)
        .open_rightward()
        .on_select(move |section, ix, _, cx| {
            let mut defaults = base.clone();
            defaults.space_id = space_ids.get(ix).cloned().flatten();
            section.set_defaults(defaults, cx);
        })
        .render(&self.project_select, cx);

        let modes: Vec<AccessMode> = harness
            .map(|h| zeron_proto::supported_access_modes(h).to_vec())
            .unwrap_or_else(|| AccessMode::ALL.to_vec());
        let current_mode = defaults.access_mode.unwrap_or_default();
        let access_ix = modes.iter().position(|m| *m == current_mode).unwrap_or(0);
        let base = defaults.clone();
        let picked_modes = modes.clone();
        let access = widgets::select("telegram-access", "Access", theme, |s: &mut Self| {
            &mut s.access_select
        })
        .options(
            modes
                .iter()
                .map(|m| widgets::SelectOption::new(m.label()).detail(m.description())),
            access_ix,
        )
        .width(200.0)
        .open_rightward()
        .on_select(move |section, ix, _, cx| {
            let mut defaults = base.clone();
            defaults.harness = defaults.harness.or(harness);
            defaults.access_mode = picked_modes.get(ix).copied();
            section.set_defaults(defaults, cx);
        })
        .render(&self.access_select, cx);

        widgets::section_card(theme)
            .mt(px(0.0))
            .p(px(16.0))
            .gap(px(14.0))
            .child(widgets::page_subtitle(
                theme,
                "Newly linked chats start with these. Each chat can change its own with /provider, /model, /project and /access.",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(12.0))
                    .child(field("Provider", provider.into_any_element()))
                    .child(field("Model", model.into_any_element()))
                    .child(field("Project", project.into_any_element()))
                    .child(field("Access", access.into_any_element())),
            )
            .into_any_element()
    }
}

impl Render for TelegramSection {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.watch_task.is_none() {
            self.start_watch(cx);
        }
        let theme = Theme::of(cx).for_settings_surface();
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message));
        let connection = self.render_connection(&theme, cx);
        let linking = self.render_linking(&theme, cx);
        let defaults = self.render_defaults(&theme, cx);
        div()
            .flex()
            .flex_col()
            .child(widgets::section(&theme, "Telegram", connection).children(error))
            .child(widgets::section(&theme, "Linked chats", linking))
            .child(widgets::section(
                &theme,
                "Defaults for new Telegram chats",
                defaults,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_reads_as_a_sentence() {
        assert_eq!(
            connection_label(
                &TelegramConnection::Connected {
                    bot_username: "gs_bot".into()
                },
                true
            ),
            "Connected as @gs_bot"
        );
        assert_eq!(
            connection_label(&TelegramConnection::Off, false),
            "No bot token yet"
        );
        assert_eq!(
            connection_label(
                &TelegramConnection::Error {
                    message: "Telegram rejected the bot token".into()
                },
                true
            ),
            "Telegram rejected the bot token"
        );
    }

    #[test]
    fn the_token_field_never_echoes_the_token() {
        assert_eq!(masked_placeholder(0), TOKEN_PLACEHOLDER);
        let masked = masked_placeholder(46);
        assert!(masked.starts_with("••••••••"), "{masked}");
        assert!(masked.contains("46 characters"));
    }
}
