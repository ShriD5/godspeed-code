//! Prompt-idle watchdog (`GODSPEED_ACP_PROMPT_IDLE_MS`): an agent that starts
//! a turn and then goes quiet with the prompt still open must end the run
//! with a visible error, but not while one of its tools is still running.
//! Own test binary: the env knob is process-global.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp.sh");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

#[tokio::test]
async fn a_prompt_gone_quiet_after_its_tool_finishes_closes_the_run() {
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("GODSPEED_ACP_PROMPT_IDLE_MS", "700");
    }
    let (_steer_tx, steer_rx) = mpsc::channel(8);
    let controls = RunControls {
        realtime: None,
        execution_lease: None,
        request_input: Box::new(move |_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
        turn: Default::default(),
    };
    let request = RunRequest {
        instance: None,
        mcp: None,
        prompt: "scenario:prompt-idle".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
        access: None,
        env: Default::default(),
    };
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let stream = harness.run(request, controls).await.expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("watchdog closed the run in time");

    let tool_done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .expect("the long tool finished before the watchdog fired");
    let error = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Error { message } if message.contains("stopped responding")))
        .expect("visible error chip");
    assert!(
        tool_done < error,
        "an open tool pauses the watchdog: {events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(_),
            ..
        })
    ));
}
