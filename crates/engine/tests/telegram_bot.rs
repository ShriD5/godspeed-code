//! The Telegram bot against a real engine and a local mock of the Bot API
//! (never the real Telegram): linking, threads (plain text, reply-to, /new,
//! queueing, /stop), provider/model/access pickers, /schedule variants and
//! their buttons, scheduled-run notifications, approval and question cards
//! answered through the command ledger, 4096-character chunking, and the
//! token never reaching logs or RPC replies.

#[path = "support/telegram_mock.rs"]
mod telegram_mock;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use telegram_mock::MockBotApi;
use zeron_doc::{MessageRole, SessionCommandPayload};
use zeron_engine::agents::Clock;
use zeron_engine::telegram::{Telegram, TelegramDeps, TelegramOptions};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::agents::{AgentDraft, AgentTrigger};
use zeron_proto::telegram::{TelegramConnection, TelegramStatus};
use zeron_proto::{
    AccessMode, AgentEvent, ApprovalKind, ApprovalRequest, DoneStatus, HarnessId, Model,
    ReasoningLevel, RunRequest, SteeringMode, UserInputQuestion,
};
use zeron_rpc::methods;

const TOKEN: &str = "123456789:AAH-mock_SECRET_value_0123456789abc";
const SECRET: &str = "AAH-mock_SECRET_value_0123456789abc";
const USER: i64 = 7001;
const SPACE: &str = "space-telegram";

type RequestLog = Arc<Mutex<Vec<RunRequest>>>;

/// A stand-in agent. Prompts steer it: `APPROVE` asks to run a command,
/// `ASK` asks a question, `SLOW` runs until stopped, `PAUSE` replies after
/// a moment, `LONG` replies with 9000 characters, `FAIL`/`LIMIT` fail; any
/// other prompt is echoed (agent runs reply with a report).
struct ScriptedHarness {
    id: HarnessId,
    name: &'static str,
    models: Vec<String>,
    requests: RequestLog,
}

fn long_reply() -> String {
    format!("{}\n", "x".repeat(99)).repeat(90)
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        self.name
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(self
            .models
            .iter()
            .map(|id| Model {
                id: id.clone(),
                label: id.to_uppercase(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            })
            .collect())
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests.lock().unwrap().push(request.clone());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let prompt = request.prompt.clone();
        let harness = self.id;
        tokio::spawn(async move {
            let send = |event: AgentEvent| {
                let _ = tx.send(event);
            };
            let done = |status: DoneStatus, error: Option<&str>| AgentEvent::Done {
                status,
                result: None,
                error: error.map(str::to_owned),
                session_id: Some("sess".into()),
            };
            send(AgentEvent::SessionStarted {
                harness,
                model: "m".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess".into(),
                assistant_message_id: "a-1".into(),
            });
            let text = |t: String| AgentEvent::TextDelta { text: t };
            if prompt.contains("APPROVE") {
                let mut approval = ApprovalRequest::new(ApprovalKind::Command, "Clean the build");
                approval.command = Some("rm -rf build/ && make <all>".into());
                approval.cwd = Some("/project".into());
                let decision =
                    zeron_harness::approval::ask(controls.request_input.as_ref(), approval).await;
                send(text(format!("Decision: {}.", decision.wire_name())));
                send(done(DoneStatus::Completed, None));
            } else if prompt.contains("ASK") {
                let answers = (controls.request_input)(vec![UserInputQuestion {
                    id: "q-db".into(),
                    header: "Database".into(),
                    question: "Which database?".into(),
                    options: vec!["Postgres".into(), "SQLite".into()],
                    prefill: None,
                    multiline: false,
                    multi_select: false,
                    approval: None,
                }])
                .await
                .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_default();
                send(text(format!("You picked {picked}.")));
                send(done(DoneStatus::Completed, None));
            } else if prompt.contains("SLOW") {
                send(text("Working on it.".into()));
                tokio::select! {
                    _ = controls.interrupt.cancelled() => {}
                    _ = controls.turn.stop_requested() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                send(done(DoneStatus::Interrupted, None));
            } else if prompt.contains("PAUSE") {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                send(text("slow done".into()));
                send(done(DoneStatus::Completed, None));
            } else if prompt.contains("LONG") {
                send(text(long_reply()));
                send(done(DoneStatus::Completed, None));
            } else if prompt.contains("FAIL") {
                send(AgentEvent::Error {
                    message: "boom: the build is broken".into(),
                });
                send(done(DoneStatus::Errored, Some("boom: the build is broken")));
            } else if prompt.contains("LIMIT") {
                send(AgentEvent::UsageLimit {
                    reset_at: Some(Utc::now() + TimeDelta::hours(2)),
                    window: None,
                });
                send(AgentEvent::Error {
                    message: "You've hit your usage limit".into(),
                });
                send(done(
                    DoneStatus::Errored,
                    Some("You've hit your usage limit"),
                ));
            } else if prompt.contains("## Instructions") {
                send(text("Report: all green.".into()));
                send(done(DoneStatus::Completed, None));
            } else {
                send(text(format!("echo: {prompt}")));
                send(done(DoneStatus::Completed, None));
            }
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        });
        Ok(stream.boxed())
    }
}

/// The wall clock plus an adjustable offset (link-code expiry).
#[derive(Default)]
struct OffsetClock(Mutex<TimeDelta>);

impl Clock for OffsetClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now() + *self.0.lock().unwrap()
    }
}

