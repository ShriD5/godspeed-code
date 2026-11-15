//! Idle compaction (`zeron_engine::idle_compaction`) through the real
//! ClaudeHarness and the fake CLI's `idle-compact` scenario: a large parked
//! chat compacts shortly before its prompt cache expires, as one quiet
//! transcript row, while the idle reaper waits for it; small, running,
//! waiting, archived and switched-off chats never do; and a message sent
//! mid-compaction waits in the queue and runs after it.
//!
//! The env knobs are process-global, so every test here shares them: the
//! cache lasts 4 s (the compaction starts 2 s in, half the lifetime before
//! expiry) and the idle reaper's window is 1.5 s, shorter than that wait.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use zeron_engine::{EngineCore, EngineProfile, HarnessRegistry};
use zeron_harness::ClaudeHarness;
use zeron_proto::{HarnessId, RunRequest, SandboxLevel, SessionStatus};

const CHAT: &str = "chat-idle-compact";
const TTL_MS: u64 = 4_000;
const IDLE_MS: u64 = 1_500;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: before any engine (any reader of these vars) exists in this
        // test process; every test shares the same values.
        unsafe {
            std::env::set_var("GODSPEED_IDLE_COMPACT_TTL_MS", TTL_MS.to_string());
            std::env::set_var("GODSPEED_SESSION_IDLE_MS", IDLE_MS.to_string());
            std::env::remove_var("GODSPEED_IDLE_COMPACTION");
            std::env::remove_var("GODSPEED_IDLE_COMPACT_MIN_TOKENS");
        }
    });
}

struct Fixture {
    core: EngineCore,
    cwd: PathBuf,
    _dir: tempfile::TempDir,
}

fn request(cwd: &Path, prompt: &str) -> RunRequest {
    RunRequest {
        instance: None,
        mcp: None,
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: false,
        attachments: vec![],
        worktree: None,
        resume: None,
        access: None,
        env: [(
            "CLAUDE_CONFIG_DIR".to_owned(),
            cwd.join("claude").display().to_string(),
        )]
        .into(),
    }
}

async fn fixture() -> Fixture {
    init_env();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/tests/fixtures/fake-claude.sh");
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(ClaudeHarness::new().with_executable(script)));
    let profile = EngineProfile::development(&dir.path().join("data"), "test-org", "test-user");
    let core =
        EngineCore::assemble_with_profile(profile, registry, HarnessId::ClaudeCode, None).unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": core.device_id }),
        )
        .await
        .expect("createChat");
    // Titled up front: the auto-titler must not run a harness turn of its own.
    core.workspace.rename_chat(CHAT, "Pre-titled").unwrap();
    Fixture {
        core,
        cwd,
        _dir: dir,
    }
}

impl Fixture {
    async fn start(&self, prompt: &str) {
        self.core
            .sessions
            .dispatch(
                CHAT,
                HarnessId::ClaudeCode,
                request(&self.cwd, prompt),
                None,
            )
            .await
            .expect("dispatch");
        until("the first turn to end", || {
            self.status() == Some(SessionStatus::Idle)
                && self.texts().contains(&"long answer".into())
        })
        .await;
    }

    fn entries(&self) -> Vec<SessionMessageEntry> {
        self.core
            .doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap_or_default()
    }

