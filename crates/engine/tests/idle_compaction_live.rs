//! Opt-in real-CLI check of idle compaction: one throwaway Claude chat in a
//! temp directory, a cache lifetime shortened so the compaction comes within
//! a minute, and the next turn's cache write compared with the context the
//! compaction replaced. Three model requests on the signed-in account.
//!
//! GODSPEED_TEST_MODEL=claude-haiku-4-5 cargo test -p zeron-engine --test idle_compaction_live -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry, TurnUsage};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::ClaudeHarness;
use zeron_proto::{HarnessId, RunRequest, SandboxLevel, SessionStatus};

const CHAT: &str = "idle-compact-live";

async fn until(what: &str, secs: u64, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(secs), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap_or_default()
}

fn usage(entry: &SessionMessageEntry) -> Option<TurnUsage> {
    entry.turn_stats.as_ref().and_then(|s| s.usage)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "uses the signed-in Claude account; run by hand"]
async fn a_real_claude_chat_compacts_while_idle_and_the_next_turn_starts_small() {
    // SAFETY: before the engine (any reader) exists.
    unsafe {
        // Compact 45 s after the last request instead of 58 min.
        std::env::set_var("GODSPEED_IDLE_COMPACT_TTL_MS", "90000");
        std::env::set_var("GODSPEED_IDLE_COMPACT_MIN_TOKENS", "1000");
    }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let notes: String = (1..=1500)
        .map(|n| format!("{n:04} the quick brown fox jumps over the lazy dog, entry {n}\n"))
        .collect();
    std::fs::write(work.join("notes.txt"), notes).unwrap();

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ClaudeHarness::new()));
    let core = EngineCore::assemble(
        &dir.path().join("engine"),
        Arc::new(registry),
        HarnessId::ClaudeCode,
        None,
    )
    .unwrap();
    let request = |prompt: &str| RunRequest {
        instance: None,
        mcp: None,
        prompt: prompt.into(),
        harness: None,
        model: std::env::var("GODSPEED_TEST_MODEL").ok(),
        reasoning: None,
        model_options: Default::default(),
        cwd: work.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: vec![],
        worktree: None,
        resume: None,
        access: None,
        env: Default::default(),
    };
    let idle = || {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| s.status == SessionStatus::Idle)
    };

    core.sessions
        .dispatch(
            CHAT,
            HarnessId::ClaudeCode,
            request(
                "Read notes.txt in full with the Read tool, then reply with only the last line's number.",
            ),
            None,
        )
        .await
        .unwrap();
    until("the first turn", 240, || {
        idle() && entries(&core).iter().any(|e| usage(e).is_some())
    })
    .await;
    let first = entries(&core)
        .iter()
        .rev()
        .find_map(usage)
        .expect("first turn usage");
    eprintln!("first turn: {first:?}");
    let context = core.doc_host.open(CHAT).unwrap().doc().context_usage();
    eprintln!("context before: {context:?}");

    let compacted_at = tokio::time::Instant::now();
    until("the idle compaction", 180, || {
        idle()
            && entries(&core).iter().any(|e| {
                e.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::Compaction { .. }))
            })
    })
    .await;
    let compaction = entries(&core)
        .into_iter()
        .find(|e| {
            e.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Compaction { .. }))
        })
        .unwrap();
    eprintln!(
        "compaction after {:?}: {:#?}\ncompaction usage: {:?}",
        compacted_at.elapsed(),
        compaction.parts,
        usage(&compaction)
    );
    assert!(
        !entries(&core)
            .iter()
            .any(|e| e.role == MessageRole::User && format!("{:?}", e.parts).contains("/compact"))
    );

    core.sessions
        .dispatch(
            CHAT,
            HarnessId::ClaudeCode,
            request("Reply with just the word ok."),
            None,
        )
        .await
        .unwrap();
    until("the next turn", 240, || {
        idle()
            && entries(&core)
                .last()
                .is_some_and(|e| e.role == MessageRole::Assistant && usage(e).is_some())
    })
    .await;
    let next = entries(&core).last().and_then(usage).unwrap();
    eprintln!("next turn: {next:?}");
    let written = next.cache_write_tokens.unwrap_or(0) + next.input_tokens;
    let before = first.cache_write_tokens.unwrap_or(0) + first.cache_read_tokens.unwrap_or(0);
    eprintln!("next turn wrote {written} uncached tokens; the first turn's context was ~{before}");
    core.shutdown().await;
}
