//! Always-on agents against a real engine: each fire becomes a normal chat in
//! the agent's space, the run is followed to completion, and the inbox row
//! carries the reply excerpt. Covers run-now, interval and cron fires on a
//! controlled clock (misfire grace, backlog collapse, idempotent ticks),
//! skip-if-running with an approval that is never auto-denied, auto-pause,
//! and the webhook trigger with `{{body.*}}` templating over the IPC port.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone as _, Utc};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use zeron_doc::{MessagePart, SessionCommandPayload};
use zeron_engine::agents::{Agents, Clock};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::agents::{AgentRun, AgentRunFilter, AgentRunStatus, AgentSummary};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};
use zeron_rpc::methods;

const SPACE: &str = "space-agents";

type PromptLog = Arc<Mutex<Vec<String>>>;

/// Answers by keyword in the instructions: `FAIL` errors out, `ASK` waits
/// on an approval question first, anything else replies with a report.
struct ScriptedHarness {
    prompts: PromptLog,
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
        self.prompts.lock().unwrap().push(request.prompt.clone());
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: "sess".into(),
            assistant_message_id: "a-1".into(),
        };
        let done = |status: DoneStatus, error: Option<&str>| AgentEvent::Done {
            status,
            result: None,
            error: error.map(str::to_owned),
            session_id: Some("sess".into()),
        };
        let instructions = request
            .prompt
            .split("## Instructions\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        if instructions.contains("FAIL") {
            let events = vec![
                Ok(started),
                Ok(AgentEvent::Error {
                    message: "boom: the build is broken".into(),
                }),
                Ok(done(DoneStatus::Errored, Some("boom: the build is broken"))),
            ];
            return Ok(futures::stream::iter(events).boxed());
        }
        if instructions.contains("ASK") {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let request_input = controls.request_input;
            tokio::spawn(async move {
                let _ = tx.send(started);
                let answers = request_input(vec![UserInputQuestion {
                    id: "q-approve".into(),
                    header: "Approval".into(),
                    question: "Run `rm -rf build/`?".into(),
                    options: vec!["Allow".into(), "Deny".into()],
                    prefill: None,
                    multiline: false,
                    multi_select: false,
                }])
                .await
                .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "nothing".into());
                let _ = tx.send(AgentEvent::TextDelta {
                    text: format!("You picked {picked}; cleaned up."),
                });
                let _ = tx.send(done(DoneStatus::Completed, None));
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            });
            return Ok(stream.boxed());
        }
        let events = vec![
            Ok(started),
            Ok(AgentEvent::TextDelta {
                text: "Report: all green.\nNothing else to do.".into(),
            }),
            Ok(done(DoneStatus::Completed, None)),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn set(&self, at: DateTime<Utc>) {
        *self.0.lock().unwrap() = at;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

struct World {
    _tmp: tempfile::TempDir,
    core: EngineCore,
    client: zeron_rpc::RpcClient,
    prompts: PromptLog,
    workspace_dir: std::path::PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let prompts = PromptLog::default();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ScriptedHarness {
        prompts: prompts.clone(),
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
    core.workspace
        .create_space(
            SPACE,
            &core.device_id,
            &workspace_dir.to_string_lossy(),
            None,
            false,
        )
        .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    World {
        _tmp: tmp,
        core,
        client,
        prompts,
        workspace_dir,
    }
}

fn draft(name: &str, instructions: &str, triggers: Value) -> Value {
    json!({
        "name": name,
        "harness": "mock",
        "spaceId": SPACE,
        "instructions": instructions,
        "triggers": triggers,
    })
}

async fn create(client: &zeron_rpc::RpcClient, draft: Value) -> AgentSummary {
    let value = client
        .call(methods::CREATE_AGENT, json!({ "draft": draft }))
        .await
        .expect("CreateAgent");
    serde_json::from_value(value).unwrap()
}

async fn runs(client: &zeron_rpc::RpcClient, filter: Value) -> Vec<AgentRun> {
    let value = client
        .call(methods::LIST_AGENT_RUNS, filter)
        .await
        .expect("ListAgentRuns");
    serde_json::from_value(value).unwrap()
}

fn runs_of(agents: &Agents, agent_id: &str) -> Vec<AgentRun> {
    agents
        .list_runs(&AgentRunFilter {
            agent_id: Some(agent_id.into()),
            include_archived: true,
            ..Default::default()
        })
        .unwrap()
}

async fn wait_for<F>(mut done: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_run(world: &World, run_id: &str, status: AgentRunStatus) -> AgentRun {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let found = world
            .core
            .agents
            .list_runs(&AgentRunFilter {
                include_archived: true,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .find(|r| r.id == run_id);
        if let Some(run) = found.filter(|r| r.status == status) {
            return run;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run {run_id} never reached {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn run_now_and_interval_fires_become_chats_with_inbox_summaries() {
    let w = world();
    let agent = create(
        &w.client,
        draft(
            "Nightly",
            "Check the build and report.",
            json!([{ "kind": "interval", "everySecs": 3600 }]),
        ),
    )
    .await;
    assert!(agent.draft.enabled);
    assert_eq!(agent.workspace_path, w.workspace_dir.to_string_lossy());
    assert!(
        agent
            .memory_path
            .ends_with(&format!("agents/{}/MEMORY.md", agent.id))
    );
    assert!(
        agent.next_fire_at.is_some(),
        "interval agents have a next fire"
    );
    std::fs::write(&agent.memory_path, "- last run: build was flaky on macOS\n").unwrap();

    let run: AgentRun = serde_json::from_value(
        w.client
            .call(methods::RUN_AGENT_NOW, json!({ "id": agent.id }))
            .await
            .expect("RunAgentNow"),
    )
    .unwrap();
    let chat_id = run.chat_id.clone().expect("the run created a chat");
    let done = wait_for_run(&w, &run.id, AgentRunStatus::Succeeded).await;
    assert_eq!(
        done.summary.as_deref(),
        Some("Report: all green. Nothing else to do.")
    );
    assert!(done.started_at.is_some() && done.finished_at.is_some());
    assert!(!done.read);

    // A normal chat in the agent's space, titled for the agent and trigger.
    let chat = w.core.workspace.chat(&chat_id).unwrap().unwrap();
    assert_eq!(chat.title.as_deref(), Some("Nightly · Run now"));
    assert_eq!(chat.space_id.as_deref(), Some(SPACE));
    assert_eq!(chat.config.as_ref().unwrap().harness, HarnessId::Mock);

    // Memory is injected and the agent is told where to update it.
    let prompt = w.prompts.lock().unwrap().last().cloned().unwrap();
    assert!(
        prompt.contains("- last run: build was flaky on macOS"),
        "{prompt}"
    );
    assert!(prompt.contains(&agent.memory_path));
    assert!(prompt.ends_with("## Instructions\nCheck the build and report."));

    // The inbox links the chat back to the run; mark read + archive.
    let by_chat = runs(&w.client, json!({ "chatId": chat_id })).await;
    assert_eq!(by_chat.len(), 1);
    assert_eq!(by_chat[0].id, run.id);
    let changed = w
        .client
        .call(
            methods::MARK_AGENT_RUNS,
            json!({ "ids": [run.id], "read": true, "archived": true }),
        )
        .await
        .unwrap();
    assert_eq!(changed["changed"], 2);
    assert!(runs(&w.client, json!({})).await.is_empty());

    // An interval fire, driven through the scheduler's tick.
    let next = agent.next_fire_at.unwrap();
    w.core
        .agents
        .tick_at(next + chrono::TimeDelta::seconds(5))
        .await;
    let interval_runs: Vec<_> = runs_of(&w.core.agents, &agent.id)
        .into_iter()
        .filter(|r| r.trigger == zeron_proto::agents::AgentTriggerKind::Interval)
        .collect();
    assert_eq!(interval_runs.len(), 1);
    assert_eq!(interval_runs[0].scheduled_at, next);
    let fired = wait_for_run(&w, &interval_runs[0].id, AgentRunStatus::Succeeded).await;
    let chat = w
        .core
        .workspace
        .chat(fired.chat_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(chat.title.as_deref(), Some("Nightly · Interval"));
    // The same tick again is a no-op: the fire was recorded.
    w.core
        .agents
        .tick_at(next + chrono::TimeDelta::seconds(6))
        .await;
    assert_eq!(runs_of(&w.core.agents, &agent.id).len(), 2);

    let deleted = w
        .client
        .call(methods::DELETE_AGENT, json!({ "id": agent.id }))
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], true);
    assert!(!std::path::Path::new(&agent.memory_path).exists());
    assert!(
        w.core.workspace.chat(&chat_id).unwrap().is_some(),
        "chats stay"
    );
    w.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cron_ticks_on_a_controlled_clock_honor_grace_backlog_and_restarts() {
    let w = world();
    let clock = Arc::new(ManualClock(Mutex::new(utc(2026, 6, 1, 8, 0))));
    let open = || {
        Agents::open_with_clock(
            &w._tmp.path().join("scheduled"),
            w.core.workspace.clone(),
            w.core.doc_host.clone(),
            w.core.sessions.clone(),
            w.core.registry.clone(),
            clock.clone(),
        )
        .unwrap()
    };
    let agents = open();
    let agent = agents
        .create(
            serde_json::from_value(draft(
                "Morning",
                "Summarize overnight changes.",
                json!([{ "kind": "cron", "expr": "0 9 * * *", "timezone": "Europe/Berlin" }]),
            ))
            .unwrap(),
        )
        .unwrap();
    // 09:00 Berlin (CEST) = 07:00 UTC; at 08:00 UTC today's slot has passed.
    assert_eq!(
        agents.tick_at(utc(2026, 6, 1, 8, 0)).await,
        Some(utc(2026, 6, 2, 7, 0))
    );
    assert!(runs_of(&agents, &agent.id).is_empty());

    // 10 minutes late: inside the 15 minute grace → runs for 07:00.
    clock.set(utc(2026, 6, 2, 7, 10));
    agents.tick_at(utc(2026, 6, 2, 7, 10)).await;
    let first = runs_of(&agents, &agent.id);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].scheduled_at, utc(2026, 6, 2, 7, 0));
    wait_for(
        || runs_of(&agents, &agent.id)[0].status == AgentRunStatus::Succeeded,
        "cron run to finish",
    )
    .await;

    // A restart recomputes from persisted state: same answer, no re-fire.
    drop(agents);
    let agents = open();
    assert_eq!(
        agents.tick_at(utc(2026, 6, 2, 7, 11)).await,
        Some(utc(2026, 6, 3, 7, 0))
    );
    assert_eq!(runs_of(&agents, &agent.id).len(), 1);

    // Missed by 5h → recorded as skipped, not run.
    clock.set(utc(2026, 6, 3, 12, 0));
    agents.tick_at(utc(2026, 6, 3, 12, 0)).await;
    let skipped = runs_of(&agents, &agent.id)[0].clone();
    assert_eq!(skipped.status, AgentRunStatus::Skipped);
    assert_eq!(skipped.scheduled_at, utc(2026, 6, 3, 7, 0));
    assert!(skipped.chat_id.is_none());
    assert!(
        skipped
            .summary
            .as_deref()
            .unwrap()
            .contains("missed by 5h 0m"),
        "{:?}",
        skipped.summary
    );

    // Down for two more days, back 5 minutes after a slot: one run that
    // stands in for the missed day.
    clock.set(utc(2026, 6, 5, 7, 5));
    agents.tick_at(utc(2026, 6, 5, 7, 5)).await;
    let latest = runs_of(&agents, &agent.id)[0].clone();
    assert_eq!(latest.scheduled_at, utc(2026, 6, 5, 7, 0));
    assert_eq!(latest.collapsed, 1, "06-04 folds into 06-05");
    assert_ne!(latest.status, AgentRunStatus::Skipped);
    assert_eq!(runs_of(&agents, &agent.id).len(), 3);
    wait_for(
        || runs_of(&agents, &agent.id)[0].status == AgentRunStatus::Succeeded,
        "backlog run to finish",
    )
    .await;
    agents.shutdown().await;
    w.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_agents_skip_fires_and_approvals_wait_until_answered() {
    let w = world();
    let agent = create(
        &w.client,
        draft("Janitor", "ASK before cleaning.", json!([])),
    )
    .await;
    let run = w.core.agents.run_now(&agent.id).await.unwrap();
    let parked = wait_for_run(&w, &run.id, AgentRunStatus::NeedsApproval).await;
    let chat_id = parked.chat_id.clone().unwrap();

    // Further fires while it waits are skipped and fold into one row.
    let skip = w.core.agents.run_now(&agent.id).await.unwrap();
    assert_eq!(skip.status, AgentRunStatus::Skipped);
    assert!(skip.summary.as_deref().unwrap().contains("still active"));
    let again = w.core.agents.run_now(&agent.id).await.unwrap();
    assert_eq!(again.id, skip.id);
    assert_eq!(again.collapsed, 1);
    let snapshot = w.core.agents.snapshot().unwrap();
    assert_eq!(snapshot.needs_approval_runs, 1);
    assert_eq!(
        snapshot.agents[0].active_run_id.as_deref(),
        Some(run.id.as_str())
    );

    // Nobody answers on the user's behalf.
    tokio::time::sleep(Duration::from_millis(500)).await;
    wait_for_run(&w, &run.id, AgentRunStatus::NeedsApproval).await;

    let request_id = w
        .core
        .doc_host
        .open(&chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
        .iter()
        .flat_map(|e| e.parts.iter())
        .find_map(|part| match part {
            MessagePart::Input { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the question is in the transcript");
    w.core
        .doc_host
        .queue_command(
            &chat_id,
            SessionCommandPayload::RespondInput {
                request_id,
                answers: vec![UserInputAnswer {
                    question_id: "q-approve".into(),
                    labels: vec!["Allow".into()],
                }],
            },
        )
        .unwrap();
    let done = wait_for_run(&w, &run.id, AgentRunStatus::Succeeded).await;
    assert_eq!(
        done.summary.as_deref(),
        Some("You picked Allow; cleaned up.")
    );
    w.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn three_failures_in_a_row_pause_the_agent() {
    let w = world();
    let agent = create(
        &w.client,
        draft(
            "Flaky",
            "FAIL on purpose.",
            json!([{ "kind": "interval", "everySecs": 60 }, { "kind": "webhook" }]),
        ),
    )
    .await;
    for attempt in 1..=3 {
        let run = w.core.agents.run_now(&agent.id).await.unwrap();
        let failed = wait_for_run(&w, &run.id, AgentRunStatus::Failed).await;
        assert_eq!(
            failed.summary.as_deref(),
            Some("boom: the build is broken"),
            "attempt {attempt}"
        );
    }
    let paused = &w.core.agents.list().unwrap()[0];
    assert_eq!(paused.consecutive_failures, 3);
    let reason = paused.paused_reason.as_deref().expect("auto-paused");
    assert!(reason.contains("3 consecutive failures") && reason.contains("boom"));
    assert!(paused.next_fire_at.is_none());

    // The scheduler leaves a paused agent alone.
    let before = runs_of(&w.core.agents, &agent.id).len();
    w.core
        .agents
        .tick_at(Utc::now() + chrono::TimeDelta::hours(3))
        .await;
    assert_eq!(runs_of(&w.core.agents, &agent.id).len(), before);

    // Re-enabling lifts the pause and restarts the schedule.
    let resumed: AgentSummary = serde_json::from_value(
        w.client
            .call(
                methods::SET_AGENT_ENABLED,
                json!({ "id": agent.id, "enabled": true }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(resumed.paused_reason.is_none());
    assert_eq!(resumed.consecutive_failures, 0);
    assert!(resumed.next_fire_at.is_some());
    w.core.shutdown().await;
}

async fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let json = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    (status, json)
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_fires_with_templated_payload_and_dedupes_deliveries() {
    let w = world();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(zeron_rpc::serve_ws_listener(listener, w.core.rpc_service()));
    w.core.sessions.set_ipc_port(port);

    let agent = create(
        &w.client,
        draft(
            "Triage",
            "New {{headers.x-github-event}} on {{body.repository.full_name}}: #{{body.issue.number}} {{body.issue.title}}",
            json!([{ "kind": "webhook" }]),
        ),
    )
    .await;
    let url = agent.webhook_url.clone().expect("served IPC port → URL");
    assert_eq!(
        url,
        format!("http://127.0.0.1:{port}{}", agent.webhook_path)
    );

    let body =
        r#"{"repository":{"full_name":"acme/app"},"issue":{"number":42,"title":"Crash on save"}}"#;
    let headers = [("X-GitHub-Event", "issues"), ("X-GitHub-Delivery", "d-1")];
    let (status, reply) = post(port, &agent.webhook_path, &headers, body).await;
    assert_eq!(status, 202, "{reply}");
    let run_id = reply["runId"].as_str().unwrap().to_string();
    assert!(reply["chatId"].is_string());
    let done = wait_for_run(&w, &run_id, AgentRunStatus::Succeeded).await;
    assert_eq!(done.trigger, zeron_proto::agents::AgentTriggerKind::Webhook);
    let prompt = w.prompts.lock().unwrap().last().cloned().unwrap();
    assert!(
        prompt.ends_with("## Instructions\nNew issues on acme/app: #42 Crash on save"),
        "{prompt}"
    );

    // A redelivery of the same delivery id is the same run.
    let (status, reply) = post(port, &agent.webhook_path, &headers, body).await;
    assert_eq!(status, 202);
    assert_eq!(reply["runId"], run_id.as_str());
    assert_eq!(runs_of(&w.core.agents, &agent.id).len(), 1);

    // Wrong token and wrong method are refused; rotation kills the old URL.
    let wrong = format!("/hooks/agents/{}/nope", agent.id);
    assert_eq!(post(port, &wrong, &[], "{}").await.0, 404);
    let rotated: AgentSummary = serde_json::from_value(
        w.client
            .call(methods::ROTATE_AGENT_WEBHOOK, json!({ "id": agent.id }))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_ne!(rotated.webhook_path, agent.webhook_path);
    assert_eq!(post(port, &agent.webhook_path, &[], "{}").await.0, 404);

    // Disabled agents refuse deliveries instead of queueing them.
    w.core.agents.set_enabled(&agent.id, false).unwrap();
    let (status, reply) = post(port, &rotated.webhook_path, &[], "{}").await;
    assert_eq!(status, 409, "{reply}");

    // WatchAgents streams the snapshot.
    let mut watch = w
        .client
        .subscribe(methods::WATCH_AGENTS, json!({}))
        .await
        .unwrap();
    let first = watch.recv().await.unwrap();
    assert_eq!(first["agents"][0]["id"], agent.id.as_str());
    assert_eq!(first["unreadRuns"], 1);
    w.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_changes_fire_one_debounced_run() {
    let w = world();
    let src = w.workspace_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let agent = create(
        &w.client,
        draft(
            "Reviewer",
            "Review what changed.",
            json!([{ "kind": "fileWatch", "paths": ["src"], "debounceMs": 300 }]),
        ),
    )
    .await;
    // Let the watch go live before writing (the hub installs it in a batch).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();
    std::fs::write(src.join("b.rs"), "fn b() {}").unwrap();
    // Outside the watched path: never a reason to run.
    std::fs::write(w.workspace_dir.join("README.md"), "hi").unwrap();

    wait_for(
        || {
            runs_of(&w.core.agents, &agent.id)
                .iter()
                .any(|r| r.status == AgentRunStatus::Succeeded)
        },
        "file-change run",
    )
    .await;
    let all = runs_of(&w.core.agents, &agent.id);
    assert_eq!(all.len(), 1, "one burst, one run: {all:?}");
    assert_eq!(
        all[0].trigger,
        zeron_proto::agents::AgentTriggerKind::FileWatch
    );
    let prompt = w.prompts.lock().unwrap().last().cloned().unwrap();
    assert!(prompt.contains("Changed paths:\n"), "{prompt}");
    assert!(
        prompt.contains("- src/a.rs") && prompt.contains("- src/b.rs"),
        "{prompt}"
    );
    assert!(!prompt.contains("README.md"));
    w.core.shutdown().await;
}
