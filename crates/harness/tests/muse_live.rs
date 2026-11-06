//! Real `muse serve` on its echo provider: no login, no network, no spend.
//! Skips when the Muse Code CLI is not installed. Muse's home and config are
//! isolated in a temp dir, so the user's sessions and credentials are untouched.
#![cfg(unix)]
use futures::StreamExt;
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{
    CancellationToken, CatalogFailure, CatalogFailureCode, Harness, MuseHarness, RunControls,
    SteerMessage, TurnControl,
};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};

fn isolated_echo() -> Option<(tempfile::TempDir, MuseHarness)> {
    let exe = match MuseHarness::new().resolve_executable() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("skipping: {error}");
            return None;
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let quote =
        |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let wrapper = dir.path().join("muse-echo");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport XDG_DATA_HOME={} XDG_CONFIG_HOME={}\nexec {} \"$@\" --provider echo\n",
            quote(&dir.path().join("data")),
            quote(&dir.path().join("config")),
            quote(&exe)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir_all(dir.path().join("work")).unwrap();
    Some((dir, MuseHarness::new().with_executable(wrapper)))
}

struct Run {
    stream: futures::stream::BoxStream<'static, Result<AgentEvent, zeron_harness::HarnessError>>,
    steer: mpsc::Sender<SteerMessage>,
    interrupt: CancellationToken,
    turn: TurnControl,
}

async fn start(
    harness: &MuseHarness,
    cwd: &std::path::Path,
    prompt: &str,
    resume: Option<String>,
) -> Run {
    let (steer, steering) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let turn = TurnControl::default();
    let controls = RunControls {
        realtime: None,
        execution_lease: None,
        steering,
        interrupt: interrupt.clone(),
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        turn: turn.clone(),
    };
    let request = RunRequest {
        instance: None,
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume,
        attachments: vec![],
        worktree: None,
        mcp: None,
        access: None,
        env: Default::default(),
    };
    let stream = harness.run(request, controls).await.unwrap();
    Run {
        stream,
        steer,
        interrupt,
        turn,
    }
}

/// Events up to and including the next `Done`.
async fn until_done(run: &mut Run) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(event) = run.stream.next().await {
            let event = event.unwrap();
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                return;
            }
        }
        panic!("stream ended before Done: {events:?}");
    })
    .await
    .expect("Muse turn settles");
    events
}

fn text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn done(events: &[AgentEvent]) -> (DoneStatus, Option<String>, Option<String>) {
    match events.last() {
        Some(AgentEvent::Done {
            status,
            error,
            session_id,
            ..
        }) => (*status, error.clone(), session_id.clone()),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn steers(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Steered { .. }))
        .count()
}

async fn send(run: &Run, prompt: &str) {
    run.steer.send(SteerMessage::text(prompt)).await.unwrap();
}

#[tokio::test]
async fn echo_session_streams_steers_stops_and_resumes() {
    let Some((dir, harness)) = isolated_echo() else {
        return;
    };
    let cwd = dir.path().join("work");

    // initialize + session/start + a streamed turn, with a steer sent while
    // it runs: one engine turn, the steer confirmed exactly once.
    let mut run = start(&harness, &cwd, "hello muse", None).await;
    let first = run.stream.next().await.unwrap().unwrap();
    let AgentEvent::SessionStarted {
        session_id: session,
        harness: id,
        ..
    } = first
    else {
        panic!("expected SessionStarted, got {first:?}");
    };
    assert_eq!(id, zeron_proto::HarnessId::Muse);
    send(&run, "redirect").await;
    let events = until_done(&mut run).await;
    // A steer Muse absorbs before echo's single model call gets no echo of
    // its own; one admitted after it runs as the next turn of the same run.
    assert!(text(&events).contains("echo: "), "{events:?}");
    assert_eq!(steers(&events), 1, "{events:?}");
    let (status, error, done_session) = done(&events);
    assert_eq!(status, DoneStatus::Completed, "{error:?}");
    assert_eq!(done_session.as_deref(), Some(session.as_str()));

    // The runtime stays warm: the next prompt starts a turn in place.
    send(&run, "second").await;
    let events = until_done(&mut run).await;
    assert_eq!(steers(&events), 1);
    assert!(text(&events).contains("echo: second"), "{events:?}");
    assert_eq!(done(&events).0, DoneStatus::Completed);

    // Stop in place: turn/interrupt ends the turn, the host stays up. Echo's
    // end-of-turn reminder wait keeps the turn open long enough to stop.
    send(&run, "third").await;
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), run.stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let steered = matches!(event, AgentEvent::Steered { .. });
        events.push(event);
        if steered {
            run.turn.stop_turn();
            break;
        }
    }
    events.extend(until_done(&mut run).await);
    assert_eq!(done(&events).0, DoneStatus::Interrupted, "{events:?}");
    send(&run, "after stop").await;
    let events = until_done(&mut run).await;
    assert!(text(&events).contains("echo: after stop"), "{events:?}");
    assert_eq!(done(&events).0, DoneStatus::Completed);

    // Tearing the runtime down mid-turn still ends with Done(Interrupted).
    send(&run, "fourth").await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), run.stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(event, AgentEvent::Steered { .. }) {
            run.interrupt.cancel();
            break;
        }
    }
    let events = until_done(&mut run).await;
    assert_eq!(done(&events).0, DoneStatus::Interrupted, "{events:?}");
    assert!(
        tokio::time::timeout(Duration::from_secs(10), run.stream.next())
            .await
            .unwrap()
            .is_none()
    );

    // A new host resumes the same native session.
    let mut run = start(&harness, &cwd, "after restart", Some(session.clone())).await;
    let events = until_done(&mut run).await;
    assert!(
        matches!(&events[0], AgentEvent::SessionStarted { session_id, .. } if *session_id == session)
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "{events:?}"
    );
    assert!(text(&events).contains("echo: after restart"));
    assert_eq!(done(&events).2.as_deref(), Some(session.as_str()));
    drop(run);

    // An unknown session starts fresh with a visible notice instead of failing.
    let mut run = start(
        &harness,
        &cwd,
        "fresh",
        Some("01a13e59-0000-7000-8000-000000000000".into()),
    )
    .await;
    let events = until_done(&mut run).await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::Error { message } if message.contains("could not restore"))
    ));
    assert_eq!(done(&events).0, DoneStatus::Completed);
}

#[tokio::test]
async fn echo_model_discovery_reports_a_missing_login() {
    let Some((_dir, harness)) = isolated_echo() else {
        return;
    };
    // The isolated home has no credentials, so the catalog is empty: Muse's
    // logged-out answer, which must read as auth-required, not a crash.
    match harness.model_catalog(true).await {
        Ok(catalog) => assert!(!catalog.models.is_empty()),
        Err(error) => assert_eq!(
            CatalogFailure::classify(&error),
            CatalogFailureCode::AuthRequired,
            "{error}"
        ),
    }
}
