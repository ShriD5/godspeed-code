//! The queue itself, derived from state the engine already keeps: registry
//! chat rows and live session statuses, the open input requests in chat
//! docs, the always-on agent run inbox, and the questions this service
//! timed out. Pure, so the ordering and folding rules test without an
//! engine.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeron_doc::{MessagePart, SessionMessageEntry};
use zeron_proto::agents::{AgentRun, AgentRunStatus};
use zeron_proto::attention::{
    AttentionAction, AttentionAgentRun, AttentionItem, AttentionKind, attention_order,
};
use zeron_proto::{
    Chat, ChatIndicator, LifecycleState, Session, Space, UserInputQuestion, approval_risk,
};

const SUMMARY_CHARS: usize = 140;

/// An input request still waiting in a live turn.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PendingInput {
    pub chat_id: String,
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    /// When this engine first saw it waiting.
    pub since: DateTime<Utc>,
}

impl PendingInput {
    /// The single permission request this input is, if it is one.
    pub fn approval(&self) -> Option<&zeron_proto::ApprovalRequest> {
        match self.questions.as_slice() {
            [question] => question.approval.as_ref(),
            _ => None,
        }
    }

    /// Only plain questions time out; anything carrying an approval waits.
    pub fn can_time_out(&self) -> bool {
        !self.questions.is_empty() && self.questions.iter().all(|q| q.approval.is_none())
    }
}

/// A question this service answered for the user after nobody did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TimedOutQuestion {
    pub chat_id: String,
    pub request_id: String,
    pub question: String,
    pub minutes: u32,
    pub timed_out_at: DateTime<Utc>,
}

/// The unresolved input requests in a transcript, oldest first.
pub(super) fn open_inputs(
    entries: &[SessionMessageEntry],
) -> Vec<(String, Vec<UserInputQuestion>)> {
    let mut open: Vec<(String, Vec<UserInputQuestion>)> = Vec::new();
    for part in entries.iter().flat_map(|entry| entry.parts.iter()) {
        if let MessagePart::Input {
            request_id,
            questions,
            resolved,
            ..
        } = part
        {
            open.retain(|(id, _)| id != request_id);
            if !resolved {
                open.push((request_id.clone(), questions.clone()));
            }
        }
    }
    open
}

pub(super) struct Sources<'a> {
    pub device_id: &'a str,
    pub chats: &'a [Chat],
    pub spaces: &'a [Space],
    pub sessions: &'a [Session],
    pub pending: &'a [PendingInput],
    /// Newest first, as the agent store lists them.
    pub runs: &'a [AgentRun],
    pub timed_out: &'a [TimedOutQuestion],
}

