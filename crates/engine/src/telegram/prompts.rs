//! Approvals and questions in Telegram. A Telegram thread's requests go to
//! its chat; an agent run's go to the chat that scheduled it, or to every
//! linked chat for agents made in the app. Each request becomes a card per
//! chat; the first answer anywhere wins, goes out as `RespondInput` through
//! the command ledger, and the other cards close themselves.

use zeron_doc::{MessagePart, SessionCommandPayload};
use zeron_proto::agents::AgentRunFilter;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

use super::Inner;
use super::api::{BotApi, Format, SendOptions};
use super::render::{self, escape_html};
use super::store::{LinkedChat, Prompt, PromptState};

enum Answered {
    Sent,
    NextQuestion,
    Stale,
    Failed(String),
}

impl Answered {
    fn toast(self) -> String {
        match self {
            Self::Sent => "Sent.".into(),
            Self::NextQuestion => "Next question.".into(),
            Self::Stale => "Already answered.".into(),
            Self::Failed(err) => format!("Couldn't send the answer: {err}"),
        }
    }
}

struct Input {
    request_id: String,
    questions: Vec<UserInputQuestion>,
    resolved: bool,
}

impl Inner {
    fn inputs(&self, chat_id: &str) -> Vec<Input> {
        let Ok(entries) = self
            .deps
            .doc_host
            .open(chat_id)
            .and_then(|handle| Ok(handle.doc().read_entries()?))
        else {
            return Vec::new();
        };
        let mut inputs: Vec<Input> = Vec::new();
        for part in entries.iter().flat_map(|entry| entry.parts.iter()) {
            if let MessagePart::Input {
                request_id,
                questions,
                resolved,
                ..
            } = part
            {
                inputs.retain(|input| &input.request_id != request_id);
                inputs.push(Input {
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolved: *resolved,
                });
            }
        }
        inputs
    }

    /// The request still waits for an answer in a live turn.
    fn input_open(&self, chat_id: &str, request_id: &str) -> bool {
        self.deps.sessions.turn_in_flight(chat_id)
            && self
                .inputs(chat_id)
                .iter()
                .any(|input| input.request_id == request_id && !input.resolved)
    }

    /// Linked Telegram chats that should see `chat_id`'s requests.
    fn prompt_targets(&self, chat_id: &str) -> Vec<i64> {
        let linked = |tg: i64| self.linked(tg).is_some();
        if let Ok(Some(owner)) = self.store.thread_owner(chat_id) {
            return [owner].into_iter().filter(|tg| linked(*tg)).collect();
        }
        let agent = self
            .deps
            .agents
            .list_runs(&AgentRunFilter {
                chat_id: Some(chat_id.to_owned()),
                include_archived: true,
                limit: Some(1),
                ..Default::default()
            })
            .ok()
            .and_then(|runs| runs.into_iter().next())
            .map(|run| run.agent_id);
        let Some(agent_id) = agent else {
            return Vec::new();
        };
        match self.store.schedule_for_agent(&agent_id) {
            Ok(Some(row)) => [row.tg_chat].into_iter().filter(|tg| linked(*tg)).collect(),
            _ => self
                .store
                .linked_chats()
                .unwrap_or_default()
                .into_iter()
                .map(|chat| chat.chat_id)
                .collect(),
        }
    }

    fn chat_title(&self, chat_id: &str) -> String {
        self.deps
            .workspace
            .chat(chat_id)
            .ok()
            .flatten()
            .and_then(|chat| chat.title)
            .unwrap_or_else(|| "Untitled thread".into())
    }

    /// Send new cards, retry unsent ones, and close cards whose request was
    /// answered elsewhere or abandoned.
    pub(super) async fn sync_prompts(&self, api: &BotApi) {
        let sessions = self.deps.sessions.watch_sessions().borrow().clone();
        let now = self.options.clock.now();
        for session in sessions.iter().filter(|s| {
            s.status == zeron_proto::SessionStatus::AwaitingInput && s.device_id == self.device_id
        }) {
            let targets = self.prompt_targets(&session.chat_id);
            if targets.is_empty() {
                continue;
            }
            for input in self
                .inputs(&session.chat_id)
                .into_iter()
                .filter(|i| !i.resolved)
            {
                for tg in &targets {
                    if let Err(err) = self.store.insert_prompt(
                        *tg,
                        &session.chat_id,
                        &input.request_id,
                        &input.questions,
                        now,
                    ) {
                        tracing::warn!(%err, "telegram: approval card not recorded");
                    }
                }
            }
        }
        for mut prompt in self.store.pending_prompts().unwrap_or_default() {
            if !self.input_open(&prompt.chat_id, &prompt.request_id) {
                self.close_prompt(
                    api,
                    &mut prompt,
                    "Answered in Godspeed, or no longer needed.",
                )
                .await;
            } else if prompt.message_id.is_none() {
                self.send_card(api, &mut prompt).await;
            }
        }
    }

