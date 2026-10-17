//! The "Needs you" inbox against a real engine: approvals, questions,
//! failures and finished chats land in one ordered `WatchAttention` stream
//! that updates live; seen-marking drops items; a batch approve answers only
//! low-risk approvals; a question timeout answers the agent and parks the
//! question, while approvals never time out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;

use zeron_doc::SessionCommandPayload;
use zeron_engine::agents::Clock;
use zeron_engine::attention::{AttentionDeps, AttentionService};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::attention::{
    AttentionKind, AttentionSnapshot, BatchApproval, QUESTION_TIMEOUT_ANSWER,
    QuestionTimeoutSettings,
};
use zeron_proto::{
    AgentEvent, ApprovalKind, ApprovalRequest, ApprovalRisk, ChatConfig, DoneStatus, HarnessId,
    Model, ReasoningLevel, RunRequest, SandboxLevel, SteeringMode, UserInputQuestion,
};
use zeron_rpc::methods;

const SPACE: &str = "space-inbox";

type Replies = Arc<Mutex<Vec<String>>>;

/// By prompt: `APPROVE <command>` waits on a command approval, `ASK` on a
/// question, `FAIL` errors out, anything else replies and finishes. What
/// the user answered is recorded in `replies`.
struct ScriptedHarness {
    replies: Replies,
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Scripted"
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
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: "sess".into(),
            assistant_message_id: "a-1".into(),
        };
        let done = |status: DoneStatus| AgentEvent::Done {
            status,
            result: None,
            error: None,
            session_id: Some("sess".into()),
        };
        let prompt = request.prompt.clone();
        if prompt == "FAIL" {
            let events = vec![
                Ok(started),
                Ok(AgentEvent::Error {
                    message: "boom".into(),
                }),
                Ok(done(DoneStatus::Errored)),
            ];
            return Ok(futures::stream::iter(events).boxed());
        }
        let waits = prompt.starts_with("APPROVE ") || prompt == "ASK";
        if !waits {
            let events = vec![
                Ok(started),
                Ok(AgentEvent::TextDelta {
                    text: "All done.".into(),
                }),
                Ok(done(DoneStatus::Completed)),
            ];
            return Ok(futures::stream::iter(events).boxed());
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let request_input = controls.request_input;
        let replies = self.replies.clone();
        let cwd = request.cwd.clone();
        tokio::spawn(async move {
            let _ = tx.send(started);
            let reply = match prompt.strip_prefix("APPROVE ") {
                Some(command) => {
                    let mut approval = ApprovalRequest::new(ApprovalKind::Command, "Run it");
                    approval.command = Some(command.into());
                    approval.cwd = Some(cwd);
                    let decision =
                        zeron_harness::approval::ask(request_input.as_ref(), approval).await;
                    format!("{command}: {}", decision.wire_name())
                }
                None => {
                    let answers = request_input(vec![UserInputQuestion {
                        id: "q1".into(),
                        header: "Pick".into(),
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
                    format!("ASK: {picked}")
                }
            };
            replies.lock().unwrap().push(reply);
            let _ = tx.send(done(DoneStatus::Completed));
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        });
        Ok(stream.boxed())
    }
}

struct World {
    _tmp: tempfile::TempDir,
    core: EngineCore,
    client: zeron_rpc::RpcClient,
    replies: Replies,
    workspace_dir: String,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let replies = Replies::default();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ScriptedHarness {
        replies: replies.clone(),
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let workspace_dir = tmp.path().join("project");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let workspace_dir = workspace_dir.to_string_lossy().to_string();
    core.workspace
        .create_space(SPACE, &core.device_id, &workspace_dir, None, false)
        .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    World {
        _tmp: tmp,
        core,
        client,
        replies,
        workspace_dir,
    }
}

impl World {
    fn start(&self, chat_id: &str, prompt: &str) {
        let config = ChatConfig {
            harness: HarnessId::Mock,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            access_mode: None,
        };
        self.core
            .workspace
            .create_chat(chat_id, Some(SPACE), None, Some(config), None)
            .unwrap();
        self.core
            .doc_host
            .queue_command(
                chat_id,
                SessionCommandPayload::Run {
                    request: RunRequest {
                        mcp: None,
                        prompt: prompt.into(),
                        harness: Some(HarnessId::Mock),
                        model: None,
                        reasoning: None,
                        model_options: Default::default(),
                        cwd: self.workspace_dir.clone(),
                        sandbox: SandboxLevel::WorkspaceWrite,
                        auto_approve: false,
                        attachments: Vec::new(),
                        worktree: None,
                        resume: None,
                        access: None,
                    },
                    message_id: format!("m-{chat_id}"),
                },
            )
            .unwrap();
    }

    fn replies(&self) -> Vec<String> {
        self.replies.lock().unwrap().clone()
    }

    async fn attention(&self) -> AttentionSnapshot {
        serde_json::from_value(
            self.client
                .call(methods::GET_ATTENTION, json!({}))
                .await
                .expect("GetAttention"),
        )
        .unwrap()
    }
}

/// Next stream items until `done` holds for one.
async fn watch_until(
    stream: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    mut done: impl FnMut(&AttentionSnapshot) -> bool,
    what: &str,
) -> AttentionSnapshot {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let item = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("WatchAttention stream open");
        let snapshot: AttentionSnapshot = serde_json::from_value(item).unwrap();
        if done(&snapshot) {
            return snapshot;
        }
    }
}

async fn wait_for(mut done: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn kinds(snapshot: &AttentionSnapshot) -> Vec<(String, AttentionKind)> {
    snapshot
        .items
        .iter()
        .map(|item| (item.chat_id.clone().unwrap_or_default(), item.kind))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_ordered_live_queue_with_seen_marking_and_answers() {
    let w = world();
    let mut stream = w
        .client
        .subscribe(methods::WATCH_ATTENTION, json!({}))
        .await
        .expect("WatchAttention");
    let first = watch_until(&mut stream, |_| true, "the first snapshot").await;
    assert!(first.items.is_empty());
    assert_eq!(first.device_id, w.core.device_id);

    w.start("low", "APPROVE ls -la");
    w.start("high", "APPROVE git push --force");
    w.start("ask", "ASK");
    w.start("broke", "FAIL");
    w.start("done", "hello");

    let snapshot = watch_until(&mut stream, |s| s.items.len() == 5, "five items").await;
    assert_eq!(
        kinds(&snapshot),
        [
            ("high".to_string(), AttentionKind::Approval),
            ("ask".to_string(), AttentionKind::Question),
            ("low".to_string(), AttentionKind::Approval),
            ("broke".to_string(), AttentionKind::Failed),
            ("done".to_string(), AttentionKind::Finished),
        ]
    );
    let high = &snapshot.items[0];
    assert_eq!(high.risk, Some(ApprovalRisk::High));
    assert_eq!(high.summary, "git push --force");
    assert_eq!(high.harness, Some(HarnessId::Mock));
    assert_eq!(high.project.as_deref(), Some("project"));
    assert_eq!(
        high.approval.as_ref().and_then(|a| a.command.as_deref()),
        Some("git push --force")
    );
    assert_eq!(snapshot.items[2].risk, Some(ApprovalRisk::Low));
    assert_eq!(snapshot.items[1].summary, "Which database?");

    // A batch approve touches only the low-risk approval, whatever it's handed.
    let all: Vec<String> = snapshot.items.iter().map(|i| i.id.clone()).collect();
    let batch: BatchApproval = serde_json::from_value(
        w.client
            .call(methods::APPROVE_LOW_RISK, json!({ "itemIds": all }))
            .await
            .expect("ApproveLowRisk"),
    )
    .unwrap();
    assert_eq!(batch.approved, [snapshot.items[2].id.clone()]);
    assert_eq!(batch.skipped.len(), 4);
    wait_for(
        || w.replies().contains(&"ls -la: approve".to_string()),
        "the low-risk approval to reach the agent",
    )
    .await;
    let snapshot = watch_until(
        &mut stream,
        |s| {
            !s.items
                .iter()
                .any(|i| i.kind == AttentionKind::Approval && i.risk == Some(ApprovalRisk::Low))
        },
        "the approved item to leave",
    )
    .await;
    assert!(
        snapshot
            .items
            .iter()
            .any(|i| i.risk == Some(ApprovalRisk::High)),
        "the high-risk approval still waits"
    );
    assert!(!w.replies().iter().any(|r| r.starts_with("git push")));

    // Declining goes through the same answer path.
    w.client
        .call(
            methods::RESPOND_ATTENTION,
            json!({ "itemId": high.id, "decision": "decline" }),
        )
        .await
        .expect("RespondAttention");
    wait_for(
        || {
            w.replies()
                .contains(&"git push --force: decline".to_string())
        },
        "the decline to reach the agent",
    )
    .await;

    // Dismissing a finished item marks its chat seen everywhere.
    let done_item = w
        .attention()
        .await
        .items
        .into_iter()
        .find(|i| i.chat_id.as_deref() == Some("done"))
        .expect("the finished chat is listed");
    let dismissed = w
        .client
        .call(
            methods::DISMISS_ATTENTION,
            json!({ "itemId": done_item.id }),
        )
        .await
        .expect("DismissAttention");
    assert_eq!(dismissed["dismissed"], true);
    let chat = w.core.workspace.chat("done").unwrap().unwrap();
    assert!(!chat.unseen(), "dismissing marks the chat seen");

    // Seeing a chat anywhere else drops its item live too.
    w.client
        .call(
            methods::MUTATE,
            json!({ "op": "markChatSeen", "chatId": "broke" }),
        )
        .await
        .expect("markChatSeen");
    let snapshot = watch_until(
        &mut stream,
        |s| {
            !s.items
                .iter()
                .any(|i| i.chat_id.as_deref() == Some("broke"))
        },
        "the seen failure to leave",
    )
    .await;
    // Approvals and questions can't be dismissed: they need an answer.
    let question = snapshot
        .items
        .iter()
        .find(|i| i.kind == AttentionKind::Question)
        .expect("the question still waits");
    assert!(
        w.client
            .call(methods::DISMISS_ATTENTION, json!({ "itemId": question.id }))
            .await
            .is_err()
    );
}

struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn question_timeout_answers_and_parks_but_approvals_wait() {
    let w = world();
    w.start("ask", "ASK");
    w.start("approve", "APPROVE ls");
    let mut stream = w.core.attention.watch();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while stream.borrow_and_update().items.len() < 2 {
        tokio::time::timeout_at(deadline, stream.changed())
            .await
            .expect("both chats wait on the user")
            .unwrap();
    }

    let t0 = Utc::now();
    let clock = Arc::new(ManualClock(Mutex::new(t0)));
    let data = w._tmp.path().join("timeout-service");
    std::fs::create_dir_all(&data).unwrap();
    let attention = AttentionService::open_with_clock(
        &data,
        AttentionDeps {
            workspace: w.core.workspace.clone(),
            doc_host: w.core.doc_host.clone(),
            sessions: w.core.sessions.clone(),
            agents: w.core.agents.clone(),
        },
        clock,
    );
    attention
        .set_timeout_settings(QuestionTimeoutSettings {
            default_minutes: Some(10),
            ..Default::default()
        })
        .unwrap();

    let deadline = attention
        .tick_at(t0)
        .expect("a waiting question has a deadline");
    assert_eq!(deadline, t0 + TimeDelta::minutes(10));
    attention.tick_at(t0 + TimeDelta::minutes(9));
    assert!(w.replies().is_empty(), "nothing times out early");

    attention.tick_at(t0 + TimeDelta::minutes(11));
    wait_for(
        || {
            w.replies()
                .contains(&format!("ASK: {QUESTION_TIMEOUT_ANSWER}"))
        },
        "the agent to get the timeout answer",
    )
    .await;
    wait_for(
        || {
            w.core
                .sessions
                .session_status("ask")
                .is_some_and(|s| s.status != zeron_proto::SessionStatus::AwaitingInput)
        },
        "the question's turn to move on",
    )
    .await;

    attention.tick_at(t0 + TimeDelta::hours(5));
    let snapshot = attention.snapshot();
    let parked = snapshot
        .items
        .iter()
        .find(|i| i.kind == AttentionKind::TimedOut)
        .expect("the question is parked as timed out");
    assert_eq!(parked.chat_id.as_deref(), Some("ask"));
    assert_eq!(parked.summary, "No answer after 10 min: Which database?");
    let approval = snapshot
        .items
        .iter()
        .find(|i| i.kind == AttentionKind::Approval)
        .expect("the approval still waits");
    assert_eq!(approval.chat_id.as_deref(), Some("approve"));
    assert!(
        !w.replies().iter().any(|r| r.starts_with("ls")),
        "approvals are never answered by a timeout"
    );

    // A per-chat "off" overrides the default; the parked item survives a
    // restart of the service and clears once acknowledged.
    let settings = attention
        .set_chat_timeout(
            "approve",
            Some(zeron_proto::attention::ChatQuestionTimeout::Off),
        )
        .unwrap();
    assert_eq!(settings.minutes_for("approve"), None);
    let reopened = AttentionService::open(
        &data,
        AttentionDeps {
            workspace: w.core.workspace.clone(),
            doc_host: w.core.doc_host.clone(),
            sessions: w.core.sessions.clone(),
            agents: w.core.agents.clone(),
        },
    );
    assert_eq!(reopened.timeout_settings(), settings);
    assert!(reopened.dismiss(&parked.id).unwrap());
    assert!(
        !reopened
            .snapshot()
            .items
            .iter()
            .any(|i| i.kind == AttentionKind::TimedOut)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_agent_runs_fold_in_and_dismissing_reads_the_run() {
    let w = world();
    let agent: zeron_proto::agents::AgentSummary = serde_json::from_value(
        w.client
            .call(
                methods::CREATE_AGENT,
                json!({ "draft": {
                    "name": "Nightly",
                    "harness": "mock",
                    "spaceId": SPACE,
                    "instructions": "check things",
                    "triggers": [],
                }}),
            )
            .await
            .expect("CreateAgent"),
    )
    .unwrap();
    let run: zeron_proto::agents::AgentRun = serde_json::from_value(
        w.client
            .call(methods::RUN_AGENT_NOW, json!({ "id": agent.id }))
            .await
            .expect("RunAgentNow"),
    )
    .unwrap();
    let mut stream = w
        .client
        .subscribe(methods::WATCH_ATTENTION, json!({}))
        .await
        .expect("WatchAttention");
    let snapshot = watch_until(
        &mut stream,
        |s| s.items.iter().any(|i| i.id == format!("run:{}", run.id)),
        "the finished run",
    )
    .await;
    let item = snapshot
        .items
        .iter()
        .find(|i| i.id == format!("run:{}", run.id))
        .unwrap();
    assert_eq!(item.kind, AttentionKind::Finished);
    assert_eq!(item.chat_id, run.chat_id);
    assert_eq!(item.agent_run.as_ref().unwrap().agent_name, "Nightly");
    assert_eq!(
        snapshot.items.len(),
        1,
        "the run's chat is folded into the run, not listed twice"
    );

    w.client
        .call(methods::DISMISS_ATTENTION, json!({ "itemId": item.id }))
        .await
        .expect("DismissAttention");
    let runs = w
        .core
        .agents
        .list_runs(&zeron_proto::agents::AgentRunFilter::default())
        .unwrap();
    assert!(runs.iter().all(|r| r.read), "dismissing reads the run");
    watch_until(&mut stream, |s| s.items.is_empty(), "an empty inbox").await;
}
