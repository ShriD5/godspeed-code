//! Actual ClaudeHarness stream-json decoding through the engine's document
//! writer, from a captured 2.1.310 transcript the fake CLI replays. Offline;
//! no installed agent or model account is involved.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use zeron_engine::{EngineCore, EngineProfile, HarnessRegistry, SteerOutcome};
use zeron_harness::ClaudeHarness;
use zeron_proto::{HarnessId, RunRequest, SandboxLevel, SessionStatus};

const CHAT: &str = "claude-subagents";
/// The typed fork's chip, named after the command and the fork's task.
const CHIP: &str = "forked-command-fanout-a4d8fe55a62c09757";

fn assemble(dir: &Path) -> (EngineCore, EngineProfile) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/tests/fixtures/fake-claude.sh");
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(ClaudeHarness::new().with_executable(fixture)));
    let profile = EngineProfile::development(dir, "test-org", "test-user");
    let core =
        EngineCore::assemble_with_profile(profile.clone(), registry, HarnessId::ClaudeCode, None)
            .unwrap();
    (core, profile)
}

fn entries(core: &EngineCore, doc: &str) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(doc)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap_or_default()
}

/// The chat's spawn chips, the rows its lanes card lists.
fn rows(core: &EngineCore) -> Vec<(String, Option<String>, Option<SubagentStatus>)> {
    entries(core, CHAT)
        .iter()
        .flat_map(|e| &e.parts)
        .filter_map(|p| match p {
            MessagePart::Tool {
                id,
                call,
                subagent_ref,
                subagent_status,
                ..
            } if call.is_subagent_spawn() => {
                Some((id.clone(), subagent_ref.clone(), *subagent_status))
            }
            _ => None,
        })
        .collect()
}

/// The running-subagents badge.
fn badge(core: &EngineCore) -> Option<u32> {
    core.sessions
        .session_status(CHAT)
        .map(|s| s.running_subagents)
}

fn texts(entries: &[SessionMessageEntry], role: MessageRole) -> Vec<&str> {
    entries
        .iter()
        .filter(|e| e.role == role)
        .flat_map(|e| &e.parts)
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typed_fork_is_a_running_row_until_its_replayed_transcript_lands() {
    let dir = tempfile::tempdir().unwrap();
    let (core, profile) = assemble(dir.path());
    let request = RunRequest {
        mcp: None,
        prompt: "scenario:typed-fork-replay".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: dir.path().display().to_string(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: false,
        attachments: vec![],
        worktree: None,
        resume: None,
        access: None,
        env: Default::default(),
    };
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::ClaudeCode,
            request,
            Some("user-prompt".into()),
        )
        .await
        .unwrap();
    let sub = format!("{CHAT}--sub--{CHIP}");
    let running = (
        CHIP.to_owned(),
        Some(sub.clone()),
        Some(SubagentStatus::Running),
    );

    // The fork runs and the CLI holds its transcript back: its row is the
    // chat's one running spawn, on the badge, and its tab the skill's prompt.
    until("the fork's running row", || {
        rows(&core) == [running.clone()] && badge(&core) == Some(1)
    })
    .await;
    let opened = entries(&core, &sub);
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0].role, MessageRole::User);
    let prompt = texts(&opened, MessageRole::User).concat();
    assert!(
        prompt.starts_with("Base directory for this skill:"),
        "{prompt}"
    );
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Working)
    );

    std::fs::write(dir.path().join("release"), "").unwrap();
    // The capture's result lists two CLI-side permission denials, which
    // Godspeed reports as an errored turn: it ends Errored, not Idle.
    until("the turn's end", || {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| matches!(s.status, SessionStatus::Idle | SessionStatus::Errored))
            && entries(&core, CHAT).iter().any(|e| {
                e.role == MessageRole::Assistant
                    && e.status.is_some_and(|s| s != MessageStatus::Streaming)
            })
    })
    .await;
    // Settled with the turn: off the badge, its row done.
    assert_eq!(badge(&core), Some(0));
    assert_eq!(
        rows(&core),
        [(
            CHIP.to_owned(),
            Some(sub.clone()),
            Some(SubagentStatus::Done)
        )]
    );
    // Its tab got the replay: the prompt once, then the fork's own turn.
    let tab = entries(&core, &sub);
    assert_eq!(
        tab.iter().map(|e| e.role).collect::<Vec<_>>(),
        [MessageRole::User, MessageRole::Assistant],
        "{tab:?}"
    );
    assert_eq!(texts(&tab, MessageRole::User).concat(), prompt);
    assert!(
        tab.iter()
            .all(|e| e.status == Some(MessageStatus::Complete)),
        "{tab:?}"
    );
    let shape: Vec<&str> = tab[1]
        .parts
        .iter()
        .map(|p| match p {
            MessagePart::Reasoning { .. } => "thinking",
            MessagePart::Tool {
                call,
                resolved: true,
                ..
            } if call.is_subagent_spawn() => "agent",
            MessagePart::Text { text, .. } if text.trim() == "alpha bravo" => "reply",
            _ => "other",
        })
        .collect();
    assert_eq!(
        shape,
        ["thinking", "agent", "agent", "thinking", "reply"],
        "{tab:?}"
    );
    core.shutdown().await;
    drop(core);

    // The agents the fork ran are chips of its tab, with no tab of their own.
    let db = rusqlite::Connection::open(profile.store_root().join("docs.sqlite3")).unwrap();
    let ids: Vec<String> = db
        .prepare("SELECT doc_id FROM snapshots WHERE doc_id LIKE 'claude-subagents--sub--%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids, [sub]);
}

