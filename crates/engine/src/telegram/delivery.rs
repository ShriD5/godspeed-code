//! What the bot sends on its own: replies to Telegram messages once their
//! turn settles (typing meanwhile), approval cards, and the results of
//! schedules made from Telegram. One loop, woken by session and agent
//! changes and a short tick; everything it sends is recorded first-win so
//! a restart never repeats it.

use std::sync::Arc;
use std::time::Duration;

use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_proto::SessionStatus;
use zeron_proto::agents::{AgentRun, AgentRunFilter, AgentRunStatus};

use super::Inner;
use super::api::BotApi;
use super::schedules::local_time;
use super::store::{LinkedChat, PendingTurn, ScheduleRow};

pub(super) const TYPING_EVERY: Duration = Duration::from_secs(4);
const TICK: Duration = Duration::from_secs(4);
/// Session updates arrive in bursts while a turn streams.
const COALESCE: Duration = Duration::from_millis(100);
const ERROR_CHARS: usize = 600;
const RECENT_RUNS: u32 = 20;

/// The assistant's answer to one user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Reply {
    pub text: String,
    pub error: Option<String>,
    pub stopped: bool,
}

enum TurnState {
    /// The chat is gone; nothing will ever answer.
    Gone,
    Waiting,
    Done(Reply),
}

fn excerpt(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// The settled reply to the user entry at `index`: the assistant entries
/// up to the next user entry, once the last of them stopped streaming.
pub(super) fn settled_reply(entries: &[SessionMessageEntry], index: usize) -> Option<Reply> {
    let rest = entries.get(index + 1..)?;
    let end = rest
        .iter()
        .position(|e| e.role == MessageRole::User)
        .unwrap_or(rest.len());
    let reply: Vec<&SessionMessageEntry> = rest[..end]
        .iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .collect();
    let last = reply.last()?;
    if last.status == Some(MessageStatus::Streaming) {
        return None;
    }
    let mut text = String::new();
    let mut error = None;
    for part in reply.iter().flat_map(|e| e.parts.iter()) {
        match part {
            MessagePart::Text { text: chunk, .. } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(chunk);
            }
            MessagePart::Error { message, .. } => error = Some(message.clone()),
            _ => {}
        }
    }
    Some(Reply {
        text: text.trim().to_string(),
        error,
        stopped: last.status == Some(MessageStatus::Aborted),
    })
}

/// The reply to the newest user entry (an agent run's prompt).
fn last_reply(entries: &[SessionMessageEntry]) -> Option<Reply> {
    let index = entries.iter().rposition(|e| e.role == MessageRole::User)?;
    settled_reply(entries, index)
}

impl Reply {
    pub(super) fn render(&self) -> String {
        let mut out = self.text.clone();
        let mut note = |line: String| {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&line);
        };
        if let Some(error) = &self.error {
            note(format!("❌ {}", excerpt(error, ERROR_CHARS)));
        } else if self.stopped {
            note("⏹ Stopped.".into());
        }
        if out.is_empty() {
            out.push_str("✅ Done.");
        }
        out
    }
}

