use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionCommandStatus};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, ApprovalKind, ApprovalRequest, DoneStatus, HarnessId, Model, ReasoningLevel,
    RunRequest, SandboxLevel, SessionStatus, SteeringMode, UserInputAnswer, UserInputQuestion,
};

#[derive(Default)]
struct TestHarness {
    prompts: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for TestHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Delegation test harness"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        let needs_approval = matches!(
            request.prompt.as_str(),
            "parent own approval" | "child task"
        );
        if needs_approval {
            let question_id = if request.prompt == "child task" {
                "child-approval"
            } else {
                "parent-approval"
            };
            let request_input = (controls.request_input)(vec![UserInputQuestion {
                id: question_id.into(),
                header: "Permission".into(),
                question: format!("Allow {question_id}?").into(),
                options: vec!["Allow".into(), "Deny".into()],
                multi_select: false,
                prefill: None,
                multiline: false,
                approval: Some(ApprovalRequest::new(
                    ApprovalKind::Command,
                    format!("Run {question_id}"),
                )),
            }]);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                let answers = request_input.await.unwrap_or_default();
                let answer = answers
                    .first()
                    .and_then(|answer| answer.labels.first())
                    .cloned()
                    .unwrap_or_else(|| "none".into());
                let _ = tx.send(Ok(AgentEvent::TextDelta {
                    text: format!("{question_id} answered {answer}"),
                }));
                let _ = tx.send(Ok(done()));
            });
            return Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })));
        }

        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::TextDelta {
                text: "Parent received the child notification".into(),
            }),
            Ok(done()),
        ])
        .boxed())
    }
}

fn done() -> AgentEvent {
    AgentEvent::Done {
        status: DoneStatus::Completed,
        result: None,
        error: None,
        session_id: Some("delegation-test-session".into()),
    }
}

fn assemble(path: &std::path::Path, harness: Arc<TestHarness>) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    EngineCore::assemble(path, Arc::new(registry), HarnessId::Mock, None).unwrap()
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        instance: None,
        mcp: None,
        prompt: prompt.into(),
        harness: Some(HarnessId::Mock),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
        access: None,
        env: Default::default(),
    }
}

async fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn entries(core: &EngineCore, chat_id: &str) -> Vec<zeron_doc::SessionMessageEntry> {
    core.doc_host
        .open(chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
}

fn pending_request_id(core: &EngineCore, chat_id: &str) -> Option<String> {
    entries(core, chat_id)
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::Input {
                request_id,
                resolved: false,
                ..
            } => Some(request_id.clone()),
            _ => None,
        })
}