    async fn send_card(&self, api: &BotApi, prompt: &mut Prompt) {
        let index = prompt.answers.len();
        let Some(question) = prompt.questions.get(index) else {
            return;
        };
        let card = render::prompt_card(
            &self.chat_title(&prompt.chat_id),
            question,
            index,
            prompt.questions.len(),
        );
        let options = SendOptions {
            keyboard: Some(render::prompt_keyboard(prompt.id, index, question)),
            html: true,
            ..Default::default()
        };
        let Some(message) = self.send(api, prompt.tg_chat, &card, options).await else {
            return;
        };
        prompt.message_id = Some(message.message_id);
        prompt.card = card;
        if let Err(err) = self
            .store
            .remember_message(prompt.tg_chat, message.message_id, &prompt.chat_id)
            .and_then(|()| self.store.save_prompt(prompt))
        {
            tracing::warn!(%err, "telegram: approval card not saved");
        }
    }

    /// Freeze a card with `note` appended and its buttons removed.
    async fn finish_card(&self, api: &BotApi, prompt: &Prompt, note: &str) -> String {
        let card = format!("{}\n\n{note}", prompt.card);
        if let Some(message_id) = prompt.message_id
            && let Err(err) = api
                .edit_message_text(prompt.tg_chat, message_id, &card, None, Format::Html)
                .await
        {
            tracing::warn!(%err, "telegram: approval card not updated");
        }
        card
    }

    async fn close_prompt(&self, api: &BotApi, prompt: &mut Prompt, note: &str) {
        if prompt.message_id.is_some() {
            prompt.card = self
                .finish_card(api, prompt, &format!("<i>{}</i>", escape_html(note)))
                .await;
        }
        prompt.state = PromptState::Closed;
        if let Err(err) = self.store.save_prompt(prompt) {
            tracing::warn!(%err, "telegram: approval card not closed");
        }
    }

    /// Record `label` for the card's current question; the last answer
    /// sends the response.
    async fn record_answer(&self, api: &BotApi, mut prompt: Prompt, label: String) -> Answered {
        if !self.input_open(&prompt.chat_id, &prompt.request_id) {
            self.close_prompt(api, &mut prompt, "Already answered in Godspeed.")
                .await;
            return Answered::Stale;
        }
        let index = prompt.answers.len();
        let Some(question) = prompt.questions.get(index).cloned() else {
            return Answered::Stale;
        };
        prompt.answers.push(UserInputAnswer {
            question_id: question.id.clone(),
            labels: vec![label.clone()],
        });
        let outcome = render::outcome_line(&question, &label);
        if prompt.answers.len() < prompt.questions.len() {
            prompt.card = self.finish_card(api, &prompt, &outcome).await;
            prompt.message_id = None;
            if let Err(err) = self.store.save_prompt(&prompt) {
                tracing::warn!(%err, "telegram: answer not saved");
            }
            self.send_card(api, &mut prompt).await;
            return Answered::NextQuestion;
        }
        let sent = self.deps.doc_host.queue_command(
            &prompt.chat_id,
            SessionCommandPayload::RespondInput {
                request_id: prompt.request_id.clone(),
                answers: prompt.answers.clone(),
            },
        );
        if let Err(err) = sent {
            return Answered::Failed(err.to_string());
        }
        prompt.card = self.finish_card(api, &prompt, &outcome).await;
        prompt.state = PromptState::Answered;
        if let Err(err) = self.store.save_prompt(&prompt) {
            tracing::warn!(%err, "telegram: answer not saved");
        }
        self.wake_delivery.notify_one();
        Answered::Sent
    }

    pub(super) async fn answer_button(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        prompt_id: i64,
        question: usize,
        option: usize,
    ) -> Option<String> {
        let prompt = match self.store.prompt(prompt_id) {
            Ok(Some(prompt)) if prompt.tg_chat == link.chat_id => prompt,
            _ => return Some("That request is gone.".into()),
        };
        if prompt.state != PromptState::Pending || question != prompt.answers.len() {
            return Some("Already answered.".into());
        }
        let label = prompt
            .questions
            .get(question)
            .and_then(|q| render::option_label(q, option))?;
        Some(self.record_answer(api, prompt, label).await.toast())
    }

    /// A reply to a card: free text answers a question; an approval takes a
    /// plain yes/no too.
    pub(super) async fn answer_with_text(
        &self,
        api: &BotApi,
        prompt: Prompt,
        text: &str,
        reply_to: i64,
    ) {
        let Some(question) = prompt.questions.get(prompt.answers.len()) else {
            return;
        };
        let label = match &question.approval {
            None => Some(text.trim().to_string()),
            Some(approval) => approval.decision_for(text).and_then(|decision| {
                approval
                    .decisions
                    .iter()
                    .find(|option| option.decision == decision)
                    .map(|option| option.label.clone())
            }),
        };
        let Some(label) = label.filter(|l| !l.is_empty()) else {
            self.say(
                api,
                prompt.tg_chat,
                "Use the buttons on that card to answer this approval.",
                Some(reply_to),
            )
            .await;
            return;
        };
        let tg_chat = prompt.tg_chat;
        match self.record_answer(api, prompt, label).await {
            Answered::Sent | Answered::NextQuestion => {}
            other => {
                self.say(api, tg_chat, &other.toast(), Some(reply_to)).await;
            }
        }
    }
}