/// A typed `/skill` sent to a parked session goes to its live process as a
/// steer. The CLI starts it (`command_lifecycle` "started", so `Steered`)
/// before the fork's task start, so the chip lands in the new turn instead
/// of at the parked gate, and its row and tab work as for a first prompt.
#[tokio::test(flavor = "multi_thread")]
async fn a_typed_fork_sent_to_a_parked_session_keeps_its_chip() {
    const PROBE: &str = "forked-command-probe-t2";
    let dir = tempfile::tempdir().unwrap();
    let (core, _profile) = assemble(dir.path());
    let request = RunRequest {
        mcp: None,
        prompt: "scenario:typed-fork-parked-steer".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: dir.path().display().to_string(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: false,
        attachments: vec![],
        worktree: None,
        resume: None,
        access: None,
        env: Default::default(),
    };
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::ClaudeCode,
            request,
            Some("user-prompt".into()),
        )
        .await
        .unwrap();
    let idle = || {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| s.status == SessionStatus::Idle)
    };
    until("the first turn's end", || {
        idle()
            && entries(&core, CHAT).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
    })
    .await;

    // Parked with its process live: the command goes to it as a steer.
    assert_eq!(
        core.sessions.steer(CHAT, "/probe", None).await.unwrap(),
        SteerOutcome::Accepted
    );
    let sub = format!("{CHAT}--sub--{PROBE}");
    let row = |status| (PROBE.to_owned(), Some(sub.clone()), Some(status));
    until("the fork's running row", || {
        rows(&core) == [row(SubagentStatus::Running)] && badge(&core) == Some(1)
    })
    .await;
    std::fs::write(dir.path().join("release"), "").unwrap();
    until("the fork's end", || {
        idle() && rows(&core) == [row(SubagentStatus::Done)]
    })
    .await;
    assert_eq!(badge(&core), Some(0));
    let tab = entries(&core, &sub);
    assert_eq!(
        tab.iter().map(|e| e.role).collect::<Vec<_>>(),
        [MessageRole::User, MessageRole::Assistant],
        "{tab:?}"
    );
    assert_eq!(texts(&tab, MessageRole::User).concat(), "Probe.");
    core.shutdown().await;
}