pub(super) fn derive(sources: &Sources, now: DateTime<Utc>) -> Vec<AttentionItem> {
    let chats: HashMap<&str, &Chat> = sources
        .chats
        .iter()
        .filter(|chat| chat.device_id == sources.device_id && !chat.archived)
        .filter(|chat| !zeron_proto::voice::is_orchestrator_chat(&chat.id))
        .map(|chat| (chat.id.as_str(), chat))
        .collect();
    let spaces: HashMap<&str, &Space> = sources.spaces.iter().map(|s| (s.id.as_str(), s)).collect();
    let sessions: HashMap<&str, &Session> = sources
        .sessions
        .iter()
        .map(|s| (s.chat_id.as_str(), s))
        .collect();
    let mut latest_run: HashMap<&str, &AgentRun> = HashMap::new();
    for run in sources.runs {
        if let Some(chat_id) = run.chat_id.as_deref() {
            latest_run.entry(chat_id).or_insert(run);
        }
    }
    let waiting: HashSet<&str> = sources.pending.iter().map(|p| p.chat_id.as_str()).collect();
    let context = Context {
        spaces: &spaces,
        latest_run: &latest_run,
    };
    let mut items = Vec::new();

    for input in sources.pending {
        if let Some(chat) = chats.get(input.chat_id.as_str()) {
            items.push(context.input_item(chat, input));
        }
    }

    let reported_runs: Vec<&AgentRun> = sources
        .runs
        .iter()
        .filter(|run| reportable(run))
        .filter(|run| {
            let chat = run.chat_id.as_deref().and_then(|id| chats.get(id));
            chat.is_none_or(|chat| !waiting.contains(chat.id.as_str()) && !seen_since(chat, run))
        })
        .collect();
    let run_chats: HashSet<&str> = reported_runs
        .iter()
        .filter_map(|run| run.chat_id.as_deref())
        .collect();

    for chat in chats.values().filter(|chat| chat.is_top_level()) {
        if waiting.contains(chat.id.as_str())
            || run_chats.contains(chat.id.as_str())
            || chat.lifecycle.at(now) != LifecycleState::Active
        {
            continue;
        }
        let session = sessions.get(chat.id.as_str()).copied();
        if let Some(item) = context.chat_item(chat, session, now) {
            items.push(item);
        }
    }

    for run in reported_runs {
        let chat = run.chat_id.as_deref().and_then(|id| chats.get(id)).copied();
        items.push(context.run_item(run, chat));
    }

    for timed_out in sources.timed_out {
        let Some(chat) = chats.get(timed_out.chat_id.as_str()) else {
            continue;
        };
        if chat
            .last_seen_at
            .is_some_and(|seen| seen >= timed_out.timed_out_at)
        {
            continue;
        }
        items.push(context.timed_out_item(chat, timed_out));
    }

    items.sort_by(attention_order);
    items
}

/// An unread, finished run the inbox should still report.
fn reportable(run: &AgentRun) -> bool {
    !run.read
        && !run.archived
        && matches!(
            run.status,
            AgentRunStatus::Succeeded | AgentRunStatus::Failed | AgentRunStatus::Limited
        )
}

/// The run's chat was opened after the run finished: that reads it.
fn seen_since(chat: &Chat, run: &AgentRun) -> bool {
    let finished = run.finished_at.unwrap_or(run.created_at);
    chat.last_seen_at.is_some_and(|seen| seen >= finished)
}

struct Context<'a> {
    spaces: &'a HashMap<&'a str, &'a Space>,
    latest_run: &'a HashMap<&'a str, &'a AgentRun>,
}

