//! `/provider`, `/model`, `/project` and `/access`: inline-button pickers
//! for a chat's defaults. Provider, model and project apply from the next
//! thread (switching mid-thread would break the provider's session); the
//! access mode also applies to the current thread, as in the app.

use std::sync::Arc;

use zeron_proto::telegram::TelegramDefaults;
use zeron_proto::{AccessMode, HarnessId, Model};

use super::Inner;
use super::api::{BotApi, Button, Format, Keyboard, Message, SendOptions};
use super::callback::Callback;
use super::store::LinkedChat;

const MODELS_PER_PAGE: usize = 8;

fn marked(label: &str, current: bool) -> String {
    if current {
        format!("• {label}")
    } else {
        label.to_string()
    }
}

fn in_rows(buttons: Vec<Button>, per_row: usize) -> Keyboard {
    buttons.chunks(per_row).map(<[Button]>::to_vec).collect()
}

impl Inner {
    fn save_defaults(
        &self,
        chat: i64,
        defaults: &TelegramDefaults,
        new_thread: bool,
    ) -> Result<(), String> {
        self.store
            .set_defaults(chat, defaults)
            .map_err(|err| err.to_string())?;
        if new_thread {
            self.store
                .set_thread(chat, None)
                .map_err(|err| err.to_string())?;
        }
        self.changed();
        Ok(())
    }

    async fn show_menu(
        &self,
        api: &BotApi,
        chat: i64,
        edit: Option<i64>,
        text: &str,
        keyboard: Keyboard,
    ) {
        let result = match edit {
            Some(message_id) => {
                api.edit_message_text(chat, message_id, text, Some(&keyboard), Format::Plain)
                    .await
            }
            None => api
                .send_message(
                    chat,
                    text,
                    &SendOptions {
                        keyboard: Some(keyboard),
                        ..Default::default()
                    },
                )
                .await
                .map(|_| ()),
        };
        if let Err(err) = result {
            tracing::warn!(%err, "telegram: menu not shown");
        }
    }

    /// Close a menu: its text becomes the outcome and the buttons go.
    async fn settle_menu(&self, api: &BotApi, message: &Message, text: &str) {
        if let Err(err) = api
            .edit_message_text(
                message.chat.id,
                message.message_id,
                text,
                None,
                Format::Plain,
            )
            .await
        {
            tracing::warn!(%err, "telegram: menu not settled");
        }
    }

    // ── provider ──────────────────────────────────────────────────────────

    pub(super) async fn show_providers(&self, api: &BotApi, link: &LinkedChat) {
        let harnesses = self.available_harnesses();
        if harnesses.is_empty() {
            self.say(
                api,
                link.chat_id,
                "No agent is installed and switched on in Godspeed on this device.",
                None,
            )
            .await;
            return;
        }
        let current = self.resolve(&link.defaults).ok().map(|r| r.harness);
        let buttons = harnesses
            .iter()
            .map(|h| {
                Button::new(
                    marked(&h.name, Some(h.id) == current),
                    Callback::Provider(h.id).encode(),
                )
            })
            .collect();
        self.show_menu(
            api,
            link.chat_id,
            None,
            "Which agent should new threads use?",
            in_rows(buttons, 2),
        )
        .await;
    }

    pub(super) async fn pick_provider(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        message: &Message,
        harness: HarnessId,
    ) -> Option<String> {
        let Some(descriptor) = self
            .available_harnesses()
            .into_iter()
            .find(|h| h.id == harness)
        else {
            return Some("That agent isn't available any more.".into());
        };
        let mut defaults = link.defaults.clone();
        if defaults.harness != Some(harness) {
            defaults.model = None;
        }
        defaults.harness = Some(harness);
        defaults.access_mode = defaults
            .access_mode
            .filter(|mode| zeron_proto::access_mode_supported(harness, *mode));
        if let Err(err) = self.save_defaults(link.chat_id, &defaults, true) {
            return Some(err);
        }
        self.settle_menu(
            api,
            message,
            &format!(
                "Agent: {}. Your next message starts a new thread.",
                descriptor.name
            ),
        )
        .await;
        None
    }

    // ── model ─────────────────────────────────────────────────────────────

    async fn models_for(&self, harness: HarnessId) -> Result<Vec<Model>, String> {
        let lease = Arc::new(self.deps.registry.execution_lease(harness).await);
        let resolved = self
            .deps
            .registry
            .resolve(harness)
            .map_err(|err| err.to_string())?;
        crate::model_catalogs::list_with_lease(&self.data_dir, resolved, false, Some(lease))
            .await
            .map_err(|err| err.to_string())
    }

