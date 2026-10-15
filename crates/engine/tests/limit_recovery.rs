//! Usage-limit recovery against a real engine: a turn the provider refuses
//! for a usage limit marks the chat Limited (persisted on its registry row),
//! sends made meanwhile wait in its queue, and a scheduled resume continues
//! the same session exactly once after the reset — across a restart too —
//! with the held messages following it. The timer is driven through
//! `tick_at`, so the clock is the test's.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;

use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload};
use zeron_engine::limit_recovery::RESUME_PROMPT;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, ChatIndicator, ChatLimit, DoneStatus, HarnessId, LimitRecovery, LimitResetSource,
    Model, ReasoningLevel, RunRequest, SandboxLevel, SteeringMode,
};
use zeron_rpc::methods;

const CHAT: &str = "chat-limited";

/// `LIMIT` is refused with a structured usage limit (reset from the test),
/// `PLAIN429` with only error text, `CONTEXT` with a full context window;
/// anything else replies.
struct LimitHarness {
    prompts: Arc<Mutex<Vec<String>>>,
    reset: Arc<Mutex<Option<DateTime<Utc>>>>,
}

#[async_trait]
impl Harness for LimitHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Limited"
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
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: "sess-limited".into(),
            assistant_message_id: "a-1".into(),
        };
        let failed = |message: &str| {
            vec![
                AgentEvent::Error {
                    message: message.into(),
                },
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message.into()),
                    session_id: Some("sess-limited".into()),
                },
            ]
        };
        let mut events = vec![started];
        if request.prompt.contains("LIMIT") {
            events.push(AgentEvent::UsageLimit {
                reset_at: *self.reset.lock().unwrap(),
                window: Some("5-hour".into()),
            });
            events.extend(failed("You've hit your usage limit"));
        } else if request.prompt.contains("PLAIN429") {
            events.extend(failed("HTTP 429 Too Many Requests"));
        } else if request.prompt.contains("CONTEXT") {
            events.extend(failed(
                "Prompt is too long: the conversation exceeds the context window",
            ));
        } else {
            events.push(AgentEvent::TextDelta {
                text: format!("ok: {}", request.prompt),
            });
            events.push(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-limited".into()),
            });
        }
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

struct World {
    _tmp: tempfile::TempDir,
    data_dir: std::path::PathBuf,
    cwd: String,
    prompts: Arc<Mutex<Vec<String>>>,
    reset: Arc<Mutex<Option<DateTime<Utc>>>>,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        Self {
            data_dir,
            cwd: cwd.to_string_lossy().into_owned(),
            _tmp: tmp,
            prompts: Default::default(),
            reset: Default::default(),
        }
    }

    fn assemble(&self) -> EngineCore {
        let registry = HarnessRegistry::new();
        registry.register(Arc::new(LimitHarness {
            prompts: self.prompts.clone(),
            reset: self.reset.clone(),
        }));
        EngineCore::assemble(&self.data_dir, Arc::new(registry), HarnessId::Mock, None)
            .expect("engine core assembles")
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }

    fn count(&self, prompt: &str) -> usize {
        self.prompts().iter().filter(|p| *p == prompt).count()
    }
}

async fn create_chat(core: &EngineCore) {
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            methods::MUTATE,
            json!({ "op": "createChat", "chatId": CHAT, "deviceId": core.device_id }),
        )
        .await
        .expect("createChat");
    // Pre-titled so the auto-titler never sends a request of its own.
    core.workspace.rename_chat(CHAT, "Pre-titled").unwrap();
}

/// A user send, through the durable command plane like the composer's.
fn send(core: &EngineCore, world: &World, prompt: &str) {
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: prompt.into(),
                    harness: Some(HarnessId::Mock),
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: world.cwd.clone(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: false,
                    resume: None,
                    attachments: Vec::new(),
                    worktree: None,
                    mcp: None,
                    access: None,
                },
                message_id: format!("msg-{}", prompt.replace(' ', "-")),
            },
        )
        .expect("queue command");
}

fn limit(core: &EngineCore) -> Option<ChatLimit> {
    core.workspace.chat(CHAT).unwrap().and_then(|c| c.limit)
}

fn queue_texts(core: &EngineCore) -> Vec<String> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_queue().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|row| row.text)
        .collect()
}