    fn texts(&self) -> Vec<String> {
        self.entries()
            .iter()
            .flat_map(|e| e.parts.clone())
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.trim().to_owned()),
                _ => None,
            })
            .collect()
    }

    fn compactions(&self) -> Vec<MessagePart> {
        self.entries()
            .iter()
            .flat_map(|e| e.parts.clone())
            .filter(|p| matches!(p, MessagePart::Compaction { .. }))
            .collect()
    }

    fn status(&self) -> Option<SessionStatus> {
        self.core.sessions.session_status(CHAT).map(|s| s.status)
    }

    fn live(&self) -> bool {
        self.core.sessions.has_run(CHAT)
    }

    fn queue(&self) -> Vec<String> {
        self.core
            .doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_queue()
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.text)
            .collect()
    }

    fn received(&self) -> Vec<String> {
        std::fs::read_to_string(self.cwd.join("received"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

async fn until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Well past when a compaction would have started (2 s), and past the
/// reaper's own window.
async fn past_the_compaction_window() {
    tokio::time::sleep(Duration::from_millis(TTL_MS - 500)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_idle_chat_compacts_before_its_cache_expires_then_reaps() {
    let f = fixture().await;
    let started = tokio::time::Instant::now();
    f.start("scenario:idle-compact").await;
    let first_turn = f
        .core
        .sessions
        .session_status(CHAT)
        .and_then(|s| s.last_completed_turn)
        .expect("the first turn notifies");

    // The reaper's window (1.5 s) ends before the compaction is due (2 s):
    // it waits, so the warm process is still there to compact.
    until("the idle compaction row", || !f.compactions().is_empty()).await;
    assert!(
        started.elapsed() >= Duration::from_millis(TTL_MS / 2),
        "compacted before the lead: {:?}",
        started.elapsed()
    );
    assert_eq!(
        f.compactions(),
        vec![MessagePart::Compaction {
            id: "c0".into(),
            summary: Some(
                "This session is being continued from a previous conversation. Summary: the parser bug is fixed."
                    .into()
            ),
            pre_tokens: Some(150_010),
            post_tokens: Some(4_200),
            error: None,
        }]
    );
    let entries = f.entries();
    // No `/compact` bubble, no "Context compacted." prose: just the row.
    assert!(
        entries
            .iter()
            .all(|e| e.role != MessageRole::User || !format!("{:?}", e.parts).contains("/compact")),
        "{entries:?}"
    );
    assert!(!f.texts().iter().any(|t| t.contains("Context compacted")));
    until("the chat to settle idle", || {
        f.status() == Some(SessionStatus::Idle)
    })
    .await;
    // Housekeeping, not a turn to tell the user about.
    assert_eq!(
        f.core
            .sessions
            .session_status(CHAT)
            .and_then(|s| s.last_completed_turn),
        Some(first_turn)
    );

    // Once compacted, the session is reaped on the user's own schedule (it
    // was due long ago), after a short grace.
    until("the reaper to end the session", || !f.live()).await;
    assert_eq!(f.compactions().len(), 1, "one idle spell compacts once");
    f.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_small_chat_never_compacts_and_reaps_on_time() {
    let f = fixture().await;
    f.start("scenario:idle-compact small").await;
    // Nothing holds the reaper for a small context.
    until("the reaper to end the session", || !f.live()).await;
    past_the_compaction_window().await;
    assert!(f.compactions().is_empty());
    assert!(!f.cwd.join("compacting").exists());
    f.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_waiting_on_a_question_never_compacts() {
    let f = fixture().await;
    f.core
        .sessions
        .dispatch(
            CHAT,
            HarnessId::ClaudeCode,
            request(&f.cwd, "scenario:idle-compact ask"),
            None,
        )
        .await
        .expect("dispatch");
    until("the question", || {
        f.status() == Some(SessionStatus::AwaitingInput)
    })
    .await;
    past_the_compaction_window().await;
    assert_eq!(f.status(), Some(SessionStatus::AwaitingInput));
    assert!(f.compactions().is_empty());
    assert!(!f.cwd.join("compacting").exists());
    f.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_chat_never_compacts() {
    let f = fixture().await;
    f.start("scenario:idle-compact").await;
    f.core.workspace.set_chat_archived(CHAT, true).unwrap();
    past_the_compaction_window().await;
    assert!(f.compactions().is_empty());
    assert!(!f.cwd.join("compacting").exists());
    f.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_setting_switches_it_off() {
    let f = fixture().await;
    let client = zeron_rpc::memory_client(f.core.rpc_service());
    let saved: zeron_proto::ChatLifecycleSettings = serde_json::from_value(
        client
            .call(
                zeron_rpc::methods::SET_CHAT_LIFECYCLE_SETTINGS,
                serde_json::json!({ "compactIdleChats": false }),
            )
            .await
            .expect("SetChatLifecycleSettings"),
    )
    .unwrap();
    assert!(!saved.compact_idle_chats);
    f.start("scenario:idle-compact").await;
    // Off, a large chat is reaped on the ordinary schedule.
    until("the reaper to end the session", || !f.live()).await;
    past_the_compaction_window().await;
    assert!(f.compactions().is_empty());
    assert!(!f.cwd.join("compacting").exists());
    f.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_sent_mid_compaction_waits_and_runs_after_it() {
    let f = fixture().await;
    std::fs::write(f.cwd.join("hold"), b"").unwrap();
    f.start("scenario:idle-compact").await;
    until("the compaction to start", || {
        f.cwd.join("compacting").exists()
    })
    .await;
    assert_eq!(f.status(), Some(SessionStatus::Working));

    // The composer's send, with the live turn's configuration.
    let mut next = f.core.sessions.last_request(CHAT).expect("a live request");
    next.prompt = "are you there".into();
    f.core
        .doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                message_id: "m-1".into(),
                request: next,
            },
        )
        .expect("queue run command");
    until("the send to wait in the queue", || {
        f.queue() == vec!["are you there"]
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(f.received().is_empty(), "delivered mid-compaction");

    std::fs::remove_file(f.cwd.join("hold")).unwrap();
    until("the reply after the compaction", || {
        f.texts().contains(&"reply:are you there".into())
    })
    .await;
    assert_eq!(f.received(), vec!["are you there"]);
    assert!(f.queue().is_empty());
    // In order: the compaction row, then the message and its reply.
    let entries = f.entries();
    let at = |find: &dyn Fn(&SessionMessageEntry) -> bool| {
        entries.iter().position(find).expect("entry present")
    };
    let compaction = at(&|e| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Compaction { .. }))
    });
    let message = at(&|e| {
        e.role == MessageRole::User
            && e.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "are you there"))
    });
    assert!(compaction < message, "{entries:?}");
    until("the reply's turn to end", || {
        f.status() == Some(SessionStatus::Idle)
    })
    .await;
    // The reply's turn is the user's: it notifies.
    let last = f
        .core
        .sessions
        .session_status(CHAT)
        .and_then(|s| s.last_completed_turn);
    assert!(
        entries
            .iter()
            .chain(f.entries().iter())
            .any(|e| Some(&e.id) == last.as_ref() && e.role == MessageRole::Assistant)
    );
    assert_eq!(f.compactions().len(), 1);
    f.core.shutdown().await;
}
