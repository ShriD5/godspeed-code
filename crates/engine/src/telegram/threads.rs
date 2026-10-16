//! Godspeed chats driven from Telegram: resolving a chat's defaults,
//! starting a thread, sending into it (Run when idle, the queue while a
//! turn runs, exactly as the composer does), stopping it, and `/status`.

use std::sync::Arc;

use zeron_doc::SessionCommandPayload;
use zeron_proto::telegram::TelegramDefaults;
use zeron_proto::{AccessMode, ChatConfig, HarnessId, SessionStatus, Space};

use super::Inner;
use super::api::{BotApi, Message};
use super::store::{LinkedChat, PendingTurn};
use crate::new_id;
use crate::registry::{HarnessDescriptor, descriptor_enabled};

const RECENT_PROJECTS: usize = 10;

/// A chat's defaults with every gap filled in.
pub(super) struct Resolved {
    pub harness: HarnessId,
    pub harness_name: String,
    pub model: Option<String>,
    pub space: Option<Space>,
    pub access: AccessMode,
}

pub(super) fn default_access(harness: HarnessId) -> AccessMode {
    let supported = zeron_proto::supported_access_modes(harness);
    if supported.contains(&AccessMode::default()) {
        AccessMode::default()
    } else {
        supported.first().copied().unwrap_or_default()
    }
}

impl Inner {
    /// Providers this device can run now (installed and switched on).
    pub(super) fn available_harnesses(&self) -> Vec<HarnessDescriptor> {
        self.deps
            .registry
            .descriptors()
            .into_iter()
            .filter(|d| d.installed && descriptor_enabled(d))
            .collect()
    }

    pub(super) fn local_space(&self, space_id: &str) -> Option<Space> {
        self.deps
            .workspace
            .space(space_id)
            .ok()
            .flatten()
            .filter(|space| space.device_id == self.device_id)
    }