fn user_messages(core: &EngineCore) -> Vec<String> {
    core.doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.role == MessageRole::User)
        .map(|e| {
            e.parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}

async fn wait_for<F: FnMut() -> bool>(mut done: F, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Quiet for a moment: nothing more is going to happen on its own.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

async fn set_recovery(
    core: &EngineCore,
    action: &str,
    reset_at: Option<DateTime<Utc>>,
) -> serde_json::Value {
    let client = zeron_rpc::memory_client(core.rpc_service());
    let mut params = json!({ "chatId": CHAT, "action": action });
    if let Some(at) = reset_at {
        params["resetAt"] = json!(at);
    }
    client
        .call(methods::SET_CHAT_LIMIT_RECOVERY, params)
        .await
        .expect("SetChatLimitRecovery")
}

/// Whole seconds: the registry row stores what serde writes.
fn reset_in(delta: TimeDelta) -> DateTime<Utc> {
    DateTime::from_timestamp((Utc::now() + delta).timestamp(), 0).unwrap()
}

async fn limited_chat(world: &World, core: &EngineCore) -> DateTime<Utc> {
    let reset = reset_in(TimeDelta::hours(1));
    *world.reset.lock().unwrap() = Some(reset);
    create_chat(core).await;
    send(core, world, "please LIMIT");
    wait_for(|| limit(core).is_some(), "the chat to go Limited").await;
    reset
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limited_turn_resumes_once_after_the_reset_and_held_sends_follow() {
    let world = World::new();
    let core = world.assemble();
    let reset = limited_chat(&world, &core).await;

    let marker = limit(&core).unwrap();
    assert_eq!(marker.reset_at, Some(reset));
    assert_eq!(marker.reset_source, Some(LimitResetSource::Provider));
    assert_eq!(marker.window.as_deref(), Some("5-hour"));
    assert_eq!(
        marker.recovery,
        LimitRecovery::Ask,
        "auto-resume is off by default"
    );
    let chat = core.workspace.chat(CHAT).unwrap().unwrap();
    let session = core.sessions.session_status(CHAT);
    assert_eq!(
        zeron_proto::view::display_status(&chat, session.as_ref(), Utc::now()),
        ChatIndicator::Limited
    );

    // A send while limited waits instead of running into the limit.
    send(&core, &world, "held one");
    wait_for(|| queue_texts(&core) == ["held one"], "the send to be held").await;
    settle().await;
    assert_eq!(world.prompts(), ["please LIMIT"]);

    let reply = set_recovery(&core, "resume", None).await;
    assert_eq!(reply["limit"]["recovery"], "resumeAtReset");

    // Before the reset nothing moves; the timer wakes for it.
    assert_eq!(
        core.limit_recovery.tick_at(reset - TimeDelta::minutes(1)),
        Some(reset)
    );
    settle().await;
    assert_eq!(world.prompts(), ["please LIMIT"]);

    // After it: one continuation, then the held send.
    core.limit_recovery.tick_at(reset + TimeDelta::seconds(1));
    wait_for(
        || world.prompts().len() == 3,
        "the continuation and the held send",
    )
    .await;
    assert_eq!(world.prompts(), ["please LIMIT", RESUME_PROMPT, "held one"]);
    assert!(limit(&core).is_none());
    assert!(queue_texts(&core).is_empty());

    // Ticks after the fact fire nothing more.
    core.limit_recovery.tick_at(reset + TimeDelta::seconds(5));
    settle().await;
    assert_eq!(world.count(RESUME_PROMPT), 1);
    // The user's message stays where it was: once.
    let users = user_messages(&core);
    assert_eq!(
        users.iter().filter(|m| *m == "please LIMIT").count(),
        1,
        "{users:?}"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_resume_survives_an_engine_restart() {
    let world = World::new();
    let core = world.assemble();
    let reset = limited_chat(&world, &core).await;
    set_recovery(&core, "resume", None).await;
    send(&core, &world, "after restart");
    wait_for(
        || queue_texts(&core) == ["after restart"],
        "the send to be held",
    )
    .await;
    core.shutdown().await;
    drop(core);

    let core = world.assemble();
    let marker = limit(&core).expect("the marker persisted");
    assert_eq!(marker.recovery, LimitRecovery::ResumeAtReset);
    assert_eq!(marker.reset_at, Some(reset));
    settle().await;
    assert_eq!(
        world.prompts(),
        ["please LIMIT"],
        "nothing fires before the reset"
    );

    core.limit_recovery.tick_at(reset + TimeDelta::seconds(1));
    core.limit_recovery.tick_at(reset + TimeDelta::seconds(2));
    wait_for(
        || world.prompts().len() == 3,
        "the resume after the restart",
    )
    .await;
    assert_eq!(
        world.prompts(),
        ["please LIMIT", RESUME_PROMPT, "after restart"]
    );
    settle().await;
    assert_eq!(world.count(RESUME_PROMPT), 1);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dismissed_limit_never_fires_and_releases_what_it_held() {
    let world = World::new();
    let core = world.assemble();
    let reset = limited_chat(&world, &core).await;
    set_recovery(&core, "resume", None).await;
    send(&core, &world, "go anyway");
    wait_for(
        || queue_texts(&core) == ["go anyway"],
        "the send to be held",
    )
    .await;

    let reply = set_recovery(&core, "dismiss", None).await;
    assert!(reply["limit"].is_null());
    wait_for(|| world.count("go anyway") == 1, "the held send to go out").await;

    core.limit_recovery.tick_at(reset + TimeDelta::seconds(1));
    core.limit_recovery.tick_at(reset + TimeDelta::hours(2));
    settle().await;
    assert_eq!(world.count(RESUME_PROMPT), 0);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_and_asking_markers_clear_at_the_reset_without_a_continuation() {
    let world = World::new();
    let core = world.assemble();
    let reset = limited_chat(&world, &core).await;
    let reply = set_recovery(&core, "snooze", None).await;
    assert_eq!(reply["limit"]["recovery"], "snoozed");
    send(&core, &world, "later");
    wait_for(|| queue_texts(&core) == ["later"], "the send to be held").await;

    core.limit_recovery.tick_at(reset + TimeDelta::seconds(1));
    wait_for(|| world.count("later") == 1, "the held send at the reset").await;
    assert!(limit(&core).is_none());
    assert_eq!(world.count(RESUME_PROMPT), 0);

    // An asking marker with nothing held stays up past the reset (the
    // banner offers the resume) and holds nothing any more.
    *world.reset.lock().unwrap() = Some(reset_in(TimeDelta::hours(3)));
    send(&core, &world, "LIMIT again");
    wait_for(|| limit(&core).is_some(), "Limited again").await;
    let marker = limit(&core).unwrap();
    core.limit_recovery
        .tick_at(marker.reset_at.unwrap() + TimeDelta::seconds(1));
    settle().await;
    assert_eq!(limit(&core).map(|l| l.recovery), Some(LimitRecovery::Ask));
    assert_eq!(world.count(RESUME_PROMPT), 0);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_missed_by_hours_asks_instead_of_firing() {
    let world = World::new();
    let core = world.assemble();
    let reset = limited_chat(&world, &core).await;
    set_recovery(&core, "resume", None).await;
    core.limit_recovery.tick_at(reset + TimeDelta::hours(13));
    settle().await;
    assert_eq!(limit(&core).map(|l| l.recovery), Some(LimitRecovery::Ask));
    assert_eq!(world.count(RESUME_PROMPT), 0);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_resume_arms_new_limits_and_text_only_limits_ask_for_a_time() {
    let world = World::new();
    let core = world.assemble();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let settings = client
        .call(
            methods::SET_LIMIT_RECOVERY_SETTINGS,
            json!({ "autoResume": true }),
        )
        .await
        .unwrap();
    assert_eq!(settings, json!({ "autoResume": true }));
    let reset = limited_chat(&world, &core).await;
    assert_eq!(
        limit(&core).map(|l| l.recovery),
        Some(LimitRecovery::ResumeAtReset)
    );
    core.limit_recovery.tick_at(reset + TimeDelta::seconds(1));
    wait_for(|| world.count(RESUME_PROMPT) == 1, "the automatic resume").await;
    wait_for(|| limit(&core).is_none(), "the marker to clear").await;

    // Text only (no structured signal, no usage meters for the mock): the
    // engine classifies it, and with no reset there is nothing to arm.
    send(&core, &world, "PLAIN429 please");
    wait_for(|| limit(&core).is_some(), "the text-only limit").await;
    let marker = limit(&core).unwrap();
    assert_eq!(marker.reset_at, None);
    assert_eq!(marker.recovery, LimitRecovery::Ask);
    let refused = client
        .call(
            methods::SET_CHAT_LIMIT_RECOVERY,
            json!({ "chatId": CHAT, "action": "resume" }),
        )
        .await;
    assert!(refused.is_err(), "resume needs a time when none is known");
    // "Retry now": a time in the past resumes at the next tick.
    let picked = Utc::now() - TimeDelta::seconds(1);
    let reply = set_recovery(&core, "resume", Some(picked)).await;
    assert_eq!(reply["limit"]["resetSource"], "user");
    wait_for(|| world.count(RESUME_PROMPT) == 2, "the retry").await;

    // A full context window is an ordinary failure.
    send(&core, &world, "CONTEXT please");
    wait_for(|| world.count("CONTEXT please") == 1, "the context failure").await;
    settle().await;
    assert!(limit(&core).is_none());

    // The setting survives a restart.
    core.shutdown().await;
    drop(core);
    let core = world.assemble();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let settings = client
        .call(methods::GET_LIMIT_RECOVERY_SETTINGS, json!({}))
        .await
        .unwrap();
    assert_eq!(settings, json!({ "autoResume": true }));
    core.shutdown().await;
}