impl Inner {
    pub(super) async fn deliver_loop(self: Arc<Self>) {
        let mut sessions = self.deps.sessions.watch_sessions();
        let mut agents = self.deps.agents.watch();
        loop {
            if let Some(api) = self.api() {
                self.deliver(&api).await;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                changed = sessions.changed() => if changed.is_err() { return },
                changed = agents.changed() => if changed.is_err() { return },
                _ = self.wake_delivery.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
            tokio::time::sleep(COALESCE).await;
        }
    }

    async fn deliver(&self, api: &BotApi) {
        self.deliver_replies(api).await;
        self.sync_prompts(api).await;
        self.deliver_run_results(api).await;
    }

    fn entries(&self, chat_id: &str) -> Option<Vec<SessionMessageEntry>> {
        self.deps
            .doc_host
            .open(chat_id)
            .ok()?
            .doc()
            .read_entries()
            .ok()
    }

    fn turn_state(&self, turn: &PendingTurn) -> TurnState {
        let Ok(Some(chat)) = self.deps.workspace.chat(&turn.chat_id) else {
            return TurnState::Gone;
        };
        let running = self.deps.sessions.turn_in_flight(&turn.chat_id);
        let Some(entries) = self.entries(&turn.chat_id) else {
            return TurnState::Waiting;
        };
        let Some(index) = entries
            .iter()
            .position(|e| e.id == turn.message_id && e.role == MessageRole::User)
        else {
            return TurnState::Waiting;
        };
        let answered_later = entries[index + 1..]
            .iter()
            .any(|e| e.role == MessageRole::User);
        match settled_reply(&entries, index) {
            // A settled segment mid-turn may still be followed by another.
            Some(reply) if answered_later || !running => {
                let mut reply = reply;
                if reply.error.is_none()
                    && let Some(limit) = chat.limit.filter(|_| !running)
                {
                    reply.error = Some(match limit.reset_at {
                        Some(at) => {
                            let zone = self
                                .linked(turn.tg_chat)
                                .map(|link| link.timezone)
                                .unwrap_or_default();
                            format!("Hit the usage limit. It resets {}.", local_time(at, &zone))
                        }
                        None => "Hit the usage limit.".into(),
                    });
                }
                TurnState::Done(reply)
            }
            // Errored after this message was written, with nothing to show.
            None if !running
                && self
                    .deps
                    .sessions
                    .session_status(&turn.chat_id)
                    .is_some_and(|s| {
                        s.status == SessionStatus::Errored
                            && s.updated_at.timestamp_millis() >= entries[index].created_at
                    }) =>
            {
                TurnState::Done(Reply {
                    text: String::new(),
                    error: Some("The run failed before it replied.".into()),
                    stopped: false,
                })
            }
            _ => TurnState::Waiting,
        }
    }

    async fn deliver_replies(&self, api: &BotApi) {
        for turn in self.store.pending_turns().unwrap_or_default() {
            if self.linked(turn.tg_chat).is_none() {
                let _ = self.store.remove_turn(&turn.message_id);
                continue;
            }
            match self.turn_state(&turn) {
                TurnState::Gone => {
                    let _ = self.store.remove_turn(&turn.message_id);
                }
                TurnState::Waiting => {
                    // Typing while it works; not while it waits on the user.
                    let working = self
                        .deps
                        .sessions
                        .session_status(&turn.chat_id)
                        .is_some_and(|s| s.status == SessionStatus::Working);
                    if working {
                        self.typing(api, turn.tg_chat, false).await;
                    }
                }
                TurnState::Done(reply) => {
                    let sent = self
                        .say(api, turn.tg_chat, &reply.render(), turn.reply_to)
                        .await;
                    if sent.is_empty() {
                        continue;
                    }
                    for message_id in sent {
                        let _ =
                            self.store
                                .remember_message(turn.tg_chat, message_id, &turn.chat_id);
                    }
                    let _ = self.store.remove_turn(&turn.message_id);
                }
            }
        }
    }

    fn run_report(&self, row: &ScheduleRow, run: &AgentRun, link: &LinkedChat) -> String {
        let name = format!("#{} ({})", row.id, row.label);
        match run.status {
            AgentRunStatus::Succeeded => {
                let reply = run
                    .chat_id
                    .as_deref()
                    .and_then(|chat| self.entries(chat))
                    .and_then(|entries| last_reply(&entries))
                    .map(|reply| reply.text)
                    .filter(|text| !text.is_empty())
                    .or_else(|| run.summary.clone())
                    .unwrap_or_else(|| "Done, no text reply.".into());
                format!("✅ {name} finished\n\n{reply}")
            }
            AgentRunStatus::Limited => {
                let next = self
                    .deps
                    .agents
                    .list()
                    .ok()
                    .and_then(|agents| agents.into_iter().find(|a| a.id == row.agent_id))
                    .and_then(|agent| agent.next_fire_at)
                    .map(|at| format!(" Next run {}.", local_time(at, &link.timezone)))
                    .unwrap_or_default();
                format!("⏸ {name} hit a usage limit and is Limited.{next}")
            }
            _ => format!(
                "❌ {name} failed: {}",
                excerpt(
                    run.summary.as_deref().unwrap_or("no reason was given"),
                    ERROR_CHARS
                )
            ),
        }
    }

    async fn deliver_run_results(&self, api: &BotApi) {
        let now = self.options.clock.now();
        for row in self.store.schedules().unwrap_or_default() {
            let Some(link) = self.linked(row.tg_chat) else {
                continue;
            };
            let runs = self
                .deps
                .agents
                .list_runs(&AgentRunFilter {
                    agent_id: Some(row.agent_id.clone()),
                    include_archived: true,
                    limit: Some(RECENT_RUNS),
                    ..Default::default()
                })
                .unwrap_or_default();
            for run in runs.into_iter().rev() {
                let reportable = matches!(
                    run.status,
                    AgentRunStatus::Succeeded | AgentRunStatus::Failed | AgentRunStatus::Limited
                ) && run.finished_at.is_some_and(|at| at >= row.created_at);
                if !reportable || self.store.is_notified(&run.id).unwrap_or(true) {
                    continue;
                }
                let sent = self
                    .say(api, row.tg_chat, &self.run_report(&row, &run, &link), None)
                    .await;
                if sent.is_empty() {
                    return;
                }
                let _ = self.store.mark_notified(&run.id, now);
                if let Some(chat_id) = &run.chat_id {
                    for message_id in sent {
                        let _ = self
                            .store
                            .remember_message(row.tg_chat, message_id, chat_id);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        role: MessageRole,
        status: Option<MessageStatus>,
        parts: Vec<MessagePart>,
    ) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn text(t: &str) -> MessagePart {
        MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }
    }

    #[test]
    fn replies_settle_per_user_message() {
        let entries = vec![
            entry("u1", MessageRole::User, None, vec![text("hi")]),
            entry(
                "a1",
                MessageRole::Assistant,
                Some(MessageStatus::Complete),
                vec![text("first")],
            ),
            entry("u2", MessageRole::User, None, vec![text("again")]),
            entry(
                "a2",
                MessageRole::Assistant,
                Some(MessageStatus::Streaming),
                vec![text("sec")],
            ),
        ];
        assert_eq!(settled_reply(&entries, 0).unwrap().text, "first");
        assert_eq!(settled_reply(&entries, 2), None);
        assert_eq!(last_reply(&entries), None);
    }

    #[test]
    fn errors_and_stops_render_after_partial_text() {
        let entries = vec![
            entry("u1", MessageRole::User, None, vec![]),
            entry(
                "a1",
                MessageRole::Assistant,
                Some(MessageStatus::Complete),
                vec![
                    text("Checked half."),
                    MessagePart::Error {
                        id: "e".into(),
                        message: "rate limited".into(),
                    },
                ],
            ),
        ];
        assert_eq!(
            settled_reply(&entries, 0).unwrap().render(),
            "Checked half.\n\n❌ rate limited"
        );
        let stopped = Reply {
            text: String::new(),
            error: None,
            stopped: true,
        };
        assert_eq!(stopped.render(), "⏹ Stopped.");
        let empty = Reply {
            text: String::new(),
            error: None,
            stopped: false,
        };
        assert_eq!(empty.render(), "✅ Done.");
    }
}
