//! Incoming updates: who may talk to the bot, `/link`, and routing each
//! command or message to its handler.

use std::sync::Arc;

use super::api::{BotApi, CallbackQuery, Message, SendOptions, Update};
use super::callback::{Callback, ScheduleAction};
use super::command::{self, Command, HELP};
use super::link::Claim;
use super::store::LinkedChat;
use super::{Inner, host_timezone};

const PRIVATE_ONLY: &str = "I only work in private chats.";
const UNLINKED: &str = "This bot only answers chats linked to Godspeed.\n\nIn Godspeed, open Settings → Agents → Telegram, get a link code, then send /link CODE here.";

impl Inner {
    pub(super) async fn handle_update(self: &Arc<Self>, api: &BotApi, update: Update) {
        if let Some(message) = update.message {
            self.handle_message(api, message).await;
        } else if let Some(query) = update.callback_query {
            self.handle_callback(api, query).await;
        }
    }

    /// Send plain text, split to Telegram's limit. Returns the message ids.
    pub(super) async fn say(
        &self,
        api: &BotApi,
        chat: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Vec<i64> {
        let mut sent = Vec::new();
        for chunk in super::render::chunks(text) {
            let options = SendOptions {
                reply_to: if sent.is_empty() { reply_to } else { None },
                ..Default::default()
            };
            match api.send_message(chat, &chunk, &options).await {
                Ok(message) => sent.push(message.message_id),
                Err(err) => {
                    tracing::warn!(%err, "telegram: send failed");
                    break;
                }
            }
        }
        sent
    }

    pub(super) async fn send(
        &self,
        api: &BotApi,
        chat: i64,
        text: &str,
        options: SendOptions,
    ) -> Option<Message> {
        match api.send_message(chat, text, &options).await {
            Ok(message) => Some(message),
            Err(err) => {
                tracing::warn!(%err, "telegram: send failed");
                None
            }
        }
    }

    pub(super) fn linked(&self, chat: i64) -> Option<LinkedChat> {
        self.store.linked(chat).unwrap_or_else(|err| {
            tracing::warn!(%err, "telegram: link lookup failed");
            None
        })
    }

    async fn handle_message(self: &Arc<Self>, api: &BotApi, message: Message) {
        let Some(text) = message.text.clone() else {
            return;
        };
        let chat = message.chat.id;
        if !message.chat.is_private() {
            self.say(api, chat, PRIVATE_ONLY, None).await;
            return;
        }
        let command = command::parse(&text);
        if let Some(Command::Link(code)) = &command {
            self.link_chat(api, &message, code).await;
            return;
        }
        let Some(link) = self.linked(chat) else {
            self.say(api, chat, UNLINKED, None).await;
            return;
        };
        match command {
            Some(command) => self.run_command(api, link, command).await,
            None => self.handle_text(api, &message, link, &text).await,
        }
    }

    async fn link_chat(&self, api: &BotApi, message: &Message, code: &str) {
        let chat = message.chat.id;
        if self.linked(chat).is_some() {
            self.say(api, chat, "This chat is already linked.", None)
                .await;
            return;
        }
        if code.trim().is_empty() {
            self.say(
                api,
                chat,
                "Usage: /link CODE (get one in Settings → Agents → Telegram)",
                None,
            )
            .await;
            return;
        }
        let reply = match self.claim_link_code(code) {
            Claim::Linked => {
                let label = message
                    .from
                    .as_ref()
                    .map(|user| user.label())
                    .unwrap_or_else(|| chat.to_string());
                let linked = LinkedChat {
                    chat_id: chat,
                    label,
                    linked_at: self.options.clock.now(),
                    timezone: host_timezone(),
                    defaults: self.store.global_defaults().unwrap_or_default(),
                    thread_id: None,
                };
                match self.store.link(&linked) {
                    Ok(()) => {
                        self.changed();
                        format!(
                            "Linked. Message me to start a thread.\n\nNew threads use {}. Times are in {} (change with /tz). /help lists everything.",
                            self.defaults_summary(&linked.defaults),
                            linked.timezone
                        )
                    }
                    Err(err) => format!("Couldn't link this chat: {err}"),
                }
            }
            Claim::Invalid => {
                "That code isn't valid. Get a new one in Settings → Agents → Telegram.".into()
            }
            Claim::Expired => {
                "That code expired. Get a new one in Settings → Agents → Telegram.".into()
            }
        };
        self.say(api, chat, &reply, None).await;
    }

    async fn run_command(self: &Arc<Self>, api: &BotApi, link: LinkedChat, command: Command) {
        let chat = link.chat_id;
        match command {
            Command::Start | Command::Help => {
                self.say(api, chat, HELP, None).await;
            }
            Command::Link(_) => {}
            Command::Unlink => {
                let reply = match self.store.unlink(chat) {
                    Ok(_) => {
                        self.changed();
                        "Unlinked. I won't answer here until you /link again. Your schedules keep running in Godspeed.".to_string()
                    }
                    Err(err) => format!("Couldn't unlink: {err}"),
                };
                self.say(api, chat, &reply, None).await;
            }
            Command::New => {
                let reply = match self.store.set_thread(chat, None) {
                    Ok(()) => format!(
                        "Fresh thread. Your next message starts it with {}.",
                        self.defaults_summary(&link.defaults)
                    ),
                    Err(err) => format!("Couldn't start over: {err}"),
                };
                self.say(api, chat, &reply, None).await;
            }
            Command::Provider => self.show_providers(api, &link).await,
            Command::Model => self.show_models(api, &link, 0, None).await,
            Command::Project => self.show_projects(api, &link).await,
            Command::Access => self.show_access(api, &link).await,
            Command::Status => {
                let status = self.status_text(&link);
                self.say(api, chat, &status, None).await;
            }
            Command::Stop => self.stop_thread(api, &link).await,
            Command::Timezone(zone) => self.set_timezone(api, &link, &zone).await,
            Command::Schedule(args) => self.create_schedule(api, &link, &args).await,
            Command::Schedules => self.list_schedules(api, &link).await,
            Command::Pause(id) => {
                self.schedule_command(api, &link, &id, ScheduleAction::Pause)
                    .await
            }
            Command::Resume(id) => {
                self.schedule_command(api, &link, &id, ScheduleAction::Resume)
                    .await
            }
            Command::RunNow(id) => {
                self.schedule_command(api, &link, &id, ScheduleAction::RunNow)
                    .await
            }
            Command::Unschedule(id) => {
                self.schedule_command(api, &link, &id, ScheduleAction::Delete)
                    .await
            }
        }
    }

    async fn set_timezone(&self, api: &BotApi, link: &LinkedChat, zone: &str) {
        let zone = zone.trim();
        let reply = if zone.is_empty() {
            format!(
                "Your time zone is {}. Change it with /tz Area/City, e.g. /tz Asia/Kolkata.",
                link.timezone
            )
        } else {
            match zone.parse::<chrono_tz::Tz>() {
                Ok(tz) => match self.store.set_timezone(link.chat_id, tz.name()) {
                    Ok(()) => {
                        self.changed();
                        format!("Time zone set to {}. New schedules use it.", tz.name())
                    }
                    Err(err) => format!("Couldn't save it: {err}"),
                },
                Err(_) => format!(
                    "I don't know the time zone {zone:?}. Use an IANA name like Europe/Berlin or Asia/Kolkata."
                ),
            }
        };
        self.say(api, link.chat_id, &reply, None).await;
    }

    async fn handle_callback(self: &Arc<Self>, api: &BotApi, query: CallbackQuery) {
        let Some(message) = query.message.clone() else {
            let _ = api.answer_callback_query(&query.id, None).await;
            return;
        };
        let chat = message.chat.id;
        let Some(link) = self.linked(chat).filter(|_| message.chat.is_private()) else {
            let _ = api
                .answer_callback_query(&query.id, Some("This chat isn't linked to Godspeed."))
                .await;
            return;
        };
        let Some(callback) = query.data.as_deref().and_then(Callback::decode) else {
            let _ = api
                .answer_callback_query(&query.id, Some("That button no longer works."))
                .await;
            return;
        };
        let toast = match callback {
            Callback::Provider(harness) => self.pick_provider(api, &link, &message, harness).await,
            Callback::Model(index) => self.pick_model(api, &link, &message, index).await,
            Callback::ModelPage(page) => {
                self.show_models(api, &link, page, Some(message.message_id))
                    .await;
                None
            }
            Callback::Project(space) => self.pick_project(api, &link, &message, space).await,
            Callback::Access(mode) => self.pick_access(api, &link, &message, mode).await,
            Callback::Schedule { id, action } => {
                self.schedule_button(api, &link, &message, id, action).await
            }
            Callback::Answer {
                prompt,
                question,
                option,
            } => {
                self.answer_button(api, &link, prompt, question, option)
                    .await
            }
            Callback::Show { prompt } => self.show_prompt(api, &link, prompt).await,
        };
        let _ = api.answer_callback_query(&query.id, toast.as_deref()).await;
    }
}
