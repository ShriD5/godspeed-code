//! Fixtures follow the MSP v1 declarations in `@muse-code/sdk` (`msp.d.ts`)
//! and frames recorded from `muse serve --provider echo`.
use super::normalize::{self, Normalizer};
use super::*;
use serde_json::json;
use zeron_proto::{
    ApprovalDecision, ApprovalKind, DoneStatus, SandboxLevel, TodoItem, TodoStatus, ToolCall,
};

fn item(method: &str, item: Value) -> (String, Value) {
    (
        method.into(),
        json!({ "sessionId": "s", "viewCursor": "v3:s:1", "item": item }),
    )
}

fn delta(id: &str, field: &str, text: &str) -> (String, Value) {
    (
        "item/delta".into(),
        json!({ "sessionId": "s", "viewCursor": "v3:s:2", "itemId": id, "field": field, "delta": text }),
    )
}

fn feed(norm: &mut Normalizer, frames: Vec<(String, Value)>) -> Vec<AgentEvent> {
    frames
        .into_iter()
        .flat_map(|(method, params)| norm.map(&method, &params))
        .collect()
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

#[test]
fn agent_text_streams_once_and_the_final_item_only_adds_what_deltas_missed() {
    let mut norm = Normalizer::default();
    let events = feed(
        &mut norm,
        vec![
            item(
                "item/started",
                json!({"itemId":"m1","kind":"agentMessage","turnId":"t","revision":1,"status":"inProgress","text":""}),
            ),
            delta("m1", "text", "echo: "),
            delta("m1", "text", "hello"),
            item(
                "item/completed",
                json!({"itemId":"m1","kind":"agentMessage","turnId":"t","revision":2,"status":"completed","text":"echo: hello world","messagePhase":"finalAnswer"}),
            ),
            // A stale re-emission after settling changes nothing.
            item(
                "item/updated",
                json!({"itemId":"m1","kind":"agentMessage","revision":1,"status":"inProgress","text":"echo: hello world!!"}),
            ),
        ],
    );
    assert_eq!(text(&events), "echo: hello world");
    assert_eq!(events.len(), 3);
    // A second message in the same turn opens a paragraph.
    let events = feed(
        &mut norm,
        vec![
            item(
                "item/started",
                json!({"itemId":"m2","kind":"agentMessage","revision":1,"status":"inProgress","text":""}),
            ),
            delta("m2", "text", "next"),
        ],
    );
    assert_eq!(text(&events), "\n\nnext");
    norm.segment();
    let events = feed(&mut norm, vec![delta("m2", "text", " part")]);
    assert_eq!(text(&events), " part");
}

#[test]
fn reasoning_summaries_stream_per_part_and_raw_text_fills_in_when_nothing_streamed() {
    let mut norm = Normalizer::default();
    let events = feed(
        &mut norm,
        vec![
            item(
                "item/started",
                json!({"itemId":"r","kind":"reasoning","revision":1,"status":"inProgress","summary":[]}),
            ),
            delta("r", "summary.0", "Plan"),
            delta("r", "summary.1", "Check"),
            item(
                "item/completed",
                json!({"itemId":"r","kind":"reasoning","revision":2,"status":"completed","summary":["Plan","Check tests"]}),
            ),
        ],
    );
    let reasoning: Vec<_> = events
        .iter()
        .map(|e| match e {
            AgentEvent::ReasoningDelta { text } => text.as_str(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(reasoning, ["Plan", "\n\nCheck", " tests"]);
    let events = feed(
        &mut norm,
        vec![item(
            "item/completed",
            json!({"itemId":"r2","kind":"reasoning","revision":1,"status":"completed","text":"raw thoughts"}),
        )],
    );
    assert_eq!(
        events,
        [AgentEvent::ReasoningDelta {
            text: "raw thoughts".into()
        }]
    );
}

#[test]
fn shell_calls_announce_once_and_settle_with_streamed_output() {
    let mut norm = Normalizer::default();
    let call = json!({"itemId":"c","kind":"toolCall","revision":1,"status":"inProgress","tool":"shell","args":"{\"command\":[\"ls\",\"-la\"]}"});
    let events = feed(
        &mut norm,
        vec![
            item("item/started", call.clone()),
            item(
                "item/updated",
                json!({"itemId":"c","kind":"toolCall","revision":2,"status":"inProgress","tool":"shell","args":"{\"command\":[\"ls\",\"-la\"]}"}),
            ),
            delta("c", "output", "total 0\n"),
            item(
                "item/completed",
                json!({"itemId":"c","kind":"toolCall","revision":3,"status":"completed","tool":"shell","args":"{\"command\":[\"ls\",\"-la\"]}"}),
            ),
        ],
    );
    assert_eq!(
        events,
        [
            AgentEvent::ToolCall {
                id: "c".into(),
                call: ToolCall::Exec {
                    command: "ls -la".into()
                }
            },
            AgentEvent::ToolResult {
                id: "c".into(),
                is_error: false,
                output: Some("total 0\n".into()),
                diff: None
            },
        ]
    );
    // An item first seen at its terminal revision (after a gap) still pairs.
    let events = feed(
        &mut norm,
        vec![item(
            "item/completed",
            json!({"itemId":"c2","kind":"toolCall","revision":4,"status":"failed","tool":"read_file","args":"{\"path\":\"/a.rs\"}","failureReason":"denied"}),
        )],
    );
    assert!(
        matches!(&events[0], AgentEvent::ToolCall { call: ToolCall::ReadFile { path }, .. } if path == "/a.rs")
    );
    assert!(
        matches!(&events[1], AgentEvent::ToolResult { is_error: true, output: Some(o), .. } if o == "denied")
    );
}

#[test]
fn edits_carry_a_diff_only_when_they_succeed() {
    let mut norm = Normalizer::default();
    let args = r#"{"file_path":"/repo/a.rs","old_string":"old","new_string":"new"}"#;
    let events = feed(
        &mut norm,
        vec![item(
            "item/completed",
            json!({"itemId":"e","kind":"toolCall","revision":2,"status":"completed","tool":"edit_file","args":args,"patchSummary":{"files":1,"added":1,"removed":1}}),
        )],
    );
    assert!(matches!(
        &events[0],
        AgentEvent::ToolCall { call: ToolCall::EditFile { path, old_string: Some(o), new_string: Some(n) }, .. }
            if path == "/repo/a.rs" && o == "old" && n == "new"
    ));
    assert!(matches!(
        &events[1],
        AgentEvent::ToolResult { diff: Some(diff), is_error: false, .. }
            if diff.new_text == "new" && diff.old_text.as_deref() == Some("old")
    ));
    let events = feed(
        &mut norm,
        vec![item(
            "item/completed",
            json!({"itemId":"e2","kind":"toolCall","revision":2,"status":"rejected","tool":"write_file","args":r#"{"path":"/x","content":"y"}"#}),
        )],
    );
    assert!(matches!(
        &events[1],
        AgentEvent::ToolResult { diff: None, is_error: true, output: Some(o), .. } if o == "Muse reported rejected."
    ));
}

#[test]
fn todo_lists_refresh_the_live_plan_and_their_tool_call_stays_hidden() {
    let mut norm = Normalizer::default();
    let hidden = feed(
        &mut norm,
        vec![
            item(
                "item/started",
                json!({"itemId":"w","kind":"toolCall","revision":1,"status":"inProgress","tool":"write_todos","args":"{}"}),
            ),
            item(
                "item/completed",
                json!({"itemId":"w","kind":"toolCall","revision":2,"status":"completed","tool":"write_todos","args":"{}"}),
            ),
        ],
    );
    assert!(hidden.is_empty());
    let events = norm.map(
        "session/todoListChanged",
        &json!({"sessionId":"s","revision":3,"sourceTool":"write_todos","viewCursor":"v","items":[
            {"text":"read","status":"completed"},
            {"text":"fix","status":"inProgress","activeForm":"Fixing"},
            {"text":"ship","status":"pending"},
            {"text":"skip","status":"cancelled"}
        ]}),
    );
    assert_eq!(
        events[0],
        AgentEvent::ToolCall {
            id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
            call: ToolCall::Todo {
                items: vec![
                    TodoItem::new("read", TodoStatus::Completed),
                    TodoItem::new("fix", TodoStatus::InProgress),
                    TodoItem::new("ship", TodoStatus::Pending),
                    TodoItem::new("skip", TodoStatus::Pending),
                ]
            }
        }
    );
    assert!(
        matches!(&events[1], AgentEvent::ToolResult { id, .. } if id == zeron_proto::LIVE_PLAN_TOOL_ID)
    );
}

#[test]
fn subagents_open_a_child_transcript_and_count_as_background_work() {
    let mut norm = Normalizer::default();
    let events = feed(
        &mut norm,
        vec![item(
            "item/started",
            json!({"itemId":"a","kind":"subagent","revision":1,"status":"inProgress","role":"explorer","objective":"Map the crate\nin detail","childSessionId":"child"}),
        )],
    );
    assert!(
        matches!(&events[0], AgentEvent::ToolCall { id, call } if id == "a" && call.is_subagent_spawn()
        && matches!(call, ToolCall::Unknown { name, .. } if name == "Agent: explorer"))
    );
    assert!(
        matches!(&events[1], AgentEvent::Subagent { parent_tool_use_id, event }
        if parent_tool_use_id == "a" && matches!(&**event, AgentEvent::UserMessage { text } if text.starts_with("Map the crate")))
    );
    assert_eq!(norm.background(), 1);
    let events = feed(
        &mut norm,
        vec![item(
            "item/completed",
            json!({"itemId":"a","kind":"subagent","revision":2,"status":"completed","childSessionId":"child","result":{"summary":"done","text":"Found 3 modules","artifactRefs":[],"evidenceRefs":[]}}),
        )],
    );
    assert_eq!(norm.background(), 0);
    assert!(
        matches!(&events[0], AgentEvent::Subagent { event, .. } if matches!(&**event, AgentEvent::TextDelta { text } if text == "Found 3 modules"))
    );
    assert!(matches!(&events[1], AgentEvent::Subagent { event, .. }
        if matches!(&**event, AgentEvent::Done { status: DoneStatus::Completed, session_id: Some(s), .. } if s == "child")));
    assert!(matches!(&events[2], AgentEvent::ToolResult { id, is_error: false, .. } if id == "a"));
}

#[test]
fn turn_terminals_decide_status_and_a_logged_out_cli_says_so() {
    let mut norm = Normalizer::default();
    norm.turn_completed(&json!({"turnId":"t","terminal":"failed","error":{"kind":"authRequired","message":"no credential","retryable":false}}));
    assert_eq!(norm.status(), DoneStatus::Errored);
    assert!(norm.error.as_deref().unwrap().contains("muse login"));
    norm.turn_completed(&json!({"turnId":"t2","terminal":"cancelled","reason":"cancelled during end-of-turn reminder wait"}));
    assert_eq!(norm.status(), DoneStatus::Interrupted);
    norm.turn_completed(&json!({"turnId":"t3","terminal":"completed"}));
    assert_eq!(norm.status(), DoneStatus::Completed);
    assert!(norm.error.is_none());
}

#[test]
fn usage_notifications_map_to_billing_and_context_occupancy() {
    let mut norm = Normalizer::default();
    let usage = norm.map(
        "session/tokenUsage",
        &json!({"sessionId":"s","turnId":"t","modelId":null,"usage":{"inputTokens":90,"outputTokens":12,"cachedTokens":40,"reasoningTokens":0},"promptTokens":130,"totalTokens":142,"cumulative":{"promptTokens":130,"outputTokens":12,"totalTokens":142}}),
    );
    assert_eq!(
        usage,
        [AgentEvent::Usage {
            input_tokens: 130,
            output_tokens: 12
        }]
    );
    let context = norm.map(
        "session/contextUsage",
        &json!({"sessionId":"s","pressure":"normal","usedTokens":142,"windowTokens":200000}),
    );
    assert_eq!(
        context,
        [AgentEvent::ContextUsage {
            tokens: Some(142),
            window: Some(200000)
        }]
    );
}

fn approval_params(choices: Value) -> Value {
    json!({
        "approvalId": "ap", "itemId": "c", "sessionId": "s", "turnId": "t", "toolCallId": "call",
        "toolName": "shell", "taskId": "task", "rawArgs": "{}", "judgeEscalated": false,
        "protectedWrite": false, "viewCursor": "v",
        "currentRequirementId": {"approvalId": "ap", "sourceIndex": 0},
        "subject": {"kind": "shell", "command": "rm -rf build"},
        "availableChoices": choices,
    })
}

#[test]
fn approvals_offer_native_choices_and_never_default_to_allow() {
    let params = approval_params(json!([
        {"choiceId":"1","decision":"approved","label":"Allow once","scope":"once"},
        {"choiceId":"2","decision":"approvedForSession","label":"Allow for session","scope":"session"},
        {"choiceId":"3","decision":"abort","label":"Reject","scope":"once"},
    ]));
    let (question, choices) = normalize::approval_question(&params);
    assert_eq!(question.header, "Run command");
    let approval = question.approval.as_ref().unwrap();
    assert_eq!(approval.kind, ApprovalKind::Command);
    assert_eq!(approval.command.as_deref(), Some("rm -rf build"));
    assert_eq!(
        question.options,
        ["Allow once", "Allow for session", "Reject"]
    );
    let decisions: Vec<_> = approval.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(
        decisions,
        [
            ApprovalDecision::Approve,
            ApprovalDecision::ApproveForSession,
            ApprovalDecision::Decline,
        ]
    );
    assert_eq!(choices[1], ("Allow for session".into(), "2".into()));
    // Shell approvals offer only `abort` to say no.
    assert_eq!(
        normalize::default_choice(&params, false).as_deref(),
        Some("3")
    );
    assert_eq!(
        normalize::default_choice(&params, true).as_deref(),
        Some("1")
    );
    let params = approval_params(json!([
        {"choiceId":"d","decision":"denied","label":"Deny","scope":"once"},
        {"choiceId":"x","decision":"abort","label":"Stop","scope":"once"},
    ]));
    assert_eq!(
        normalize::default_choice(&params, false).as_deref(),
        Some("d")
    );
    assert_eq!(normalize::default_choice(&params, true), None);
}

#[test]
fn approvals_classify_file_writes_network_and_stop_choices() {
    let mut params = approval_params(json!([
        {"choiceId":"1","decision":"approved","label":"Allow once","scope":"once"},
        {"choiceId":"d","decision":"denied","label":"Deny","scope":"once"},
        {"choiceId":"x","decision":"abort","label":"Stop","scope":"once"},
    ]));
    params["subject"] = json!({"kind": "fileAccess", "access": "write", "path": "/repo/a.rs"});
    let (question, choices) = normalize::approval_question(&params);
    let approval = question.approval.unwrap();
    assert_eq!(approval.kind, ApprovalKind::FileChange);
    assert_eq!(approval.title, "Write /repo/a.rs");
    assert_eq!(approval.paths, ["/repo/a.rs"]);
    // With `denied` on offer, `abort` is the stop-the-turn choice.
    let decisions: Vec<_> = approval.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(
        decisions,
        [
            ApprovalDecision::Approve,
            ApprovalDecision::Decline,
            ApprovalDecision::Cancel,
        ]
    );
    assert_eq!(choices.len(), 3);
    // Auto-accept edits lets an in-workspace write through; supervised asks.
    let in_workspace = crate::approval::paths_within(
        approval.paths.iter().map(String::as_str),
        "/repo",
        &[],
    );
    assert!(crate::approval::auto_decision(
        AccessMode::AutoAcceptEdits,
        approval.kind,
        in_workspace
    ));
    assert!(!crate::approval::auto_decision(
        AccessMode::Supervised,
        approval.kind,
        in_workspace
    ));

    params["subject"] = json!({"kind": "network", "host": "example.com", "port": 443});
    let (question, _) = normalize::approval_question(&params);
    let approval = question.approval.unwrap();
    assert_eq!(approval.kind, ApprovalKind::Network);
    assert_eq!(approval.title, "Reach example.com:443");
}

#[test]
fn questions_answer_with_labels_or_free_text() {
    let params = json!({"userInputId":"u","sessionId":"s","itemId":"i","toolCallId":"c","toolName":"request_user_input","turnId":"t","viewCursor":"v","questions":[
        {"id":"q1","header":"Pick","question":"Which?","options":[{"label":"A"},{"label":"B"}],"selection":{"mode":"single"}},
        {"id":"q2","header":"","question":"Which ones?","options":[{"label":"X"},{"label":"Y"}],"selection":{"mode":"multiple","minSelections":1}},
    ]});
    let questions = normalize::input_questions(&params);
    assert_eq!(questions[1].1.header, "Muse question");
    assert!(questions[1].1.multi_select);
    let answer = |q: usize, labels: &[&str]| UserInputAnswer {
        question_id: questions[q].1.id.clone(),
        labels: labels.iter().map(|s| s.to_string()).collect(),
    };
    let wire =
        normalize::input_answers(&questions, &[answer(0, &["B"]), answer(1, &["X", "Y"])]).unwrap();
    assert_eq!(
        wire,
        json!([{"questionId":"q1","selectedLabel":"B"},{"questionId":"q2","selectedLabels":["X","Y"]}])
    );
    let wire = normalize::input_answers(
        &questions,
        &[answer(0, &["something else"]), answer(1, &["Y"])],
    )
    .unwrap();
    assert_eq!(
        wire[0],
        json!({"questionId":"q1","freeText":"something else"})
    );
    // An unanswered question cancels the prompt instead.
    assert!(normalize::input_answers(&questions, &[answer(0, &["A"])]).is_none());
}

#[test]
fn model_rows_map_efforts_and_fall_back_to_the_cli_ladder() {
    let list = json!({"providerId":"meta","profileId":"tbh","source":"providerCatalog","models":[
        {"modelId":"muse-large","displayLabel":"Muse Large","providerId":"meta","isDefault":true,"isActive":false,"contextLimit":200000,"cost":null,"description":"Flagship","outputLimit":null,"profileId":null,"releaseDate":null,"variants":["none","low","medium","high","xhigh","max","ultra"],"defaultReasoningEffort":"high"},
        {"modelId":"muse-small","displayLabel":" ","providerId":"meta","isDefault":false,"isActive":false,"contextLimit":null,"cost":null,"description":null,"outputLimit":null,"profileId":null,"releaseDate":null,"variants":"unknown"},
        {"modelId":"muse-large","displayLabel":"dup","providerId":"meta","isDefault":false,"isActive":false,"contextLimit":null,"cost":null,"description":null,"outputLimit":null,"profileId":null,"releaseDate":null},
    ]});
    let models = normalize::models(&list);
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].label, "Muse Large");
    assert_eq!(models[0].description.as_deref(), Some("Flagship"));
    assert_eq!(
        models[0].reasoning_levels,
        [
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
            ReasoningLevel::Ultra
        ]
    );
    assert_eq!(models[1].label, "muse-small");
    assert_eq!(models[1].reasoning_levels.len(), 5);
    assert!(normalize::models(&json!({"models": []})).is_empty());
    assert_eq!(normalize::effort(ReasoningLevel::XHigh), Some("xhigh"));
    assert_eq!(normalize::effort(ReasoningLevel::Ultrathink), None);
}

#[test]
fn launch_settings_follow_the_sandbox_and_approval_posture() {
    let mut request = RunRequest {
        prompt: "p".into(),
        harness: None,
        model: Some("default".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: "/repo".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume: None,
        attachments: vec![],
        worktree: None,
        mcp: None,
        access: None,
    };
    assert_eq!(serve_args(&request), ["--trust-workspace"]);
    assert_eq!(picked_model(&request), None);
    for (mode, expected) in [
        (AccessMode::Supervised, ("promptUnmatched", ":ask-me")),
        (AccessMode::AutoAcceptEdits, ("promptUnmatched", ":ask-me")),
        (AccessMode::Auto, ("promptUnmatched", ":auto-review")),
        (AccessMode::FullAccess, ("allowAll", ":unrestricted")),
    ] {
        assert_eq!(approval(mode), expected, "{mode:?}");
    }
    // Supervised asks; it no longer disables the shell outright.
    request.set_access_mode(AccessMode::Supervised);
    assert_eq!(serve_args(&request), ["--trust-workspace"]);
    request.set_access_mode(AccessMode::FullAccess);
    assert!(serve_args(&request).contains(&"--disable-sandbox"));
    request.set_access_mode(AccessMode::AutoAcceptEdits);

    let harness = MuseHarness::new();
    let mut next = request.clone();
    next.model = Some("muse-large".into());
    next.reasoning = Some(ReasoningLevel::High);
    assert!(!harness.same_runtime(&request, &next));
    assert!(harness.reconfigures_in_place(&request, &next));
    // Going back to the default model, or flipping approvals, needs a new host.
    assert!(!harness.reconfigures_in_place(&next, &request));
    next.model = request.model.clone();
    next.set_access_mode(AccessMode::FullAccess);
    assert!(!harness.reconfigures_in_place(&request, &next));

    let config = session_config(Some(&zeron_proto::McpServer {
        name: "godspeed".into(),
        command: "/bin/godspeed".into(),
        args: vec!["mcp".into()],
        env: [("K".to_string(), "V".to_string())].into(),
    }))
    .unwrap();
    assert_eq!(
        config,
        json!({"mcpServers":{"godspeed":{"transport":"stdio","command":"/bin/godspeed","args":["mcp"],"env":{"K":"V"},"mode":"optional"}}})
    );
}

#[test]
fn command_ids_are_uuid_v7() {
    let id = uuid::Uuid::parse_str(&command_id()).unwrap();
    assert_eq!(id.get_version_num(), 7);
    assert_ne!(command_id(), command_id());
}

#[tokio::test]
async fn prompts_become_text_and_inlinable_image_parts() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("a.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
    let svg = dir.path().join("b.svg");
    std::fs::write(&svg, b"<svg/>").unwrap();
    let parts = input_parts(
        "look",
        &[
            svg.display().to_string(),
            dir.path().join("gone.jpg").display().to_string(),
            png.display().to_string(),
        ],
    )
    .await;
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], json!({"type":"text","text":"look"}));
    assert_eq!(parts[1]["type"], "image");
    assert_eq!(parts[1]["mediaType"], "image/png");
    assert!(
        parts[1]["base64Data"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
}
