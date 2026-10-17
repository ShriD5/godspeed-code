//! Approvals and questions in Telegram. A Telegram thread's requests go to
//! its chat; an agent run's go to the chat that scheduled it, or to every
//! linked chat for agents made in the app. Each request becomes a card per
//! chat; the first answer anywhere wins, goes out as `RespondInput` through
//! the command ledger, and the other cards close themselves. Bursts from
//! agents fold into one digest message ([`super::digest`]).

use std::collections::BTreeMap;

use zeron_doc::{MessagePart, SessionCommandPayload};
use zeron_proto::agents::AgentRunFilter;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

use super::api::{BotApi, Format, SendOptions};
use super::digest::{self, Plan};
use super::render::{self, DigestEntry, escape_html};
use super::store::{LinkedChat, Prompt, PromptState};
use super::{Inner, lock};

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

    /// A card for the user's own Telegram thread: never held for a digest.
    fn in_own_thread(&self, prompt: &Prompt) -> bool {
        matches!(self.store.thread_owner(&prompt.chat_id), Ok(Some(owner)) if owner == prompt.tg_chat)
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
        let mut waiting: BTreeMap<i64, Vec<Prompt>> = BTreeMap::new();
        for mut prompt in self.store.pending_prompts().unwrap_or_default() {
            if !self.input_open(&prompt.chat_id, &prompt.request_id) {
                self.close_prompt(
                    api,
                    &mut prompt,
                    "Answered in Godspeed, or no longer needed.",
                )
                .await;
            } else if prompt.message_id.is_none() {
                if self.in_own_thread(&prompt) {
                    self.send_card(api, &mut prompt).await;
                } else {
                    waiting.entry(prompt.tg_chat).or_default().push(prompt);
                }
            }
        }
        for (tg_chat, prompts) in waiting {
            self.deliver_waiting(api, tg_chat, prompts, now).await;
        }
    }

    /// Cards for one chat, one by one or as a digest (see [`digest::plan`]).
    async fn deliver_waiting(
        &self,
        api: &BotApi,
        tg_chat: i64,
        mut prompts: Vec<Prompt>,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let sent = {
            let mut sends = lock(&self.card_sends);
            let sent = sends.entry(tg_chat).or_default();
            digest::prune(sent, now);
            sent.clone()
        };
        let arrived: Vec<_> = prompts.iter().map(|p| p.created_at).collect();
        match digest::plan(&sent, &arrived, now) {
            Plan::Hold => {}
            Plan::Cards => {
                for prompt in &mut prompts {
                    if self.send_card(api, prompt).await {
                        self.note_card_sent(tg_chat, now);
                    }
                }
            }
            Plan::Digest => {
                if self.send_digest(api, tg_chat, &mut prompts).await {
                    self.note_card_sent(tg_chat, now);
                }
            }
        }
    }

    fn note_card_sent(&self, tg_chat: i64, at: chrono::DateTime<chrono::Utc>) {
        lock(&self.card_sends).entry(tg_chat).or_default().push(at);
    }

    async fn send_digest(&self, api: &BotApi, tg_chat: i64, prompts: &mut [Prompt]) -> bool {
        let lines: Vec<String> = prompts
            .iter()
            .enumerate()
            .map(|(ix, p)| render::digest_line(ix + 1, &self.chat_title(&p.chat_id), &p.questions))
            .collect();
        let entries: Vec<DigestEntry<'_>> = prompts
            .iter()
            .enumerate()
            .map(|(ix, p)| DigestEntry {
                number: ix + 1,
                prompt_id: p.id,
                questions: &p.questions,
            })
            .collect();
        let options = SendOptions {
            keyboard: Some(render::digest_keyboard(&entries)),
            html: true,
            ..Default::default()
        };
        let text = render::digest_text(prompts.len(), &lines);
        let Some(message) = self.send(api, tg_chat, &text, options).await else {
            return false;
        };
        for (prompt, line) in prompts.iter_mut().zip(lines) {
            prompt.message_id = Some(message.message_id);
            prompt.card = line;
            if let Err(err) = self.store.save_prompt(prompt) {
                tracing::warn!(%err, "telegram: digest item not saved");
            }
        }
        true
    }

    /// The other cards sharing `prompt`'s message, when it rides a digest.
    fn digest_of(&self, prompt: &Prompt) -> Option<Vec<Prompt>> {
        let message_id = prompt.message_id?;
        self.store
            .prompts_for_message_all(prompt.tg_chat, message_id)
            .ok()
            .filter(|members| members.len() > 1)
    }

    /// Re-render a digest from its items: waiting ones keep their buttons,
    /// settled ones show their outcome.
    async fn refresh_digest(&self, api: &BotApi, members: &[Prompt]) {
        let Some(first) = members.first() else {
            return;
        };
        let Some(message_id) = first.message_id else {
            return;
        };
        let waiting: Vec<DigestEntry<'_>> = members
            .iter()
            .enumerate()
            .filter(|(_, p)| p.state == PromptState::Pending)
            .map(|(ix, p)| DigestEntry {
                number: ix + 1,
                prompt_id: p.id,
                questions: &p.questions,
            })
            .collect();
        let lines: Vec<String> = members.iter().map(|p| p.card.clone()).collect();
        let text = render::digest_text(waiting.len(), &lines);
        let keyboard = render::digest_keyboard(&waiting);
        if let Err(err) = api
            .edit_message_text(
                first.tg_chat,
                message_id,
                &text,
                Some(&keyboard),
                Format::Html,
            )
            .await
        {
            tracing::warn!(%err, "telegram: digest not updated");
        }
    }

    /// Settle `prompt` as `state` with `note` shown: its own card freezes
    /// without buttons, or its digest line takes the note and the digest
    /// re-renders.
    async fn settle(&self, api: &BotApi, prompt: &mut Prompt, state: PromptState, note: &str) {
        prompt.state = state;
        match self.digest_of(prompt) {
            Some(_) => {
                prompt.card = format!("{}  {note}", prompt.card);
                if let Err(err) = self.store.save_prompt(prompt) {
                    tracing::warn!(%err, "telegram: digest item not saved");
                }
                if let Some(members) = self.digest_of(prompt) {
                    self.refresh_digest(api, &members).await;
                }
            }
            None => {
                if prompt.message_id.is_some() {
                    prompt.card = self.finish_card(api, prompt, note).await;
                }
                if let Err(err) = self.store.save_prompt(prompt) {
                    tracing::warn!(%err, "telegram: card not saved");
                }
            }
        }
    }

    /// Pull a digest item out into its own card (questions answer there).
    pub(super) async fn show_prompt(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        prompt_id: i64,
    ) -> Option<String> {
        let mut prompt = match self.store.prompt(prompt_id) {
            Ok(Some(prompt)) if prompt.tg_chat == link.chat_id => prompt,
            _ => return Some("That request is gone.".into()),
        };
        if prompt.state != PromptState::Pending {
            return Some("Already answered.".into());
        }
        let digest = self.digest_of(&prompt);
        prompt.message_id = None;
        prompt.card.clear();
        if let Err(err) = self.store.save_prompt(&prompt) {
            tracing::warn!(%err, "telegram: card not saved");
        }
        if let Some(members) = digest {
            let rest: Vec<Prompt> = members.into_iter().filter(|p| p.id != prompt.id).collect();
            self.refresh_digest(api, &rest).await;
        }
        self.send_card(api, &mut prompt).await;
        None
    }

    async fn send_card(&self, api: &BotApi, prompt: &mut Prompt) -> bool {
        let index = prompt.answers.len();
        let Some(question) = prompt.questions.get(index) else {
            return false;
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
            return false;
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
        true
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
        let note = format!("<i>{}</i>", escape_html(note));
        self.settle(api, prompt, PromptState::Closed, &note).await;
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
        self.settle(api, &mut prompt, PromptState::Answered, &outcome)
            .await;
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
        if self.digest_of(&prompt).is_some() {
            self.say(
                api,
                prompt.tg_chat,
                "Use the buttons on that digest to answer.",
                Some(reply_to),
            )
            .await;
            return;
        }
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