    pub(super) async fn show_models(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        page: usize,
        edit: Option<i64>,
    ) {
        let resolved = match self.resolve(&link.defaults) {
            Ok(resolved) => resolved,
            Err(err) => {
                self.say(api, link.chat_id, &err, None).await;
                return;
            }
        };
        let models = match self.models_for(resolved.harness).await {
            Ok(models) => models,
            Err(err) => {
                self.say(
                    api,
                    link.chat_id,
                    &format!("Couldn't list {} models: {err}", resolved.harness_name),
                    None,
                )
                .await;
                return;
            }
        };
        let pages = models.len().div_ceil(MODELS_PER_PAGE).max(1);
        let page = page.min(pages - 1);
        let current = resolved.model.as_deref();
        let mut keyboard: Keyboard = vec![vec![Button::new(
            marked("Default", current.is_none()),
            Callback::Model(None).encode(),
        )]];
        let start = page * MODELS_PER_PAGE;
        keyboard.extend(in_rows(
            models
                .iter()
                .enumerate()
                .skip(start)
                .take(MODELS_PER_PAGE)
                .map(|(index, model)| {
                    Button::new(
                        marked(&model.label, current == Some(model.id.as_str())),
                        Callback::Model(Some(index)).encode(),
                    )
                })
                .collect(),
            2,
        ));
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(Button::new(
                "‹ Prev",
                Callback::ModelPage(page - 1).encode(),
            ));
        }
        if page + 1 < pages {
            nav.push(Button::new(
                "Next ›",
                Callback::ModelPage(page + 1).encode(),
            ));
        }
        if !nav.is_empty() {
            keyboard.push(nav);
        }
        let text = format!(
            "{} model for new threads (page {} of {pages}):",
            resolved.harness_name,
            page + 1
        );
        self.show_menu(api, link.chat_id, edit, &text, keyboard)
            .await;
    }

    pub(super) async fn pick_model(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        message: &Message,
        index: Option<usize>,
    ) -> Option<String> {
        let resolved = match self.resolve(&link.defaults) {
            Ok(resolved) => resolved,
            Err(err) => return Some(err),
        };
        let model = match index {
            None => None,
            Some(index) => match self.models_for(resolved.harness).await {
                Ok(models) => match models.into_iter().nth(index) {
                    Some(model) => Some(model),
                    None => return Some("That model list changed; send /model again.".into()),
                },
                Err(err) => return Some(err),
            },
        };
        let mut defaults = link.defaults.clone();
        defaults.harness = Some(resolved.harness);
        defaults.model = model.as_ref().map(|m| m.id.clone());
        if let Err(err) = self.save_defaults(link.chat_id, &defaults, true) {
            return Some(err);
        }
        let label = model.map_or_else(|| "default".to_string(), |m| m.label);
        self.settle_menu(
            api,
            message,
            &format!(
                "{} model: {label}. Your next message starts a new thread.",
                resolved.harness_name
            ),
        )
        .await;
        None
    }

    // ── project ───────────────────────────────────────────────────────────

    pub(super) async fn show_projects(&self, api: &BotApi, link: &LinkedChat) {
        let current = link.defaults.space_id.as_deref();
        let mut buttons: Vec<Button> = self
            .recent_projects()
            .iter()
            .map(|space| {
                Button::new(
                    marked(space.display_name(), current == Some(space.id.as_str())),
                    Callback::Project(Some(space.id.clone())).encode(),
                )
            })
            .collect();
        buttons.push(Button::new(
            marked("No project", current.is_none()),
            Callback::Project(None).encode(),
        ));
        self.show_menu(
            api,
            link.chat_id,
            None,
            "Which project should new threads work in?",
            in_rows(buttons, 2),
        )
        .await;
    }

    pub(super) async fn pick_project(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        message: &Message,
        space_id: Option<String>,
    ) -> Option<String> {
        let name = match &space_id {
            Some(id) => match self.local_space(id) {
                Some(space) => space.display_name().to_string(),
                None => return Some("That project is no longer on this device.".into()),
            },
            None => "none (a scratch folder per thread)".to_string(),
        };
        let mut defaults = link.defaults.clone();
        defaults.space_id = space_id;
        if let Err(err) = self.save_defaults(link.chat_id, &defaults, true) {
            return Some(err);
        }
        self.settle_menu(
            api,
            message,
            &format!("Project: {name}. Your next message starts a new thread."),
        )
        .await;
        None
    }

    // ── access ────────────────────────────────────────────────────────────

    pub(super) async fn show_access(&self, api: &BotApi, link: &LinkedChat) {
        let resolved = match self.resolve(&link.defaults) {
            Ok(resolved) => resolved,
            Err(err) => {
                self.say(api, link.chat_id, &err, None).await;
                return;
            }
        };
        let keyboard = zeron_proto::supported_access_modes(resolved.harness)
            .iter()
            .map(|mode| {
                vec![Button::new(
                    marked(mode.label(), *mode == resolved.access),
                    Callback::Access(*mode).encode(),
                )]
            })
            .collect();
        let text = format!(
            "Access mode for {}:\n{}",
            resolved.harness_name,
            AccessMode::ALL
                .iter()
                .filter(|mode| zeron_proto::access_mode_supported(resolved.harness, **mode))
                .map(|mode| format!("• {}: {}", mode.label(), mode.description()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        self.show_menu(api, link.chat_id, None, &text, keyboard)
            .await;
    }

    pub(super) async fn pick_access(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        message: &Message,
        mode: AccessMode,
    ) -> Option<String> {
        let resolved = match self.resolve(&link.defaults) {
            Ok(resolved) => resolved,
            Err(err) => return Some(err),
        };
        if !zeron_proto::access_mode_supported(resolved.harness, mode) {
            return Some(format!(
                "{} can't run in {}.",
                resolved.harness_name,
                mode.label()
            ));
        }
        let mut defaults = link.defaults.clone();
        defaults.access_mode = Some(mode);
        if let Err(err) = self.save_defaults(link.chat_id, &defaults, false) {
            return Some(err);
        }
        let current = link.thread_id.as_deref().and_then(|id| {
            let chat = self.deps.workspace.chat(id).ok().flatten()?;
            let mut config = chat.config?;
            if !zeron_proto::access_mode_supported(config.harness, mode) {
                return None;
            }
            config.access_mode = Some(mode);
            config.sandbox = mode.legacy().0;
            self.deps.workspace.set_chat_config(id, &config).ok()
        });
        let scope = if current.is_some() {
            "this thread and new ones"
        } else {
            "new threads"
        };
        self.settle_menu(
            api,
            message,
            &format!("Access: {} for {scope}.", mode.label()),
        )
        .await;
        None
    }
}