    /// This device's projects, most recently active first.
    pub(super) fn recent_projects(&self) -> Vec<Space> {
        let spaces = self.deps.workspace.read_spaces().unwrap_or_default();
        let chats = self.deps.workspace.read_chats().unwrap_or_default();
        let mut projects: Vec<(Option<chrono::DateTime<chrono::Utc>>, Space)> = spaces
            .into_iter()
            .filter(|space| space.device_id == self.device_id)
            .map(|space| {
                let latest = chats
                    .iter()
                    .filter(|chat| chat.space_id.as_deref() == Some(space.id.as_str()))
                    .filter_map(|chat| chat.last_message_at.or(Some(chat.created_at)))
                    .max();
                (latest, space)
            })
            .collect();
        projects.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.display_name().cmp(b.1.display_name()))
        });
        projects
            .into_iter()
            .take(RECENT_PROJECTS)
            .map(|(_, space)| space)
            .collect()
    }

    pub(super) fn resolve(&self, defaults: &TelegramDefaults) -> Result<Resolved, String> {
        let harnesses = self.available_harnesses();
        let descriptor = defaults
            .harness
            .and_then(|id| harnesses.iter().find(|d| d.id == id))
            .or_else(|| harnesses.first())
            .ok_or("No agent is installed and switched on in Godspeed on this device.")?;
        let harness = descriptor.id;
        let same_harness = defaults.harness == Some(harness);
        Ok(Resolved {
            harness,
            harness_name: descriptor.name.clone(),
            model: defaults.model.clone().filter(|_| same_harness),
            space: defaults
                .space_id
                .as_deref()
                .and_then(|id| self.local_space(id)),
            access: defaults
                .access_mode
                .filter(|mode| zeron_proto::access_mode_supported(harness, *mode))
                .unwrap_or_else(|| default_access(harness)),
        })
    }

    pub(super) fn defaults_summary(&self, defaults: &TelegramDefaults) -> String {
        match self.resolve(defaults) {
            Ok(resolved) => format!(
                "{} · {} · {} · {}",
                resolved.harness_name,
                resolved.model.as_deref().unwrap_or("default model"),
                resolved
                    .space
                    .as_ref()
                    .map_or("no project", |space| space.display_name()),
                resolved.access.label()
            ),
            Err(err) => err,
        }
    }

    /// A Godspeed chat this engine hosts and can still send into.
    fn live_thread(&self, chat_id: &str) -> bool {
        self.deps
            .workspace
            .chat(chat_id)
            .ok()
            .flatten()
            .is_some_and(|chat| chat.device_id == self.device_id)
    }

    fn start_thread(&self, link: &LinkedChat) -> Result<String, String> {
        let resolved = self.resolve(&link.defaults)?;
        let config = ChatConfig {
            harness: resolved.harness,
            model: resolved.model,
            reasoning: None,
            model_options: Default::default(),
            sandbox: resolved.access.legacy().0,
            access_mode: Some(resolved.access),
        };
        let chat_id = new_id();
        let space_id = resolved.space.as_ref().map(|space| space.id.as_str());
        self.deps
            .workspace
            .create_chat(
                &chat_id,
                space_id,
                Some(&self.device_id),
                Some(config),
                None,
            )
            .map_err(|err| format!("Couldn't start a thread: {err}"))?;
        self.store
            .add_thread(&chat_id, link.chat_id, self.options.clock.now())
            .map_err(|err| err.to_string())?;
        Ok(chat_id)
    }

    /// Send `text` into the chat; returns the user message's id and whether
    /// it waits in the queue behind a running turn.
    fn send_to_thread(&self, chat_id: &str, text: &str) -> Result<(String, bool), String> {
        let doc_host = &self.deps.doc_host;
        if self.deps.sessions.turn_in_flight(chat_id) || doc_host.queue_len(chat_id) > 0 {
            let id = doc_host
                .queue_message_with_behavior(chat_id, text, Vec::new(), true)
                .map_err(|err| err.to_string())?;
            return Ok((id, true));
        }
        let request = doc_host
            .request_from_chat_row(chat_id, text)
            .ok_or("That thread is gone. Send /new to start another.")?;
        let message_id = new_id();
        doc_host
            .queue_command(
                chat_id,
                SessionCommandPayload::Run {
                    request,
                    message_id: message_id.clone(),
                },
            )
            .map_err(|err| err.to_string())?;
        Ok((message_id, false))
    }

    pub(super) async fn handle_text(
        self: &Arc<Self>,
        api: &BotApi,
        message: &Message,
        link: LinkedChat,
        text: &str,
    ) {
        let chat = link.chat_id;
        let replied = message.reply_to_message.as_ref().map(|m| m.message_id);
        if let Some(replied) = replied
            && let Ok(Some(prompt)) = self.store.prompt_for_message(chat, replied)
            && prompt.state == super::store::PromptState::Pending
        {
            self.answer_with_text(api, prompt, text, message.message_id)
                .await;
            return;
        }
        let target = replied
            .and_then(|id| self.store.chat_for_message(chat, id).ok().flatten())
            .or(link.thread_id.clone())
            .filter(|id| self.live_thread(id));
        let chat_id = match target {
            Some(chat_id) => chat_id,
            None => match self.start_thread(&link) {
                Ok(chat_id) => chat_id,
                Err(err) => {
                    self.say(api, chat, &err, Some(message.message_id)).await;
                    return;
                }
            },
        };
        if let Err(err) = self.store.set_thread(chat, Some(&chat_id)) {
            tracing::warn!(%err, "telegram: current thread not saved");
        }
        match self.send_to_thread(&chat_id, text) {
            Ok((message_id, queued)) => {
                let turn = PendingTurn {
                    message_id,
                    chat_id,
                    tg_chat: chat,
                    reply_to: Some(message.message_id),
                };
                if let Err(err) = self.store.add_turn(&turn, self.options.clock.now()) {
                    tracing::warn!(%err, "telegram: pending reply not saved");
                }
                if queued {
                    self.say(
                        api,
                        chat,
                        "⏳ Queued. It runs when the current turn finishes.",
                        Some(message.message_id),
                    )
                    .await;
                }
                self.typing(api, chat, true).await;
                self.wake_delivery.notify_one();
            }
            Err(err) => {
                self.say(
                    api,
                    chat,
                    &format!("❌ Couldn't send: {err}"),
                    Some(message.message_id),
                )
                .await;
            }
        }
    }

    pub(super) async fn stop_thread(&self, api: &BotApi, link: &LinkedChat) {
        let running = link
            .thread_id
            .as_deref()
            .filter(|id| self.deps.sessions.turn_in_flight(id));
        let reply = match running {
            None => "Nothing is running in the current thread.".to_string(),
            Some(chat_id) => match self
                .deps
                .doc_host
                .queue_command(chat_id, SessionCommandPayload::Interrupt {})
            {
                Ok(_) => "⏹ Stopping the current run.".to_string(),
                Err(err) => format!("Couldn't stop it: {err}"),
            },
        };
        self.say(api, link.chat_id, &reply, None).await;
    }

    fn thread_state(&self, chat_id: &str) -> &'static str {
        match self.deps.sessions.session_status(chat_id).map(|s| s.status) {
            Some(SessionStatus::Working) => "running",
            Some(SessionStatus::AwaitingInput) => "waiting for you",
            Some(SessionStatus::Errored) => "stopped on an error",
            _ => "idle",
        }
    }

    pub(super) fn status_text(&self, link: &LinkedChat) -> String {
        let thread = link
            .thread_id
            .as_deref()
            .filter(|id| self.live_thread(id))
            .map(|id| {
                let title = self
                    .deps
                    .workspace
                    .chat(id)
                    .ok()
                    .flatten()
                    .and_then(|chat| chat.title)
                    .unwrap_or_else(|| "Untitled".into());
                format!("{title} ({})", self.thread_state(id))
            })
            .unwrap_or_else(|| "none yet (your next message starts one)".into());
        let schedules = self
            .store
            .schedules()
            .map(|rows| rows.iter().filter(|r| r.tg_chat == link.chat_id).count())
            .unwrap_or(0);
        format!(
            "Thread: {thread}\nNew threads: {}\nTime zone: {}\nSchedules: {schedules}",
            self.defaults_summary(&link.defaults),
            link.timezone,
        )
    }

    /// Show "typing…" in the chat, at most every few seconds unless forced.
    pub(super) async fn typing(&self, api: &BotApi, chat: i64, force: bool) {
        let now = tokio::time::Instant::now();
        {
            let mut typing = super::lock(&self.typing);
            if !force
                && typing
                    .get(&chat)
                    .is_some_and(|last| now.duration_since(*last) < super::delivery::TYPING_EVERY)
            {
                return;
            }
            typing.insert(chat, now);
        }
        if let Err(err) = api.send_typing(chat).await {
            tracing::debug!(%err, "telegram: typing indicator failed");
        }
    }
}
