//! Chat import from T3 Code, Claude Code and Codex history.
//!
//! Fixtures are built per test under a temp "home": a T3 database created
//! with T3 Code's real projection schema (left open in WAL mode, the way the
//! running app holds it), Claude Code session files, and Codex rollouts.
//! Asserts the field mapping, transcript order, idempotency, cross-source
//! dedupe, update-in-place, that the native resume id reaches the next
//! `RunRequest`, and that no source byte changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use zeron_engine::chat_import::{ChatImporter, ImportRoots, chat_id_for};
use zeron_engine::{EngineCore, EngineProfile, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::chat_import::{
    ChatImportPreview, ChatImportRequest, ChatImportSource, ChatImportState, ChatImportSummary,
};
use zeron_proto::{
    AccessMode, AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode, ToolCall,
};

// ── fixtures ────────────────────────────────────────────────────────────────

/// T3 Code's projection tables, verbatim from a live `state.sqlite`.
const T3_SCHEMA: &str = r#"
CREATE TABLE projection_projects (
      project_id TEXT PRIMARY KEY,
      title TEXT NOT NULL,
      workspace_root TEXT NOT NULL,
      scripts_json TEXT NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      deleted_at TEXT
    , default_model_selection_json TEXT, default_thread_env_mode TEXT, favicon_path TEXT, auto_pull INTEGER NOT NULL DEFAULT 0, project_icon_json TEXT);
CREATE TABLE projection_threads (
      thread_id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      title TEXT NOT NULL,
      branch TEXT,
      worktree_path TEXT,
      latest_turn_id TEXT,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      deleted_at TEXT
    , runtime_mode TEXT NOT NULL DEFAULT 'full-access', interaction_mode TEXT NOT NULL DEFAULT 'default', model_selection_json TEXT, archived_at TEXT, latest_user_message_at TEXT, pending_approval_count INTEGER NOT NULL DEFAULT 0, pending_user_input_count INTEGER NOT NULL DEFAULT 0, has_actionable_proposed_plan INTEGER NOT NULL DEFAULT 0, settled_override TEXT, settled_at TEXT, snoozed_until TEXT, snoozed_at TEXT, title_regeneration_request_id TEXT, title_regeneration_started_at TEXT, pinned_at TEXT, pin_order_key TEXT, linked_pull_request_json TEXT, unsettled_at TEXT, branch_pull_request_json TEXT, active_order_key TEXT);
CREATE TABLE projection_thread_messages (
      message_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      role TEXT NOT NULL,
      text TEXT NOT NULL,
      is_streaming INTEGER NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    , attachments_json TEXT);
CREATE TABLE projection_thread_activities (
      activity_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      tone TEXT NOT NULL,
      kind TEXT NOT NULL,
      summary TEXT NOT NULL,
      payload_json TEXT NOT NULL,
      created_at TEXT NOT NULL
    , sequence INTEGER);
CREATE TABLE provider_session_runtime (
      thread_id TEXT PRIMARY KEY,
      provider_name TEXT NOT NULL,
      adapter_key TEXT NOT NULL,
      runtime_mode TEXT NOT NULL DEFAULT 'full-access',
      status TEXT NOT NULL,
      last_seen_at TEXT NOT NULL,
      resume_cursor_json TEXT,
      runtime_payload_json TEXT
    , provider_instance_id TEXT);
"#;

/// The smallest valid PNG (1×1).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    /// The project folder every source's sessions ran in.
    repo: String,
    /// T3 Code's own connection, held open like the running app holds it.
    t3: Connection,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo-a");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let repo = repo.to_string_lossy().to_string();
        let userdata = home.join(".t3").join("userdata");
        std::fs::create_dir_all(userdata.join("attachments")).unwrap();
        std::fs::write(userdata.join("attachments").join("att-1.png"), PNG).unwrap();
        let t3 = Connection::open(userdata.join("state.sqlite")).unwrap();
        t3.pragma_update(None, "journal_mode", "WAL").unwrap();
        // Keep every write in the WAL: the importer must read it from the copy.
        t3.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        t3.execute_batch(T3_SCHEMA).unwrap();
        let fixture = Self {
            _tmp: tmp,
            home,
            repo,
            t3,
        };
        fixture.seed_t3();
        fixture.write_claude();
        fixture.write_codex();
        fixture
    }

    fn roots(&self) -> ImportRoots {
        ImportRoots::under_home(&self.home)
    }

    fn seed_t3(&self) {
        let c = &self.t3;
        let project = |id: &str, title: &str, root: &str, deleted: Option<&str>| {
            c.execute(
                "INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json,
                 created_at, updated_at, deleted_at) VALUES (?1, ?2, ?3, '[]', ?4, ?4, ?5)",
                params![id, title, root, "2026-09-01T00:00:00.000Z", deleted],
            )
            .unwrap();
        };
        project("p1", "Repo A", &self.repo, None);
        project(
            "p-gone",
            "Gone",
            "/nowhere/gone",
            Some("2026-09-02T00:00:00.000Z"),
        );
        #[allow(clippy::too_many_arguments)]
        let thread =
            |id: &str,
             project: &str,
             title: &str,
             created: &str,
             selection: &str,
             deleted: Option<&str>,
             archived: Option<&str>,
             pinned: Option<&str>| {
                c.execute(
                "INSERT INTO projection_threads (thread_id, project_id, title, branch, created_at,
                 updated_at, deleted_at, model_selection_json, archived_at, pinned_at)
                 VALUES (?1, ?2, ?3, 'feat/x', ?4, ?4, ?5, ?6, ?7, ?8)",
                params![id, project, title, created, deleted, selection, archived, pinned],
            )
            .unwrap();
            };
        thread(
            "t-claude",
            "p1",
            "Fix the login bug",
            "2026-09-10T10:00:10.000Z",
            r#"{"instanceId":"claudeAgent","model":"claude-opus-5-5","options":[{"id":"effort","value":"high"}]}"#,
            None,
            None,
            Some("2026-09-10T11:00:00.000Z"),
        );
        thread(
            "t-codex",
            "p1",
            "Port the parser",
            "2026-09-11T10:00:10.000Z",
            r#"{"instanceId":"codex","model":"gpt-6-astra","options":[{"id":"reasoningEffort","value":"xhigh"}]}"#,
            None,
            Some("2026-09-12T00:00:00.000Z"),
            None,
        );
        thread(
            "t-cursor",
            "p1",
            "Ask grok",
            "2026-09-12T10:00:10.000Z",
            r#"{"instanceId":"cursor","model":"grok-4.6"}"#,
            None,
            None,
            None,
        );
        thread(
            "t-deleted",
            "p1",
            "Deleted thread",
            "2026-09-12T11:00:00.000Z",
            r#"{"instanceId":"codex","model":"gpt-6-astra"}"#,
            Some("2026-09-13T00:00:00.000Z"),
            None,
            None,
        );
        thread(
            "t-gone-project",
            "p-gone",
            "In a deleted project",
            "2026-09-12T12:00:00.000Z",
            r#"{"instanceId":"codex","model":"gpt-6-astra"}"#,
            None,
            None,
            None,
        );
        let runtime = |thread: &str, provider: &str, cursor: &str| {
            c.execute(
                "INSERT INTO provider_session_runtime (thread_id, provider_name, adapter_key, status,
                 last_seen_at, resume_cursor_json) VALUES (?1, ?2, ?2, 'stopped', ?3, ?4)",
                params![thread, provider, "2026-09-12T00:00:00.000Z", cursor],
            )
            .unwrap();
        };
        runtime(
            "t-claude",
            "claudeAgent",
            r#"{"threadId":"t-claude","resume":"claude-owned","resumeSessionAt":"x","turnCount":2}"#,
        );
        runtime("t-codex", "codex", r#"{"threadId":"codex-owned"}"#);
        runtime(
            "t-cursor",
            "cursor",
            r#"{"schemaVersion":1,"sessionId":"cursor-session"}"#,
        );
        for (thread, n) in [
            ("t-claude", 2),
            ("t-codex", 1),
            ("t-cursor", 1),
            ("t-deleted", 1),
        ] {
            for turn in 0..n {
                self.t3_turn(thread, turn);
            }
        }
        // The first user message carries an image.
        c.execute(
            "UPDATE projection_thread_messages SET attachments_json = ?1 WHERE message_id = 't-claude-u0'",
            params![r#"[{"type":"image","id":"att-1","name":"shot.png","mimeType":"image/png","sizeBytes":70}]"#],
        )
        .unwrap();
        // A tool call inside the first Claude turn.
        let activity = |id: &str, kind: &str, at: &str, seq: i64, payload: &str| {
            c.execute(
                "INSERT INTO projection_thread_activities (activity_id, thread_id, turn_id, tone, kind,
                 summary, payload_json, created_at, sequence) VALUES (?1, 't-claude', 'turn-0', 'tool', ?2, 'Command run', ?3, ?4, ?5)",
                params![id, kind, payload, at, seq],
            )
            .unwrap();
        };
        activity(
            "a1",
            "tool.started",
            "2026-09-10T10:00:02.500Z",
            1,
            r#"{"itemType":"command_execution","toolCallId":"toolu_1","status":"inProgress","title":"Command run","data":{"toolName":"Bash","input":{}}}"#,
        );
        activity(
            "a2",
            "tool.completed",
            "2026-09-10T10:00:02.800Z",
            2,
            r#"{"itemType":"command_execution","toolCallId":"toolu_1","status":"completed","title":"Command run","data":{"toolName":"Bash","input":{"command":"cargo test"},"result":{"content":"ok"}}}"#,
        );
    }

    /// One user → assistant → assistant turn at minute `turn` past 10:00.
    fn t3_turn(&self, thread: &str, turn: u32) {
        let day = match thread {
            "t-claude" => 10,
            "t-codex" => 11,
            _ => 12,
        };
        let at = |second: u32| format!("2026-09-{day}T10:{turn:02}:{second:02}.000Z");
        for (suffix, role, text, second) in [
            ("u", "user", format!("question {turn}"), 1),
            ("a", "assistant", format!("looking {turn}"), 2),
            ("b", "assistant", format!("answer {turn}"), 3),
        ] {
            self.t3
                .execute(
                    "INSERT INTO projection_thread_messages (message_id, thread_id, turn_id, role, text,
                     is_streaming, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
                    params![
                        format!("{thread}-{suffix}{turn}"),
                        thread,
                        format!("turn-{turn}"),
                        role,
                        text,
                        at(second)
                    ],
                )
                .unwrap();
        }
        self.t3
            .execute(
                "UPDATE projection_threads SET updated_at = ?1 WHERE thread_id = ?2",
                params![at(3), thread],
            )
            .unwrap();
    }

    fn write_claude(&self) {
        let dir = self
            .home
            .join(".claude")
            .join("projects")
            .join("-tmp-repo-a");
        std::fs::create_dir_all(dir.join("claude-new").join("subagents")).unwrap();
        let line = |v: serde_json::Value| v.to_string();
        let base = |kind: &str, uuid: &str, at: &str, message: serde_json::Value| {
            serde_json::json!({
                "type": kind, "uuid": uuid, "timestamp": at, "cwd": self.repo,
                "sessionId": "claude-new", "gitBranch": "main", "isSidechain": false,
                "message": message,
            })
        };
        let lines = [
            line(serde_json::json!({"type": "queue-operation", "operation": "enqueue"})),
            line(base(
                "user",
                "c-u1",
                "2026-09-20T09:00:00.000Z",
                serde_json::json!({"role": "user", "content": "add a health check"}),
            )),
            line({
                let mut meta = base(
                    "user",
                    "c-meta",
                    "2026-09-20T09:00:00.100Z",
                    serde_json::json!({"role": "user", "content": "<system-reminder>ctx</system-reminder>"}),
                );
                meta["isMeta"] = true.into();
                meta
            }),
            line(base(
                "assistant",
                "c-a1",
                "2026-09-20T09:00:01.000Z",
                serde_json::json!({"role": "assistant", "model": "claude-sonnet-5-5",
                    "content": [{"type": "text", "text": "Adding it."}]}),
            )),
            line(base(
                "assistant",
                "c-a2",
                "2026-09-20T09:00:02.000Z",
                serde_json::json!({"role": "assistant", "model": "claude-sonnet-5-5",
                    "content": [{"type": "tool_use", "id": "toolu_9", "name": "Edit",
                        "input": {"file_path": "src/main.rs", "old_string": "a", "new_string": "b"}}]}),
            )),
            line(base(
                "user",
                "c-r1",
                "2026-09-20T09:00:03.000Z",
                serde_json::json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_9", "content": "ok", "is_error": true}]}),
            )),
            line(base(
                "assistant",
                "c-a3",
                "2026-09-20T09:00:04.000Z",
                serde_json::json!({"role": "assistant", "model": "claude-sonnet-5-5",
                    "content": [{"type": "text", "text": "Done."}]}),
            )),
            line(
                serde_json::json!({"type": "ai-title", "aiTitle": "Health check endpoint", "sessionId": "claude-new"}),
            ),
        ];
        std::fs::write(dir.join("claude-new.jsonl"), lines.join("\n") + "\n").unwrap();
        // The session a T3 thread resumes: T3 is its canonical import.
        let owned = [
            line(serde_json::json!({
                "type": "user", "uuid": "o-u1", "timestamp": "2026-09-10T10:00:01.000Z",
                "cwd": self.repo, "sessionId": "claude-owned", "isSidechain": false,
                "message": {"role": "user", "content": "question 0"},
            })),
            line(serde_json::json!({
                "type": "assistant", "uuid": "o-a1", "timestamp": "2026-09-10T10:00:02.000Z",
                "cwd": self.repo, "sessionId": "claude-owned", "isSidechain": false,
                "message": {"role": "assistant", "content": [{"type": "text", "text": "answer 0"}]},
            })),
        ];
        std::fs::write(dir.join("claude-owned.jsonl"), owned.join("\n") + "\n").unwrap();
        // Subagent transcripts are not conversations.
        std::fs::write(
            dir.join("claude-new")
                .join("subagents")
                .join("agent-1.jsonl"),
            owned.join("\n"),
        )
        .unwrap();
    }

    fn write_codex(&self) {
        let dir = self
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09")
            .join("21");
        std::fs::create_dir_all(&dir).unwrap();
        let rollout = |id: &str, extra_meta: serde_json::Value, prompt: &str| {
            let mut meta = serde_json::json!({
                "id": id, "session_id": id, "timestamp": "2026-09-21T08:00:00.000Z",
                "cwd": self.repo, "originator": "codex_exec", "source": "exec",
                "git": {"branch": "main"},
            });
            for (k, v) in extra_meta.as_object().unwrap() {
                meta[k] = v.clone();
            }
            let item = |ordinal: u32, payload: serde_json::Value| {
                serde_json::json!({"timestamp": format!("2026-09-21T08:00:{ordinal:02}.000Z"),
                    "type": "response_item", "payload": payload})
                .to_string()
            };
            [
                serde_json::json!({"timestamp": "2026-09-21T08:00:00.000Z", "type": "session_meta", "payload": meta}).to_string(),
                serde_json::json!({"timestamp": "2026-09-21T08:00:00.000Z", "type": "turn_context",
                    "payload": {"model": "gpt-6-astra", "effort": "high"}}).to_string(),
                item(1, serde_json::json!({"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "<environment_context>\n<cwd>/x</cwd>\n</environment_context>"}]})),
                item(2, serde_json::json!({"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": prompt}]})),
                item(3, serde_json::json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Plan it"}]})),
                item(4, serde_json::json!({"type": "function_call", "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_1"})),
                item(5, serde_json::json!({"type": "function_call_output", "call_id": "call_1", "output": "a\nb"})),
                item(6, serde_json::json!({"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Listed."}]})),
                serde_json::json!({"timestamp": "2026-09-21T08:00:07.000Z", "type": "event_msg",
                    "payload": {"type": "task_complete"}}).to_string(),
            ]
            .join("\n")
                + "\n"
        };
        std::fs::write(
            dir.join("rollout-2026-09-21T08-00-00-codex-new.jsonl"),
            rollout("codex-new", serde_json::json!({}), "list files"),
        )
        .unwrap();
        std::fs::write(
            dir.join("rollout-2026-09-21T08-00-00-codex-owned.jsonl"),
            rollout("codex-owned", serde_json::json!({}), "port it"),
        )
        .unwrap();
        std::fs::write(
            dir.join("rollout-2026-09-21T08-00-00-codex-child.jsonl"),
            rollout(
                "codex-child",
                serde_json::json!({"source": {"subagent": {"thread_spawn": {"parent_thread_id": "codex-new"}}}}),
                "child work",
            ),
        )
        .unwrap();
        std::fs::write(
            self.home.join(".codex").join("session_index.jsonl"),
            r#"{"id":"codex-new","thread_name":"List the files","updated_at":"2026-09-21T08:00:07Z"}"#
                .to_string()
                + "\n",
        )
        .unwrap();
    }

    /// sha256 of every file under the fixture home.
    fn source_hashes(&self) -> BTreeMap<PathBuf, String> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    out.insert(path, format!("{:x}", Sha256::digest(&bytes)));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.home, &mut out);
        out
    }
}