fn user_texts(core: &EngineCore, chat_id: &str) -> Vec<String> {
    entries(core, chat_id)
        .into_iter()
        .filter(|entry| entry.role == MessageRole::User)
        .map(|entry| {
            entry
                .parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

fn answer(question_id: &str) -> Vec<UserInputAnswer> {
    vec![UserInputAnswer {
        question_id: question_id.into(),
        labels: vec!["Allow".into()],
    }]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_approval_and_completion_notify_once_without_answering_parent_approval() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(TestHarness::default());
    let core = assemble(dir.path(), harness);
    core.workspace
        .create_chat(
            "parent",
            None,
            Some(&core.device_id),
            None,
            Some("/tmp".into()),
        )
        .unwrap();
    core.workspace.rename_chat("parent", "Parent").unwrap();
    core.workspace
        .create_chat_with_parent_notify(
            "child",
            None,
            Some(&core.device_id),
            None,
            Some("/tmp".into()),
            Some("parent".into()),
            true,
        )
        .unwrap();
    core.workspace.rename_chat("child", "Child").unwrap();

    core.sessions
        .dispatch(
            "parent",
            HarnessId::Mock,
            request("parent own approval"),
            Some("parent-prompt".into()),
        )
        .await
        .unwrap();
    wait_for(
        || {
            core.sessions
                .session_status("parent")
                .is_some_and(|s| s.status == SessionStatus::AwaitingInput)
                && pending_request_id(&core, "parent").is_some()
        },
        "the parent's own approval",
    )
    .await;
    let parent_request_id = pending_request_id(&core, "parent").expect("parent approval persisted");

    core.sessions
        .dispatch(
            "child",
            HarnessId::Mock,
            request("child task"),
            Some("child-prompt".into()),
        )
        .await
        .unwrap();
    wait_for(
        || {
            core.sessions
                .session_status("child")
                .is_some_and(|s| s.status == SessionStatus::AwaitingInput)
                && pending_request_id(&core, "child").is_some()
        },
        "the child's approval",
    )
    .await;
    let child_request_id = pending_request_id(&core, "child").expect("child approval persisted");
    let approval_event = format!("delegation:child:approval:{child_request_id}");
    wait_for(
        || {
            core.doc_host
                .open("parent")
                .is_ok_and(|handle| handle.doc().has_delegation_receipt(&approval_event))
        },
        "the durable child approval notification",
    )
    .await;
    assert!(
        core.sessions
            .session_status("parent")
            .is_some_and(|s| s.status == SessionStatus::AwaitingInput)
    );
    assert_eq!(
        pending_request_id(&core, "parent").as_deref(),
        Some(parent_request_id.as_str())
    );
    assert!(
        core.doc_host
            .open("parent")
            .unwrap()
            .doc()
            .read_queue()
            .unwrap()
            .iter()
            .any(|row| row.id == approval_event && row.text.contains("needs approval"))
    );

    assert!(
        core.sessions
            .respond_input("parent", &parent_request_id, answer("parent-approval"))
            .unwrap()
    );
    wait_for(
        || {
            user_texts(&core, "parent")
                .iter()
                .any(|text| text.contains("needs approval"))
        },
        "the approval notification to reach the parent after its own approval is answered",
    )
    .await;
    assert_eq!(
        user_texts(&core, "parent")
            .iter()
            .filter(|text| text.contains("needs approval"))
            .count(),
        1
    );

    assert!(
        core.sessions
            .respond_input("child", &child_request_id, answer("child-approval"))
            .unwrap()
    );
    wait_for(
        || {
            core.sessions
                .session_status("child")
                .is_some_and(|s| s.status == SessionStatus::Idle && s.last_completed_turn.is_some())
        },
        "the child to complete",
    )
    .await;
    let completed_turn = core
        .sessions
        .session_status("child")
        .and_then(|session| session.last_completed_turn)
        .unwrap();
    let completion_event = format!("delegation:child:completed:{completed_turn}");
    wait_for(
        || {
            core.doc_host
                .open("parent")
                .is_ok_and(|handle| handle.doc().has_delegation_receipt(&completion_event))
                && user_texts(&core, "parent")
                    .iter()
                    .any(|text| text.contains("Child chat child completed"))
        },
        "the durable child completion notification",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let parent_texts = user_texts(&core, "parent");
    assert_eq!(
        parent_texts
            .iter()
            .filter(|text| text.contains("needs approval"))
            .count(),
        1
    );
    assert_eq!(
        parent_texts
            .iter()
            .filter(|text| text.contains("Child chat child completed"))
            .count(),
        1
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_parent_keeps_notification_durable_across_restart_and_retry() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(TestHarness::default());
    let core = assemble(dir.path(), harness.clone());
    core.workspace
        .create_chat(
            "parent",
            None,
            Some(&core.device_id),
            None,
            Some("/tmp".into()),
        )
        .unwrap();
    let parent = core.doc_host.open("parent").unwrap();
    let stop_id = core
        .doc_host
        .queue_command("parent", SessionCommandPayload::Interrupt {})
        .unwrap();
    wait_for(
        || {
            parent.doc().read_commands().is_ok_and(|commands| {
                commands.iter().any(|command| {
                    command.id == stop_id && command.status == SessionCommandStatus::Applied
                })
            })
        },
        "the explicit stop to be recorded",
    )
    .await;

    let event_id = "delegation:child:approval:restart-event";
    assert!(
        core.doc_host
            .queue_delegation_notice("parent", event_id, "Child needs approval")
            .await
            .unwrap()
    );
    assert!(
        parent
            .doc()
            .read_queue()
            .unwrap()
            .iter()
            .any(|row| row.id == event_id)
    );
    assert!(harness.prompts.lock().unwrap().is_empty());
    core.shutdown().await;
    drop(parent);
    drop(core);

    let restarted_harness = Arc::new(TestHarness::default());
    let restarted = assemble(dir.path(), restarted_harness.clone());
    let parent = restarted.doc_host.open("parent").unwrap();
    assert!(
        !restarted
            .doc_host
            .queue_delegation_notice("parent", event_id, "Child needs approval")
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        parent
            .doc()
            .read_queue()
            .unwrap()
            .iter()
            .filter(|row| row.id == event_id)
            .count(),
        1
    );
    assert!(restarted_harness.prompts.lock().unwrap().is_empty());
    restarted.shutdown().await;
}