impl Context<'_> {
    fn base(&self, id: String, kind: AttentionKind, chat: &Chat) -> AttentionItem {
        let space = chat.space_id.as_deref().and_then(|id| self.spaces.get(id));
        AttentionItem {
            id,
            kind,
            chat_id: Some(chat.id.clone()),
            title: chat_title(chat),
            space_id: chat.space_id.clone(),
            project: space.map(|s| s.display_name().to_string()),
            harness: chat.config.as_ref().map(|c| c.harness),
            created_at: chat.last_message_at.unwrap_or(chat.created_at),
            risk: None,
            summary: String::new(),
            actions: vec![AttentionAction::Open, AttentionAction::Dismiss],
            request_id: None,
            approval: None,
            agent_run: self
                .latest_run
                .get(chat.id.as_str())
                .map(|run| agent_run(run)),
        }
    }

    fn input_item(&self, chat: &Chat, input: &PendingInput) -> AttentionItem {
        let key = format!("{}:{}", chat.id, input.request_id);
        let mut item = match input.approval() {
            Some(approval) => {
                let mut item = self.base(format!("approval:{key}"), AttentionKind::Approval, chat);
                item.risk = Some(approval_risk(approval, chat.cwd.as_deref()));
                item.summary = one_line(approval.command.as_deref().unwrap_or(&approval.title));
                item.approval = Some(approval.clone());
                item.actions = if approval.decisions.iter().any(|d| d.decision.approves()) {
                    vec![
                        AttentionAction::Approve,
                        AttentionAction::Decline,
                        AttentionAction::Open,
                    ]
                } else {
                    vec![AttentionAction::Decline, AttentionAction::Open]
                };
                item
            }
            None => {
                let mut item = self.base(format!("question:{key}"), AttentionKind::Question, chat);
                item.summary = question_summary(&input.questions);
                item.actions = vec![AttentionAction::Open];
                item
            }
        };
        item.created_at = input.since;
        item.request_id = Some(input.request_id.clone());
        item
    }

    fn chat_item(
        &self,
        chat: &Chat,
        session: Option<&Session>,
        now: DateTime<Utc>,
    ) -> Option<AttentionItem> {
        let status = zeron_proto::view::display_status(chat, session, now);
        let id = format!("chat:{}", chat.id);
        let preview = chat.last_message_preview.as_deref().map(one_line);
        let item = match status {
            ChatIndicator::Errored => {
                let mut item = self.base(id, AttentionKind::Failed, chat);
                item.summary = preview.unwrap_or_else(|| "The last turn failed".into());
                item
            }
            ChatIndicator::Limited => {
                let limit = chat.limit.as_ref()?;
                let mut item = self.base(id, AttentionKind::Limited, chat);
                item.created_at = limit.limited_at;
                item.summary = match &limit.window {
                    Some(window) => format!("Hit the {window} usage limit"),
                    None => "Hit the usage limit".into(),
                };
                item.actions = vec![AttentionAction::Open];
                item
            }
            ChatIndicator::Completed => {
                let mut item = self.base(id, AttentionKind::Finished, chat);
                item.summary = preview.unwrap_or_else(|| "Finished".into());
                item
            }
            _ => return None,
        };
        Some(item)
    }

    fn run_item(&self, run: &AgentRun, chat: Option<&Chat>) -> AttentionItem {
        let kind = match run.status {
            AgentRunStatus::Failed => AttentionKind::Failed,
            AgentRunStatus::Limited => AttentionKind::Limited,
            _ => AttentionKind::Finished,
        };
        let id = format!("run:{}", run.id);
        let mut item = match chat {
            Some(chat) => self.base(id, kind, chat),
            None => AttentionItem {
                id,
                kind,
                chat_id: None,
                title: run.agent_name.clone(),
                space_id: None,
                project: None,
                harness: None,
                created_at: run.created_at,
                risk: None,
                summary: String::new(),
                actions: vec![AttentionAction::Dismiss],
                request_id: None,
                approval: None,
                agent_run: None,
            },
        };
        item.created_at = run.finished_at.unwrap_or(run.created_at);
        item.summary = run.summary.as_deref().map(one_line).unwrap_or_else(|| {
            match kind {
                AttentionKind::Failed => "The run failed",
                AttentionKind::Limited => "Hit the usage limit",
                _ => "Finished",
            }
            .into()
        });
        item.agent_run = Some(agent_run(run));
        item
    }

    fn timed_out_item(&self, chat: &Chat, timed_out: &TimedOutQuestion) -> AttentionItem {
        let mut item = self.base(
            format!("timedOut:{}:{}", chat.id, timed_out.request_id),
            AttentionKind::TimedOut,
            chat,
        );
        item.created_at = timed_out.timed_out_at;
        item.request_id = Some(timed_out.request_id.clone());
        item.summary = one_line(&format!(
            "No answer after {} min: {}",
            timed_out.minutes, timed_out.question
        ));
        item
    }
}

fn agent_run(run: &AgentRun) -> AttentionAgentRun {
    AttentionAgentRun {
        run_id: run.id.clone(),
        agent_id: run.agent_id.clone(),
        agent_name: run.agent_name.clone(),
    }
}

fn chat_title(chat: &Chat) -> String {
    chat.title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Untitled thread")
        .to_string()
}

pub(super) fn question_summary(questions: &[UserInputQuestion]) -> String {
    let Some(first) = questions.first() else {
        return "Waiting on your input".into();
    };
    let text = if first.question.trim().is_empty() {
        &first.header
    } else {
        &first.question
    };
    match questions.len() {
        1 => one_line(text),
        n => one_line(&format!("{text} (+{} more)", n - 1)),
    }
}