// ── engine harness ──────────────────────────────────────────────────────────

type RequestLog = Arc<Mutex<Vec<(HarnessId, RunRequest)>>>;

/// Records every `RunRequest` under the harness id it registers as.
struct RecordingHarness {
    id: HarnessId,
    requests: RequestLog,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        "Recording"
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
        self.requests
            .lock()
            .unwrap()
            .push((self.id, request.clone()));
        let session = request.resume.clone().unwrap_or_else(|| "fresh".into());
        let events = vec![
            Ok(AgentEvent::SessionStarted {
                harness: self.id,
                model: "m".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: session.clone(),
                assistant_message_id: "a-live".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "continued".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some(session),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

struct Engine {
    _data: tempfile::TempDir,
    core: EngineCore,
    importer: ChatImporter,
    requests: RequestLog,
}

fn engine(fixture: &Fixture) -> Engine {
    let data = tempfile::tempdir().unwrap();
    let requests: RequestLog = Arc::default();
    let registry = HarnessRegistry::new();
    for id in [HarnessId::ClaudeCode, HarnessId::Codex] {
        registry.register(Arc::new(RecordingHarness {
            id,
            requests: requests.clone(),
        }));
    }
    let core = EngineCore::assemble_with_profile(
        EngineProfile::local(data.path()).unwrap(),
        Arc::new(registry),
        HarnessId::ClaudeCode,
        None,
    )
    .unwrap();
    let importer = core.chat_import.with_roots(fixture.roots());
    Engine {
        _data: data,
        core,
        importer,
        requests,
    }
}

fn run(importer: &ChatImporter, source: ChatImportSource) -> ChatImportSummary {
    let mut items = Vec::new();
    let summary = importer.run(
        &ChatImportRequest {
            source,
            projects: None,
        },
        |item| items.push(item),
    );
    assert!(
        summary.errors.is_empty(),
        "import errors: {:?}",
        summary.errors
    );
    assert!(matches!(
        items.last(),
        Some(zeron_proto::chat_import::ChatImportProgress::Summary { .. })
    ));
    summary
}

fn state_of(preview: &ChatImportPreview, key: &str) -> Option<ChatImportState> {
    preview
        .projects
        .iter()
        .flat_map(|p| &p.chats)
        .find(|c| c.source_key == key)
        .map(|c| c.state)
}

fn entries(core: &EngineCore, key: &str) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(&chat_id_for(key))
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
}

fn texts(entry: &SessionMessageEntry) -> String {
    entry
        .parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|")
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn t3_threads_map_onto_spaces_and_chats() {
    let fixture = Fixture::new();
    let e = engine(&fixture);
    let preview = e.importer.preview(ChatImportSource::T3);
    assert!(preview.found);
    assert_eq!(preview.projects.len(), 1, "deleted project excluded");
    assert_eq!(preview.chat_count(), 3, "deleted thread excluded");
    assert_eq!(state_of(&preview, "t3:t-deleted"), None);
    assert_eq!(preview.count(ChatImportState::New), 3);

    let summary = run(&e.importer, ChatImportSource::T3);
    assert_eq!((summary.imported, summary.spaces_created), (3, 1));
    assert_eq!(summary.attachments, 1);

    let spaces = e.core.workspace.read_spaces().unwrap();
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0].path, fixture.repo);
    assert_eq!(spaces[0].name.as_deref(), Some("Repo A"));

    let claude = e
        .core
        .workspace
        .chat(&chat_id_for("t3:t-claude"))
        .unwrap()
        .expect("claude thread row");
    assert_eq!(claude.title.as_deref(), Some("Fix the login bug"));
    assert_eq!(claude.space_id.as_deref(), Some(spaces[0].id.as_str()));
    assert_eq!(claude.cwd.as_deref(), Some(fixture.repo.as_str()));
    assert_eq!(claude.branch.as_deref(), Some("feat/x"));
    assert_eq!(claude.harness_session_id.as_deref(), Some("claude-owned"));
    assert_eq!(
        claude.harness_session_cwd.as_deref(),
        Some(fixture.repo.as_str())
    );
    assert_eq!(claude.created_at.to_rfc3339(), "2026-09-10T10:00:10+00:00");
    assert!(!claude.unseen(), "history is not news");
    let config = claude.config.expect("config");
    assert_eq!(config.harness, HarnessId::ClaudeCode);
    assert_eq!(config.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(config.reasoning, Some(ReasoningLevel::High));
    // `runtime_mode` defaults to full-access in T3's schema.
    assert_eq!(config.access_mode, Some(AccessMode::FullAccess));
    assert!(
        e.core
            .workspace
            .sidebar_preferences_snapshot()
            .pinned_session_ids
            .contains(&chat_id_for("t3:t-claude")),
        "pinned thread pinned"
    );

    let codex = e
        .core
        .workspace
        .chat(&chat_id_for("t3:t-codex"))
        .unwrap()
        .unwrap();
    assert!(codex.archived);
    assert_eq!(codex.harness_session_id.as_deref(), Some("codex-owned"));
    let codex_config = codex.config.unwrap();
    assert_eq!(codex_config.harness, HarnessId::Codex);
    assert_eq!(codex_config.reasoning, Some(ReasoningLevel::XHigh));

    // Cursor's session can't be resumed by this app's Cursor driver.
    let cursor = e
        .core
        .workspace
        .chat(&chat_id_for("t3:t-cursor"))
        .unwrap()
        .unwrap();
    assert_eq!(cursor.config.unwrap().harness, HarnessId::Cursor);
    assert_eq!(cursor.harness_session_id, None);

    // Transcript: user/assistant alternate in source order; the two
    // assistant rows of a turn fold into one entry with the tool chip
    // between them where it ran.
    let log = entries(&e.core, "t3:t-claude");
    let roles: Vec<MessageRole> = log.iter().map(|e| e.role).collect();
    assert_eq!(
        roles,
        [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::User,
            MessageRole::Assistant
        ]
    );
    assert!(texts(&log[0]).starts_with("question 0"));
    assert!(
        texts(&log[0]).contains("Attached images"),
        "attachment trailer: {}",
        texts(&log[0])
    );
    let attached = texts(&log[0])
        .lines()
        .find_map(|l| l.strip_prefix("- ").map(str::to_string))
        .expect("attachment path");
    assert!(
        Path::new(&attached).is_file(),
        "attachment copied into uploads"
    );
    assert!(
        Path::new(&attached)
            .canonicalize()
            .unwrap()
            .starts_with(e.core.uploads.dir().canonicalize().unwrap()),
        "{attached} is in this profile's uploads"
    );
    let first_turn = &log[1];
    assert!(matches!(&first_turn.parts[0], MessagePart::Text { text, .. } if text == "looking 0"));
    assert!(matches!(
        &first_turn.parts[1],
        MessagePart::Tool { call: ToolCall::Exec { command }, resolved: true, is_error: false, .. }
            if command == "cargo test"
    ));
    assert!(matches!(&first_turn.parts[2], MessagePart::Text { text, .. } if text == "answer 0"));
    assert!(log.windows(2).all(|w| w[0].created_at <= w[1].created_at));
}

#[tokio::test]
async fn reimport_is_idempotent_and_updates_untouched_chats() {
    let fixture = Fixture::new();
    let e = engine(&fixture);
    run(&e.importer, ChatImportSource::T3);
    let chats_before = e.core.workspace.read_chats().unwrap().len();

    let again = run(&e.importer, ChatImportSource::T3);
    assert_eq!((again.imported, again.updated, again.skipped), (0, 0, 3));
    assert_eq!(e.core.workspace.read_chats().unwrap().len(), chats_before);
    assert_eq!(e.core.workspace.read_spaces().unwrap().len(), 1);
    assert_eq!(entries(&e.core, "t3:t-codex").len(), 2);

    // The owner kept talking in T3 Code: the newer turn is appended.
    fixture.t3_turn("t-codex", 1);
    let preview = e.importer.preview(ChatImportSource::T3);
    assert_eq!(
        state_of(&preview, "t3:t-codex"),
        Some(ChatImportState::Update)
    );
    let update = run(&e.importer, ChatImportSource::T3);
    assert_eq!((update.imported, update.updated), (0, 1));
    let log = entries(&e.core, "t3:t-codex");
    assert_eq!(log.len(), 4);
    assert!(texts(&log[2]).starts_with("question 1"));

    // Once continued here, the chat is never rewritten from the source.
    e.core
        .doc_host
        .open(&chat_id_for("t3:t-codex"))
        .unwrap()
        .write_user_message("native-1", "continued in godspeed", 1_800_000_000_000)
        .unwrap();
    fixture.t3_turn("t-codex", 2);
    let diverged = run(&e.importer, ChatImportSource::T3);
    assert_eq!(diverged.updated, 0);
    assert_eq!(entries(&e.core, "t3:t-codex").len(), 5);

    // A chat the user deleted stays deleted.
    e.core
        .workspace
        .delete_chat(&chat_id_for("t3:t-cursor"))
        .unwrap();
    let after_delete = run(&e.importer, ChatImportSource::T3);
    assert_eq!(after_delete.imported, 0);
    assert!(
        e.core
            .workspace
            .chat(&chat_id_for("t3:t-cursor"))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn sessions_land_once_across_sources() {
    let fixture = Fixture::new();
    let e = engine(&fixture);

    // Claude and Codex sessions a T3 thread resumes are T3's to import,
    // whichever source runs first.
    let claude = e.importer.preview(ChatImportSource::Claude);
    assert_eq!(
        state_of(&claude, "claude:claude-owned"),
        Some(ChatImportState::Elsewhere)
    );
    assert_eq!(
        state_of(&claude, "claude:claude-new"),
        Some(ChatImportState::New)
    );
    assert_eq!(
        claude.chat_count(),
        2,
        "subagent transcripts are not sessions"
    );
    let codex = e.importer.preview(ChatImportSource::Codex);
    assert_eq!(
        state_of(&codex, "codex:codex-owned"),
        Some(ChatImportState::Elsewhere)
    );
    assert_eq!(
        state_of(&codex, "codex:codex-new"),
        Some(ChatImportState::New)
    );
    assert_eq!(
        state_of(&codex, "codex:codex-child"),
        None,
        "subagent thread skipped"
    );

    let claude_run = run(&e.importer, ChatImportSource::Claude);
    let codex_run = run(&e.importer, ChatImportSource::Codex);
    let t3_run = run(&e.importer, ChatImportSource::T3);
    assert_eq!((claude_run.imported, claude_run.skipped), (1, 1));
    assert_eq!((codex_run.imported, codex_run.skipped), (1, 1));
    assert_eq!(t3_run.imported, 3);
    assert_eq!(e.core.workspace.read_chats().unwrap().len(), 5);
    assert_eq!(
        e.core.workspace.read_spaces().unwrap().len(),
        1,
        "one folder, one space"
    );

    let row = e
        .core
        .workspace
        .chat(&chat_id_for("claude:claude-new"))
        .unwrap()
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("Health check endpoint"));
    assert_eq!(row.branch.as_deref(), Some("main"));
    assert_eq!(
        row.config.as_ref().unwrap().model.as_deref(),
        Some("claude-sonnet-5-5")
    );
    let log = entries(&e.core, "claude:claude-new");
    assert_eq!(log.len(), 2, "meta line dropped, tool result folded");
    assert_eq!(texts(&log[0]), "add a health check");
    assert!(matches!(
        &log[1].parts[1],
        MessagePart::Tool { call: ToolCall::EditFile { path, old_string: None, new_string: None }, is_error: true, .. }
            if path == "src/main.rs"
    ));

    let codex_row = e
        .core
        .workspace
        .chat(&chat_id_for("codex:codex-new"))
        .unwrap()
        .unwrap();
    assert_eq!(codex_row.title.as_deref(), Some("List the files"));
    assert_eq!(codex_row.harness_session_id.as_deref(), Some("codex-new"));
    let codex_log = entries(&e.core, "codex:codex-new");
    assert_eq!(codex_log.len(), 2, "environment context dropped");
    assert_eq!(texts(&codex_log[0]), "list files");
    assert!(
        matches!(&codex_log[1].parts[0], MessagePart::Reasoning { text, .. } if text == "Plan it")
    );
    assert!(matches!(
        &codex_log[1].parts[1],
        MessagePart::Tool { call: ToolCall::Exec { command }, resolved: true, .. } if command == "ls"
    ));

    // A chat already resuming a session (here: native) claims it.
    let fresh = Fixture::new();
    let e2 = engine(&fresh);
    e2.core
        .workspace
        .create_space("s", &e2.core.device_id, &fresh.repo, None, true)
        .unwrap();
    e2.core
        .workspace
        .create_chat("native", Some("s"), None, None, None)
        .unwrap();
    e2.core
        .workspace
        .set_chat_harness_session("native", "claude-owned", &fresh.repo);
    let preview = e2.importer.preview(ChatImportSource::T3);
    assert_eq!(
        state_of(&preview, "t3:t-claude"),
        Some(ChatImportState::Elsewhere)
    );
    assert!(preview.projects[0].has_space);
    let summary = run(&e2.importer, ChatImportSource::T3);
    assert_eq!(
        (summary.imported, summary.spaces_created),
        (2, 0),
        "existing space reused"
    );
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

fn run_request(prompt: &str, cwd: &str) -> RunRequest {
    RunRequest {
        mcp: None,
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
        access: None,
    }
}

#[tokio::test]
async fn sending_in_an_imported_chat_resumes_the_original_session() {
    let fixture = Fixture::new();
    let e = engine(&fixture);
    run(&e.importer, ChatImportSource::Claude);
    run(&e.importer, ChatImportSource::Codex);
    run(&e.importer, ChatImportSource::T3);

    for (n, (key, harness, session)) in [
        ("claude:claude-new", HarnessId::ClaudeCode, "claude-new"),
        ("codex:codex-new", HarnessId::Codex, "codex-new"),
        ("t3:t-claude", HarnessId::ClaudeCode, "claude-owned"),
    ]
    .into_iter()
    .enumerate()
    {
        let chat_id = chat_id_for(key);
        e.core
            .doc_host
            .queue_command(
                &chat_id,
                SessionCommandPayload::Run {
                    request: run_request("keep going", &fixture.repo),
                    message_id: format!("m-{n}"),
                },
            )
            .unwrap();
        wait_for(|| e.requests.lock().unwrap().len() > n, "run request").await;
        let (ran_on, request) = e.requests.lock().unwrap()[n].clone();
        assert_eq!(request.resume.as_deref(), Some(session), "{key} resumes");
        assert_eq!(ran_on, harness, "{key} runs on its harness");
        // The new turn lands after the imported history.
        wait_for(
            || {
                entries(&e.core, key)
                    .last()
                    .is_some_and(|e| texts(e).contains("continued"))
            },
            "continued turn",
        )
        .await;
    }
}

#[tokio::test]
async fn sources_are_never_modified() {
    let fixture = Fixture::new();
    let before = fixture.source_hashes();
    assert!(
        before
            .keys()
            .any(|p| p.to_string_lossy().ends_with("state.sqlite-wal")),
        "fixture keeps T3's writes in its WAL"
    );
    let e = engine(&fixture);
    for source in ChatImportSource::ALL {
        let _ = e.importer.preview(source);
        run(&e.importer, source);
    }
    assert_eq!(fixture.source_hashes(), before);
}