struct World {
    tmp: tempfile::TempDir,
    core: EngineCore,
    client: zeron_rpc::RpcClient,
    mock: MockBotApi,
    requests: RequestLog,
}

async fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let requests = RequestLog::default();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::ClaudeCode,
        name: "Claude Code",
        models: vec!["sonnet".into(), "opus".into()],
        requests: requests.clone(),
    }));
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::Codex,
        name: "Codex",
        models: (0..10).map(|i| format!("gpt-{i}")).collect(),
        requests: requests.clone(),
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::ClaudeCode,
        None,
    )
    .expect("engine core assembles");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    core.workspace
        .create_space(
            SPACE,
            &core.device_id,
            &project.to_string_lossy(),
            None,
            false,
        )
        .unwrap();
    let mock = MockBotApi::start(TOKEN).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    World {
        tmp,
        core,
        client,
        mock,
        requests,
    }
}

async fn status(client: &zeron_rpc::RpcClient) -> TelegramStatus {
    serde_json::from_value(client.call(methods::GET_TELEGRAM, json!({})).await.unwrap()).unwrap()
}

async fn eventually<F: FnMut() -> bool>(mut done: F, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_connected(client: &zeron_rpc::RpcClient) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if matches!(
            status(client).await.connection,
            TelegramConnection::Connected { .. }
        ) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "bot never connected"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The engine's own bot, pointed at the mock and connected.
async fn connected_world() -> World {
    let w = world().await;
    w.core.telegram.set_api_base(&w.mock.base);
    w.client
        .call(methods::SET_TELEGRAM_TOKEN, json!({ "token": TOKEN }))
        .await
        .expect("SetTelegramToken");
    wait_connected(&w.client).await;
    w
}

async fn link(w: &World, chat: i64) {
    let code: Value = w
        .client
        .call(methods::CREATE_TELEGRAM_LINK_CODE, json!({}))
        .await
        .unwrap();
    w.mock
        .send(chat, &format!("/link {}", code["code"].as_str().unwrap()));
    w.mock.wait_for_text(chat, "Linked").await;
}

async fn linked_world() -> World {
    let w = connected_world().await;
    link(&w, USER).await;
    w
}

fn buttons(body: &Value) -> Vec<(String, String)> {
    body["reply_markup"]["inline_keyboard"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row.as_array().cloned().unwrap_or_default())
        .map(|b| {
            (
                b["text"].as_str().unwrap_or_default().to_string(),
                b["callback_data"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn button(body: &Value, label: &str) -> String {
    buttons(body)
        .into_iter()
        .find(|(text, _)| text.trim_start_matches("• ") == label)
        .unwrap_or_else(|| panic!("no {label:?} button in {body}"))
        .1
}

fn chats(w: &World) -> Vec<zeron_proto::Chat> {
    w.core.workspace.read_chats().unwrap()
}

fn user_texts(w: &World, chat_id: &str) -> Vec<String> {
    w.core
        .doc_host
        .open(chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.role == MessageRole::User)
        .flat_map(|e| e.parts)
        .filter_map(|p| match p {
            zeron_doc::MessagePart::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn link_codes_link_once_and_unlinked_chats_are_refused() {
    let w = world().await;
    let clock = Arc::new(OffsetClock::default());
    let telegram = Telegram::open_with(
        &w.tmp.path().join("bot"),
        TelegramDeps {
            workspace: w.core.workspace.clone(),
            doc_host: w.core.doc_host.clone(),
            sessions: w.core.sessions.clone(),
            registry: w.core.registry.clone(),
            agents: w.core.agents.clone(),
        },
        TelegramOptions {
            api_base: w.mock.base.clone(),
            poll_timeout: Duration::from_secs(1),
            clock: clock.clone(),
        },
    )
    .unwrap();
    telegram.start();
    telegram.set_token(TOKEN).unwrap();

    w.mock.send(USER, "hello?");
    w.mock
        .wait_for_text(USER, "only answers chats linked to Godspeed")
        .await;

    w.mock.send(USER, "/link WRONG1");
    w.mock.wait_for_text(USER, "isn't valid").await;

    let expired = telegram.create_link_code();
    assert_eq!(expired.code.len(), 6);
    *clock.0.lock().unwrap() = TimeDelta::minutes(11);
    w.mock.send(USER, &format!("/link {}", expired.code));
    w.mock.wait_for_text(USER, "expired").await;
    *clock.0.lock().unwrap() = TimeDelta::zero();

    let code = telegram.create_link_code();
    w.mock.send_in_group(-500, &format!("/link {}", code.code));
    w.mock.wait_for_text(-500, "private chats").await;
    assert!(telegram.status().unwrap().linked_chats.is_empty());

    w.mock
        .send(USER, &format!("/link {}", code.code.to_lowercase()));
    w.mock.wait_for_text(USER, "Linked").await;
    let linked = telegram.status().unwrap();
    assert_eq!(linked.linked_chats.len(), 1);
    assert_eq!(linked.linked_chats[0].chat_id, USER);
    assert_eq!(linked.linked_chats[0].label, "@shri");
    assert!(linked.link_code.is_none(), "a used code is gone");

    // The code was one-time: another chat can't reuse it.
    w.mock.send(USER + 1, &format!("/link {}", code.code));
    w.mock.wait_for_text(USER + 1, "isn't valid").await;
    w.mock.send(USER + 1, "/status");
    w.mock
        .wait_for_text(USER + 1, "only answers chats linked")
        .await;

    w.mock.send(USER, "/unlink");
    w.mock.wait_for_text(USER, "Unlinked").await;
    assert!(telegram.status().unwrap().linked_chats.is_empty());
    telegram.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn text_starts_a_thread_replies_continue_it_and_new_starts_over() {
    let w = linked_world().await;

    let asked = w.mock.send(USER, "hello there");
    let (first_answer, body) = w.mock.wait_for_text(USER, "echo: hello there").await;
    assert_eq!(body["reply_parameters"]["message_id"], json!(asked));
    w.mock
        .wait_for("sendChatAction", |b| {
            b["chat_id"] == json!(USER) && b["action"] == "typing"
        })
        .await;
    let first = chats(&w);
    assert_eq!(first.len(), 1);
    let first_chat = first[0].id.clone();
    let config = first[0].config.clone().unwrap();
    assert_eq!(config.harness, HarnessId::ClaudeCode);
    assert_eq!(config.access_mode, Some(AccessMode::AutoAcceptEdits));
    assert!(first[0].space_id.is_none(), "no project by default");

    w.mock.send(USER, "second");
    w.mock.wait_for_text(USER, "echo: second").await;
    assert_eq!(chats(&w).len(), 1, "plain text continues the thread");

    w.mock.send(USER, "/new");
    w.mock.wait_for_text(USER, "Fresh thread").await;
    w.mock.send(USER, "third");
    w.mock.wait_for_text(USER, "echo: third").await;
    let all = chats(&w);
    assert_eq!(all.len(), 2, "/new starts another chat");
    let second_chat = all.iter().find(|c| c.id != first_chat).unwrap().id.clone();
    assert_eq!(user_texts(&w, &second_chat), vec!["third".to_string()]);

    w.mock.reply(USER, "back to the first", first_answer);
    w.mock.wait_for_text(USER, "echo: back to the first").await;
    assert_eq!(chats(&w).len(), 2);
    assert_eq!(
        user_texts(&w, &first_chat),
        vec!["hello there", "second", "back to the first"]
    );

    w.mock.send(USER, "/status");
    let (_, body) = w.mock.wait_for_text(USER, "Thread:").await;
    let text = body["text"].as_str().unwrap();
    assert!(
        text.contains("Claude Code · default model · no project · Auto-accept edits"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_model_and_access_choices_persist_and_drive_new_threads() {
    let w = linked_world().await;

    w.mock.send(USER, "/provider");
    let (menu, body) = w.mock.wait_for_text(USER, "Which agent").await;
    let names: Vec<String> = buttons(&body).into_iter().map(|(t, _)| t).collect();
    assert_eq!(names, vec!["• Claude Code", "Codex"]);
    w.mock.tap(USER, menu, &button(&body, "Codex"));
    w.mock
        .wait_for("editMessageText", |b| {
            b["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Agent: Codex"))
        })
        .await;
    w.mock.wait_for("answerCallbackQuery", |_| true).await;
    assert_eq!(
        status(&w.client).await.linked_chats[0].defaults.harness,
        Some(HarnessId::Codex)
    );

    w.mock.send(USER, "/model");
    let (menu, body) = w
        .mock
        .wait_for_text(USER, "Codex model for new threads (page 1 of 2)")
        .await;
    assert!(buttons(&body).iter().any(|(t, _)| t == "GPT-7"));
    assert!(!buttons(&body).iter().any(|(t, _)| t == "GPT-9"));
    w.mock.tap(USER, menu, &button(&body, "Next ›"));
    let page = w
        .mock
        .wait_for("editMessageText", |b| {
            b["text"]
                .as_str()
                .is_some_and(|t| t.contains("page 2 of 2"))
        })
        .await;
    w.mock.tap(USER, menu, &button(&page, "GPT-9"));
    w.mock
        .wait_for("editMessageText", |b| {
            b["text"]
                .as_str()
                .is_some_and(|t| t.contains("Codex model: GPT-9"))
        })
        .await;

    w.mock.send(USER, "/access");
    let (menu, body) = w.mock.wait_for_text(USER, "Access mode for Codex").await;
    w.mock.tap(USER, menu, &button(&body, "Supervised"));
    w.mock
        .wait_for("editMessageText", |b| {
            b["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Access: Supervised"))
        })
        .await;

    let defaults = status(&w.client).await.linked_chats[0].defaults.clone();
    assert_eq!(defaults.model.as_deref(), Some("gpt-9"));
    assert_eq!(defaults.access_mode, Some(AccessMode::Supervised));

    w.mock.send(USER, "hi codex");
    w.mock.wait_for_text(USER, "echo: hi codex").await;
    let request = w.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(request.harness, Some(HarnessId::Codex));
    assert_eq!(request.model.as_deref(), Some("gpt-9"));
    assert_eq!(request.access.map(|a| a.mode), Some(AccessMode::Supervised));
}

fn agent_trigger(w: &World, name_has: &str) -> AgentTrigger {
    w.core
        .agents
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.draft.instructions.contains(name_has))
        .unwrap_or_else(|| panic!("no agent for {name_has}"))
        .draft
        .triggers[0]
        .clone()
}

async fn pick_project(w: &World) {
    w.mock.send(USER, "/project");
    let (menu, body) = w.mock.wait_for_text(USER, "Which project").await;
    w.mock.tap(USER, menu, &format!("p:{SPACE}"));
    assert!(
        buttons(&body)
            .iter()
            .any(|(_, data)| data == &format!("p:{SPACE}"))
    );
    w.mock
        .wait_for("editMessageText", |b| {
            b["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Project: project"))
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn schedule_variants_create_agents_in_the_chat_time_zone() {
    let w = linked_world().await;

    w.mock.send(USER, "/schedule daily 09:00 Morning report");
    w.mock.wait_for_text(USER, "run in a project").await;
    assert!(w.core.agents.list().unwrap().is_empty());

    pick_project(&w).await;
    w.mock.send(USER, "/tz Asia/Kolkata");
    w.mock
        .wait_for_text(USER, "Time zone set to Asia/Kolkata")
        .await;

    let kolkata = |expr: &str| AgentTrigger::Cron {
        expr: expr.into(),
        timezone: "Asia/Kolkata".into(),
    };
    for (command, needle, expected) in [
        (
            "daily 09:00 Morning report",
            "Morning report",
            kolkata("0 9 * * *"),
        ),
        (
            "weekdays 18:30 Wrap up the day",
            "Wrap up",
            kolkata("30 18 * * 1-5"),
        ),
        (
            "weekly fri 08:15 Release notes",
            "Release notes",
            kolkata("15 8 * * 5"),
        ),
        (
            "hourly Check the queue",
            "Check the queue",
            kolkata("0 * * * *"),
        ),
        (
            "every 30m Poll the deploy",
            "Poll the deploy",
            AgentTrigger::Interval { every_secs: 1800 },
        ),
    ] {
        let id = w.core.agents.list().unwrap().len() + 1;
        w.mock.send(USER, &format!("/schedule {command}"));
        w.mock
            .wait_for_text(USER, &format!("Scheduled #{id}"))
            .await;
        assert_eq!(agent_trigger(&w, needle), expected, "{command}");
    }
    let agents = w.core.agents.list().unwrap();
    assert_eq!(agents.len(), 5);
    let morning = agents
        .iter()
        .find(|a| a.draft.instructions == "Morning report")
        .unwrap();
    assert_eq!(morning.draft.space_id, SPACE);
    assert_eq!(morning.draft.harness, HarnessId::ClaudeCode);
    assert_eq!(morning.draft.access_mode, Some(AccessMode::AutoAcceptEdits));
    assert!(morning.next_fire_at.is_some());

    w.mock.send(USER, "/schedule daily 25:00 Nope");
    w.mock
        .wait_for_text(USER, "Times are in Asia/Kolkata")
        .await;
    assert_eq!(w.core.agents.list().unwrap().len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn schedule_buttons_control_the_agent_and_results_come_back() {
    let w = linked_world().await;
    pick_project(&w).await;
    w.mock.send(USER, "/schedule every 30m Report on the build");
    w.mock.wait_for_text(USER, "Scheduled #1").await;
    let agent_id = w.core.agents.list().unwrap()[0].id.clone();
    let enabled = |w: &World| w.core.agents.list().unwrap()[0].draft.enabled;

    w.mock.send(USER, "/schedules");
    let (card, body) = w.mock.wait_for_text(USER, "#1 · every 30m").await;
    let labels: Vec<String> = buttons(&body).into_iter().map(|(t, _)| t).collect();
    assert_eq!(labels, vec!["Pause", "Run now", "Delete"]);

    w.mock.tap(USER, card, &button(&body, "Pause"));
    let paused = w
        .mock
        .wait_for("editMessageText", |b| {
            b["text"].as_str().is_some_and(|t| t.contains("⏸ Paused"))
        })
        .await;
    assert!(!enabled(&w));
    w.mock.tap(USER, card, &button(&paused, "Resume"));
    eventually(|| enabled(&w), "resume").await;

    w.mock.tap(USER, card, &button(&body, "Run now"));
    let (result, body) = w
        .mock
        .wait_for_text(USER, "✅ #1 (every 30m) finished")
        .await;
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .contains("Report: all green.")
    );

    // Replying to a result continues that run's chat.
    let run_chat = w.core.agents.list_runs(&Default::default()).unwrap()[0]
        .chat_id
        .clone()
        .unwrap();
    w.mock.reply(USER, "thanks, now go deeper", result);
    w.mock
        .wait_for_text(USER, "echo: thanks, now go deeper")
        .await;
    assert!(user_texts(&w, &run_chat).contains(&"thanks, now go deeper".to_string()));

    w.mock.send(USER, "/schedule every 2h FAIL on purpose");
    w.mock.wait_for_text(USER, "Scheduled #2").await;
    w.mock.send(USER, "/runnow 2");
    w.mock.wait_for_text(USER, "Running #2 now").await;
    w.mock
        .wait_for_text(USER, "❌ #2 (every 2h) failed: boom: the build is broken")
        .await;

    w.mock.send(USER, "/schedule every 2h LIMIT me");
    w.mock.wait_for_text(USER, "Scheduled #3").await;
    w.mock.send(USER, "/runnow #3");
    w.mock
        .wait_for_text(USER, "⏸ #3 (every 2h) hit a usage limit")
        .await;

    w.mock.send(USER, "/pause 9");
    w.mock.wait_for_text(USER, "No schedule #9 here").await;

    w.mock
        .tap(USER, card, &button(&body_of_card(&w, card).await, "Delete"));
    w.mock
        .wait_for("editMessageText", |b| b["text"] == json!("Deleted #1."))
        .await;
    assert!(
        w.core
            .agents
            .list()
            .unwrap()
            .iter()
            .all(|a| a.id != agent_id)
    );
}

/// The latest keyboard shown on `card` (sent or edited).
async fn body_of_card(w: &World, card: i64) -> Value {
    w.mock
        .calls()
        .into_iter()
        .rev()
        .find(|c| {
            (c.method == "editMessageText" && c.body["message_id"] == json!(card))
                || (c.method == "sendMessage" && c.body["__message_id"] == json!(card))
        })
        .map(|c| c.body)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_show_the_exact_command_and_buttons_answer_through_the_ledger() {
    let w = linked_world().await;

    w.mock.send(USER, "APPROVE the cleanup");
    let (card, body) = w.mock.wait_for_text(USER, "Approval needed").await;
    let text = body["text"].as_str().unwrap();
    assert_eq!(body["parse_mode"], "HTML");
    assert!(
        text.contains("<pre>rm -rf build/ &amp;&amp; make &lt;all&gt;</pre>"),
        "{text}"
    );
    assert!(text.contains("in <code>/project</code>"), "{text}");
    let labels: Vec<String> = buttons(&body).into_iter().map(|(t, _)| t).collect();
    assert_eq!(labels, vec!["Allow once", "Allow for session", "Deny"]);

    w.mock.tap(USER, card, &button(&body, "Allow once"));
    w.mock
        .wait_for("editMessageText", |b| {
            b["message_id"] == json!(card)
                && b["text"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("✅ Allow once"))
                && b["reply_markup"]["inline_keyboard"] == json!([])
        })
        .await;
    w.mock.wait_for_text(USER, "Decision: approve.").await;
    let chat_id = chats(&w)[0].id.clone();
    let commands = w
        .core
        .doc_host
        .open(&chat_id)
        .unwrap()
        .doc()
        .read_commands()
        .unwrap();
    assert!(commands.iter().any(|c| matches!(
        &c.payload,
        SessionCommandPayload::RespondInput { answers, .. } if answers[0].labels == ["Approve"]
    )));

    // A question: options as buttons.
    w.mock.send(USER, "ASK me");
    let (card, body) = w.mock.wait_for_text(USER, "Which database?").await;
    w.mock.tap(USER, card, &button(&body, "SQLite"));
    w.mock.wait_for_text(USER, "You picked SQLite.").await;

    // A question answered by replying to its card in free text.
    w.mock.send(USER, "ASK again");
    let cards = || {
        w.mock
            .calls_to("sendMessage")
            .into_iter()
            .filter(|b| {
                b["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Which database?"))
            })
            .collect::<Vec<_>>()
    };
    eventually(|| cards().len() == 2, "second question card").await;
    let card = cards()[1]["__message_id"].as_i64().unwrap();
    w.mock.reply(USER, "DuckDB", card);
    w.mock.wait_for_text(USER, "You picked DuckDB.").await;

    // An agent made in the app: its approval reaches every linked chat and
    // waits there (no timer) until answered.
    let agent = w
        .core
        .agents
        .create(AgentDraft {
            name: "Cleaner".into(),
            harness: HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            space_id: SPACE.into(),
            instructions: "APPROVE the nightly cleanup".into(),
            enabled: true,
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            access_mode: None,
            triggers: vec![],
        })
        .unwrap();
    let run = w.core.agents.run_now(&agent.id).await.unwrap();
    let before = w.mock.calls_to("sendMessage").len();
    eventually(
        || {
            w.mock.calls_to("sendMessage")[before..]
                .iter()
                .any(|b| b["text"].as_str().is_some_and(|t| t.contains("Cleaner")))
        },
        "agent approval card",
    )
    .await;
    let card_body = w.mock.calls_to("sendMessage")[before..]
        .iter()
        .find(|b| b["text"].as_str().is_some_and(|t| t.contains("Cleaner")))
        .cloned()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        w.core
            .agents
            .list_runs(&Default::default())
            .unwrap()
            .iter()
            .any(|r| r.id == run.id
                && r.status == zeron_proto::agents::AgentRunStatus::NeedsApproval)
    );
    w.mock.tap(
        USER,
        card_body["__message_id"].as_i64().unwrap(),
        &button(&card_body, "Deny"),
    );
    eventually(
        || {
            w.core
                .agents
                .list_runs(&Default::default())
                .unwrap()
                .iter()
                .any(|r| r.id == run.id && r.summary.as_deref() == Some("Decision: decline."))
        },
        "denied agent run",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bursts_of_agent_approvals_fold_into_one_digest_answered_in_place() {
    let w = linked_world().await;
    let mut runs = Vec::new();
    for n in 1..=6 {
        let agent = w
            .core
            .agents
            .create(AgentDraft {
                name: format!("Cleaner {n}"),
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                space_id: SPACE.into(),
                instructions: "APPROVE the nightly cleanup".into(),
                enabled: true,
                sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
                auto_approve: false,
                access_mode: None,
                triggers: vec![],
            })
            .unwrap();
        runs.push(w.core.agents.run_now(&agent.id).await.unwrap());
    }
    let is_card = |b: &Value| {
        b["text"]
            .as_str()
            .is_some_and(|t| t.contains("Approval needed"))
    };
    let is_digest = |b: &Value| b["text"].as_str().is_some_and(|t| t.contains("need you"));
    let lines = |b: &Value| {
        b["text"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("Run command"))
            .count()
    };
    eventually(
        || {
            let sent = w.mock.calls_to("sendMessage");
            let cards = sent.iter().filter(|b| is_card(b)).count();
            let digested: usize = sent.iter().filter(|b| is_digest(b)).map(lines).sum();
            cards + digested == 6
        },
        "every approval as a card or a digest line",
    )
    .await;
    let sent = w.mock.calls_to("sendMessage");
    assert!(
        sent.iter().filter(|b| is_card(b)).count() <= 3,
        "no more than three cards in a minute"
    );
    let digest = sent
        .iter()
        .find(|b| is_digest(b) && lines(b) >= 2)
        .cloned()
        .expect("a digest of the burst");
    let text = digest["text"].as_str().unwrap();
    assert!(
        text.contains("<code>rm -rf build/ &amp;&amp; make &lt;all&gt;</code>"),
        "{text}"
    );
    let labels: Vec<String> = buttons(&digest).into_iter().map(|(t, _)| t).collect();
    assert!(labels.contains(&"✅ 1".to_string()), "{labels:?}");
    assert!(labels.contains(&"❌ 1".to_string()), "{labels:?}");

    // Allowing one item answers it through the ledger and re-renders the
    // digest with its outcome; the rest keep their buttons.
    let digest_id = digest["__message_id"].as_i64().unwrap();
    let waiting_before = lines(&digest);
    w.mock.tap(USER, digest_id, &button(&digest, "✅ 1"));
    let edited = w
        .mock
        .wait_for("editMessageText", |b| {
            b["message_id"] == json!(digest_id)
                && b["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("✅ Allow once"))
        })
        .await;
    let edited_labels: Vec<String> = buttons(&edited).into_iter().map(|(t, _)| t).collect();
    assert!(
        !edited_labels.contains(&"✅ 1".to_string()),
        "{edited_labels:?}"
    );
    assert_eq!(edited_labels.len(), (waiting_before - 1) * 2);
    eventually(
        || {
            w.core
                .agents
                .list_runs(&Default::default())
                .unwrap()
                .iter()
                .any(|r| r.summary.as_deref() == Some("Decision: approve."))
        },
        "the approved run to finish",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_queue_behind_a_running_turn_and_stop_interrupts_it() {
    let w = linked_world().await;

    w.mock.send(USER, "PAUSE then answer");
    eventually(|| chats(&w).len() == 1, "thread").await;
    let chat_id = chats(&w)[0].id.clone();
    eventually(|| w.core.sessions.turn_in_flight(&chat_id), "turn running").await;
    w.mock.send(USER, "and then this");
    w.mock.wait_for_text(USER, "Queued").await;
    w.mock.wait_for_text(USER, "echo: and then this").await;
    let texts = w.mock.texts_to(USER);
    let slow = texts.iter().position(|t| t == "slow done").unwrap();
    let queued = texts
        .iter()
        .position(|t| t == "echo: and then this")
        .unwrap();
    assert!(slow < queued, "{texts:?}");

    w.mock.send(USER, "/stop");
    w.mock.wait_for_text(USER, "Nothing is running").await;

    w.mock.send(USER, "SLOW job");
    eventually(|| w.core.sessions.turn_in_flight(&chat_id), "slow turn").await;
    w.mock.send(USER, "/stop");
    w.mock.wait_for_text(USER, "Stopping").await;
    let (_, body) = w.mock.wait_for_text(USER, "Working on it.").await;
    assert!(
        body["text"].as_str().unwrap().contains("⏹ Stopped."),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn long_replies_are_split_at_4096_characters() {
    let w = linked_world().await;
    let asked = w.mock.send(USER, "LONG please");
    eventually(
        || {
            w.mock
                .texts_to(USER)
                .iter()
                .filter(|t| t.starts_with("xxx"))
                .count()
                >= 3
        },
        "three chunks",
    )
    .await;
    let parts: Vec<Value> = w
        .mock
        .calls_to("sendMessage")
        .into_iter()
        .filter(|b| b["text"].as_str().is_some_and(|t| t.starts_with("xxx")))
        .collect();
    assert_eq!(parts.len(), 3);
    let texts: Vec<&str> = parts.iter().map(|b| b["text"].as_str().unwrap()).collect();
    assert!(texts.iter().all(|t| t.encode_utf16().count() <= 4096));
    assert_eq!(texts.concat(), long_reply().trim());
    assert_eq!(parts[0]["reply_parameters"]["message_id"], json!(asked));
    assert!(parts[1].get("reply_parameters").is_none());
}

/// Every log line this test binary emits from here on.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = buffer.clone();
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_writer(move || Sink(writer.clone()))
            .try_init();
        buffer
    })
    .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_token_never_reaches_logs_or_rpc_replies() {
    let logs = captured_logs();
    let w = world().await;
    let watch = w
        .client
        .subscribe(methods::WATCH_TELEGRAM, json!({}))
        .await
        .unwrap();

    // A dead endpoint first: transport errors name the request URL, which
    // carries the token.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_base = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    w.core.telegram.set_api_base(&dead_base);
    let reply = w
        .client
        .call(methods::SET_TELEGRAM_TOKEN, json!({ "token": TOKEN }))
        .await
        .unwrap();
    assert_eq!(reply["tokenSet"], true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let error = loop {
        if let TelegramConnection::Error { message } = status(&w.client).await.connection {
            break message;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no connection error"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(error.contains("couldn't reach Telegram"), "{error}");

    let bad = w
        .client
        .call(
            methods::SET_TELEGRAM_TOKEN,
            json!({ "token": "not a token" }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(bad.contains("doesn't look like a bot token"), "{bad}");

    w.core.telegram.set_api_base(&w.mock.base);
    wait_connected(&w.client).await;
    link(&w, USER).await;
    w.mock.send(USER, "hello");
    w.mock.wait_for_text(USER, "echo: hello").await;

    let token_file = w.tmp.path().join("data/telegram/bot-token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let mut replies = vec![
        w.client
            .call(methods::GET_TELEGRAM, json!({}))
            .await
            .unwrap(),
        w.client
            .call(methods::CREATE_TELEGRAM_LINK_CODE, json!({}))
            .await
            .unwrap(),
    ];
    let cleared = w
        .client
        .call(methods::CLEAR_TELEGRAM_TOKEN, json!({}))
        .await
        .unwrap();
    assert_eq!(cleared["tokenSet"], false);
    assert_eq!(cleared["connection"]["state"], "off");
    assert!(!token_file.exists());
    replies.push(cleared);
    let mut watch = watch;
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(200), watch.recv()).await
    {
        replies.push(item);
    }
    assert!(replies.len() > 4, "the watch streamed status");
    for reply in &replies {
        let text = reply.to_string();
        assert!(!text.contains(SECRET), "token in an RPC reply: {text}");
    }

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).to_string();
    assert!(logs.contains("telegram: connected"), "logs were captured");
    assert!(
        logs.contains("telegram: poller stopped"),
        "the dead endpoint was logged"
    );
    assert!(!logs.contains(SECRET), "token in the logs");
}