/// The first non-empty line, cut to a sidebar-friendly length.
fn one_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= SUMMARY_CHARS {
        return line.to_string();
    }
    let mut out: String = line.chars().take(SUMMARY_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone};
    use zeron_proto::agents::AgentTriggerKind;
    use zeron_proto::{
        ApprovalKind, ApprovalRequest, ApprovalRisk, ChatConfig, ChatLifecycle, ChatLimit,
        HarnessId, LimitRecovery, SandboxLevel, SessionStatus,
    };

    const DEVICE: &str = "dev-a";
    const WS: &str = "/work/app";

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 9, 0, 0).unwrap() + TimeDelta::minutes(minute)
    }

    fn chat(id: &str) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": DEVICE,
            "title": format!("Chat {id}"),
            "archived": false,
            "cwd": WS,
            "branch": null,
            "checkoutId": null,
            "config": null,
            "lastMessagePreview": format!("{id} says done"),
            "lastMessageAt": at(1),
            "createdAt": at(0),
            "spaceId": "space-1",
        }))
        .unwrap()
    }

    fn with_config(mut chat: Chat) -> Chat {
        chat.config = Some(ChatConfig {
            harness: HarnessId::Codex,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            access_mode: None,
        });
        chat
    }

    fn seen(mut chat: Chat, minute: i64) -> Chat {
        chat.last_seen_at = Some(at(minute));
        chat
    }

    fn space() -> Space {
        serde_json::from_value(serde_json::json!({
            "id": "space-1",
            "deviceId": DEVICE,
            "path": WS,
            "createdAt": at(0),
        }))
        .unwrap()
    }

    fn session(chat_id: &str, status: SessionStatus, now: DateTime<Utc>) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: chat_id.into(),
            device_id: DEVICE.into(),
            status,
            started_at: None,
            updated_at: now,
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: false,
        }
    }

    fn approval_input(chat_id: &str, request: &str, command: &str, minute: i64) -> PendingInput {
        let mut approval = ApprovalRequest::new(ApprovalKind::Command, format!("Run {command}"));
        approval.command = Some(command.into());
        approval.cwd = Some(WS.into());
        PendingInput {
            chat_id: chat_id.into(),
            request_id: request.into(),
            questions: vec![approval.into_question("q1")],
            since: at(minute),
        }
    }

    fn question_input(chat_id: &str, request: &str, minute: i64) -> PendingInput {
        PendingInput {
            chat_id: chat_id.into(),
            request_id: request.into(),
            questions: vec![UserInputQuestion {
                id: "q1".into(),
                header: "Pick".into(),
                question: "Which database?".into(),
                options: vec!["Postgres".into(), "SQLite".into()],
                multi_select: false,
                prefill: None,
                multiline: false,
                approval: None,
            }],
            since: at(minute),
        }
    }

    fn run(id: &str, chat_id: Option<&str>, status: AgentRunStatus, minute: i64) -> AgentRun {
        AgentRun {
            id: id.into(),
            agent_id: "agent-1".into(),
            agent_name: "Nightly".into(),
            trigger: AgentTriggerKind::Manual,
            scheduled_at: at(minute),
            started_at: Some(at(minute)),
            finished_at: Some(at(minute)),
            status,
            summary: Some(format!("{id} summary")),
            chat_id: chat_id.map(str::to_owned),
            collapsed: 0,
            read: false,
            archived: false,
            created_at: at(minute),
        }
    }

    struct Fixture {
        chats: Vec<Chat>,
        sessions: Vec<Session>,
        pending: Vec<PendingInput>,
        runs: Vec<AgentRun>,
        timed_out: Vec<TimedOutQuestion>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                chats: Vec::new(),
                sessions: Vec::new(),
                pending: Vec::new(),
                runs: Vec::new(),
                timed_out: Vec::new(),
            }
        }

        fn derive(&self, now: DateTime<Utc>) -> Vec<AttentionItem> {
            let spaces = [space()];
            derive(
                &Sources {
                    device_id: DEVICE,
                    chats: &self.chats,
                    spaces: &spaces,
                    sessions: &self.sessions,
                    pending: &self.pending,
                    runs: &self.runs,
                    timed_out: &self.timed_out,
                },
                now,
            )
        }
    }

    fn ids(items: &[AttentionItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn every_state_lands_in_one_ordered_queue() {
        let now = at(30);
        let mut f = Fixture::new();
        for id in [
            "push", "ls", "ask", "broke", "limited", "done", "quiet", "working",
        ] {
            f.chats.push(with_config(chat(id)));
        }
        f.pending
            .push(approval_input("push", "r-push", "git push --force", 20));
        f.pending.push(approval_input("ls", "r-ls", "ls -la", 10));
        f.pending.push(question_input("ask", "r-ask", 25));
        f.sessions
            .push(session("broke", SessionStatus::Errored, now));
        f.sessions
            .push(session("working", SessionStatus::Working, now));
        f.chats
            .iter_mut()
            .find(|c| c.id == "limited")
            .unwrap()
            .limit = Some(ChatLimit {
            limited_at: at(5),
            reset_at: None,
            reset_source: None,
            window: Some("5-hour".into()),
            recovery: LimitRecovery::Ask,
            login: None,
        });
        let quiet = f.chats.iter_mut().find(|c| c.id == "quiet").unwrap();
        quiet.last_seen_at = Some(at(2));

        let items = f.derive(now);
        assert_eq!(
            ids(&items),
            [
                "approval:push:r-push",
                "question:ask:r-ask",
                "approval:ls:r-ls",
                "chat:broke",
                "chat:limited",
                "chat:done",
            ]
        );
        let push = &items[0];
        assert_eq!(push.risk, Some(ApprovalRisk::High));
        assert_eq!(push.summary, "git push --force");
        assert_eq!(push.created_at, at(20));
        assert_eq!(push.harness, Some(HarnessId::Codex));
        assert_eq!(push.project.as_deref(), Some("app"));
        assert_eq!(push.title, "Chat push");
        assert_eq!(
            push.actions,
            [
                AttentionAction::Approve,
                AttentionAction::Decline,
                AttentionAction::Open
            ]
        );
        assert!(push.approval.as_ref().is_some_and(|a| a.command.is_some()));
        assert!(items[2].batch_approvable(), "ls is low risk");
        assert!(!push.batch_approvable());
        assert_eq!(items[1].summary, "Which database?");
        assert_eq!(items[1].actions, [AttentionAction::Open]);
        assert_eq!(items[3].summary, "broke says done");
        assert_eq!(items[4].summary, "Hit the 5-hour usage limit");
        assert_eq!(items[4].actions, [AttentionAction::Open]);
    }

    #[test]
    fn other_devices_archived_snoozed_and_child_chats_stay_out_of_the_passive_queue() {
        let now = at(30);
        let mut f = Fixture::new();
        let mut remote = chat("remote");
        remote.device_id = "dev-b".into();
        let mut archived = chat("archived");
        archived.archived = true;
        let mut snoozed = chat("snoozed");
        snoozed.lifecycle = ChatLifecycle::new(LifecycleState::Snoozed { until: at(60) }, at(3));
        let mut child = chat("child");
        child.parent_chat_id = Some("parent".into());
        f.chats = vec![remote, archived, snoozed, child.clone()];
        assert!(f.derive(now).is_empty());

        // A child chat waiting on the user still needs them.
        f.pending
            .push(approval_input("child", "r1", "cargo test", 4));
        f.pending
            .push(approval_input("remote", "r2", "cargo test", 4));
        assert_eq!(ids(&f.derive(now)), ["approval:child:r1"]);
        assert_eq!(f.derive(now)[0].risk, Some(ApprovalRisk::Medium));
    }

    #[test]
    fn agent_runs_fold_into_their_chats_and_reading_either_clears_them() {
        let now = at(30);
        let mut f = Fixture::new();
        f.chats.push(chat("run-chat"));
        f.chats.push(chat("manual"));
        f.runs
            .push(run("run-1", Some("run-chat"), AgentRunStatus::Succeeded, 6));
        f.runs.push(run("run-0", None, AgentRunStatus::Failed, 4));
        f.runs
            .push(run("skipped", None, AgentRunStatus::Skipped, 3));
        let items = f.derive(now);
        assert_eq!(ids(&items), ["run:run-0", "chat:manual", "run:run-1"]);
        let folded = &items[2];
        assert_eq!(folded.kind, AttentionKind::Finished);
        assert_eq!(folded.chat_id.as_deref(), Some("run-chat"));
        assert_eq!(folded.summary, "run-1 summary");
        assert_eq!(folded.agent_run.as_ref().unwrap().agent_name, "Nightly");
        assert_eq!(items[0].kind, AttentionKind::Failed);
        assert_eq!(items[0].title, "Nightly");
        assert_eq!(items[0].actions, [AttentionAction::Dismiss]);

        // Opening the run's chat after it finished reads it.
        f.chats[0] = seen(chat("run-chat"), 7);
        assert_eq!(ids(&f.derive(now)), ["run:run-0", "chat:manual"]);

        // A read run falls back to the chat's own unseen state.
        f.chats[0] = chat("run-chat");
        f.runs[0].read = true;
        assert!(ids(&f.derive(now)).contains(&"chat:run-chat"));
    }

    #[test]
    fn a_waiting_agent_run_shows_once_as_its_approval_with_the_agent_named() {
        let now = at(30);
        let mut f = Fixture::new();
        f.chats.push(chat("run-chat"));
        f.runs.push(run(
            "run-1",
            Some("run-chat"),
            AgentRunStatus::NeedsApproval,
            6,
        ));
        f.pending
            .push(approval_input("run-chat", "r1", "rm -rf build", 7));
        let items = f.derive(now);
        assert_eq!(ids(&items), ["approval:run-chat:r1"]);
        assert_eq!(items[0].agent_run.as_ref().unwrap().run_id, "run-1");
    }

    #[test]
    fn timed_out_questions_stay_until_the_chat_is_seen() {
        let now = at(30);
        let mut f = Fixture::new();
        f.chats.push(seen(chat("ask"), 5));
        f.timed_out.push(TimedOutQuestion {
            chat_id: "ask".into(),
            request_id: "r1".into(),
            question: "Which database?".into(),
            minutes: 10,
            timed_out_at: at(20),
        });
        let items = f.derive(now);
        assert_eq!(ids(&items), ["timedOut:ask:r1"]);
        assert_eq!(items[0].summary, "No answer after 10 min: Which database?");
        f.chats[0] = seen(chat("ask"), 21);
        assert!(f.derive(now).is_empty());
    }

    #[test]
    fn open_inputs_keep_only_unresolved_requests() {
        let input = |request: &str, resolved: bool| MessagePart::Input {
            id: format!("p-{request}"),
            request_id: request.into(),
            questions: vec![],
            resolved,
        };
        let entry = SessionMessageEntry {
            id: "a".into(),
            role: zeron_doc::MessageRole::Assistant,
            parts: vec![input("r1", false), input("r2", false), input("r1", true)],
            created_at: 0,
            device_id: DEVICE.into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        };
        let open = open_inputs(&[entry]);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].0, "r2");
    }

    #[test]
    fn summaries_are_one_short_line() {
        assert_eq!(one_line("\n  first line\nsecond"), "first line");
        let long = "x".repeat(400);
        assert_eq!(one_line(&long).chars().count(), SUMMARY_CHARS);
        let mut many = question_input("c", "r", 0).questions;
        many.push(many[0].clone());
        assert_eq!(question_summary(&many), "Which database? (+1 more)");
    }
}
