//! Worktree environments end to end over the RPC surface: files copied into
//! new worktrees, unique persisted port blocks, templated setup in the
//! "(setup)" terminal and teardown on delete, the branch template, the
//! variables every process in the worktree sees (terminals, Actions, agent
//! runs), and sleep/wake. Everything runs in temporary repositories.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{MessageRole, MessageStatus, SessionCommandPayload};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, CreateWorktreeOutcome, DoneStatus, HarnessId, Model, ProjectActionDraft,
    ProjectActionIcon, ProjectActionRun, ReasoningLevel, RepoRef, RunRequest, SandboxLevel,
    SteeringMode, TerminalEvent, TerminalSession, WakeWorktreeOutcome, WorktreeEnvironmentSnapshot,
    WorktreeEnvironmentStatus, WorktreeSetupState, WorktreeSpec,
};
use zeron_rpc::{RpcClient, methods};

const SPACE: &str = "space-env";

/// Completes every turn at once and records the environment it ran with.
struct RecordingHarness {
    runs: Arc<Mutex<Vec<RunRequest>>>,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recorder"
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
        self.runs.lock().unwrap().push(request.clone());
        let events: Vec<Result<AgentEvent, HarnessError>> = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess-env".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta { text: "ok".into() }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-env".into()),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    core: EngineCore,
    client: RpcClient,
    runs: Arc<Mutex<Vec<RunRequest>>>,
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn assemble(data: &Path, runs: &Arc<Mutex<Vec<RunRequest>>>) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { runs: runs.clone() }));
    EngineCore::assemble(data, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join(".gitignore"), ".env*\nnode_modules\n").unwrap();
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(repo.join(".env.local"), "LOCAL=1\n").unwrap();

    let runs = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(&root.join("data"), &runs);
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            methods::SET_WORKTREE_SETTINGS,
            serde_json::json!({
                "useCustomDirectory": true,
                "customDirectory": root.join("worktrees"),
            }),
        )
        .await
        .expect("worktrees stay in the temp dir");
    core.workspace
        .create_space(SPACE, &core.device_id, &repo.to_string_lossy(), None, true)
        .expect("project");
    Fixture {
        _tmp: tmp,
        root,
        repo,
        core,
        client,
        runs,
    }
}

impl Fixture {
    async fn set_environment(&self, environment: serde_json::Value) -> WorktreeEnvironmentSnapshot {
        self.client
            .call_as(
                methods::SET_WORKTREE_ENVIRONMENT,
                serde_json::json!({ "spaceId": SPACE, "environment": environment }),
            )
            .await
            .expect("save environment")
    }

    async fn create(&self) -> CreateWorktreeOutcome {
        self.client
            .call_as(
                methods::CREATE_WORKTREE,
                serde_json::json!({
                    "repoPath": self.repo,
                    "branch": "main",
                    "spaceId": SPACE,
                }),
            )
            .await
            .expect("create worktree")
    }

    async fn status(&self, path: &str) -> Option<WorktreeEnvironmentStatus> {
        self.client
            .call_as(
                methods::GET_WORKTREE_STATUS,
                serde_json::json!({ "path": path }),
            )
            .await
            .expect("status")
    }

    async fn terminal_output(&self, terminal: &TerminalSession, needle: &str) -> String {
        let mut rx = self
            .core
            .terminals
            .subscribe(&terminal.id, None)
            .expect("subscribe");
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let text = String::from_utf8_lossy(&output).into_owned();
            if text.contains(needle) {
                return text;
            }
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(TerminalEvent::Data { data, .. })) => {
                    output.extend(BASE64.decode(data).unwrap());
                }
                Ok(Some(TerminalEvent::Exit { .. })) | Ok(None) | Err(_) => {
                    panic!("terminal ended before {needle:?}: {text}")
                }
            }
        }
    }
}

async fn wait_for(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_status(
    fixture: &Fixture,
    path: &str,
    done: impl Fn(&WorktreeEnvironmentStatus) -> bool,
) -> WorktreeEnvironmentStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = fixture.status(path).await.expect("worktree has a status");
        if done(&status) {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "status never settled: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    )
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_copies_files_reserves_ports_and_runs_templated_setup_and_teardown() {
    let fixture = fixture().await;
    let teardown_log = fixture.root.join("teardown.log");
    let snapshot = fixture
        .set_environment(serde_json::json!({
            "copy": [".env*"],
            "ports": { "count": 3, "names": { "web": 0, "api": 1 } },
            "setup": [
                "printf '%s|%s|%s|%s|%s' '{{slug}}' '{{port.web}}' '{{branch}}' \"$PORT_API\" '{{project}}' > setup.out"
            ],
            "teardown": [format!(
                "printf '%s:%s:%s' '{{{{slug}}}}' \"$GODSPEED_PORT\" \"$PWD\" >> '{}'",
                teardown_log.display()
            )],
            "branch": "wt/{{project}}-{{slug}}"
        }))
        .await;
    assert_eq!(snapshot.environment.ports.as_ref().unwrap().count, 3);

    let first = fixture.create().await;
    let path = PathBuf::from(&first.worktree.path);
    let slug = first.worktree.name.clone();
    assert_eq!(first.worktree.branch, format!("wt/app-{slug}"));
    assert_eq!(
        std::fs::read_to_string(path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(
        std::fs::read_to_string(path.join(".env.local")).unwrap(),
        "LOCAL=1\n"
    );
    let setup = first.setup_action.as_ref().expect("setup terminal started");
    assert_eq!(setup.action_name, "Environment");

    let status = wait_for_status(&fixture, &first.worktree.path, |status| {
        status.setup.as_ref() != Some(&WorktreeSetupState::Running) && status.setup.is_some()
    })
    .await;
    assert_eq!(status.setup, Some(WorktreeSetupState::Succeeded));
    let ports = status.ports.clone().expect("ports reserved");
    assert_eq!(ports.count, 3);
    let written = std::fs::read_to_string(path.join("setup.out")).unwrap();
    assert_eq!(
        written,
        format!("{slug}|{}|wt/app-{slug}|{}|app", ports.base, ports.base + 1)
    );

    let second = fixture.create().await;
    let second_ports = fixture
        .status(&second.worktree.path)
        .await
        .unwrap()
        .ports
        .unwrap();
    assert!(
        second_ports.base > ports.last() || second_ports.last() < ports.base,
        "{ports:?} and {second_ports:?} overlap"
    );

    let refs: Vec<RepoRef> = fixture
        .client
        .call_as(
            methods::LIST_REFS,
            serde_json::json!({ "repoPath": fixture.repo }),
        )
        .await
        .unwrap();
    let listed = refs
        .iter()
        .find(|r| r.name == first.worktree.branch)
        .expect("worktree branch listed");
    assert_eq!(listed.worktree_ports.as_ref(), Some(&ports));
    assert!(!listed.worktree_sleeping);

    // Reservations survive an engine restart.
    let Fixture {
        _tmp,
        root,
        repo,
        core,
        client,
        runs,
    } = fixture;
    drop(client);
    core.shutdown().await;
    drop(core);
    let restarted = assemble(&root.join("data"), &runs);
    let client = zeron_rpc::memory_client(restarted.rpc_service());
    let after: Option<WorktreeEnvironmentStatus> = client
        .call_as(
            methods::GET_WORKTREE_STATUS,
            serde_json::json!({ "path": first.worktree.path }),
        )
        .await
        .unwrap();
    assert_eq!(after.unwrap().ports, Some(ports.clone()));

    let deleted = client
        .call(
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": repo,
                "worktreePath": first.worktree.path,
                "force": true,
            }),
        )
        .await
        .unwrap();
    assert!(deleted["teardownError"].is_null(), "{deleted}");
    assert!(!path.exists());
    assert_eq!(
        std::fs::read_to_string(&teardown_log).unwrap(),
        format!("{slug}:{}:{}", ports.base, path.display())
    );
    let gone: Option<WorktreeEnvironmentStatus> = client
        .call_as(
            methods::GET_WORKTREE_STATUS,
            serde_json::json!({ "path": first.worktree.path }),
        )
        .await
        .unwrap();
    assert!(gone.is_none(), "the record and its ports are released");
    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_are_explained_and_never_leave_a_half_made_worktree() {
    let fixture = fixture().await;

    // A failing setup step keeps the worktree and says which step failed.
    fixture
        .set_environment(serde_json::json!({
            "setup": ["true", "echo broken >&2; exit 7", "touch never-runs"]
        }))
        .await;
    let created = fixture.create().await;
    let status = wait_for_status(&fixture, &created.worktree.path, |status| {
        matches!(status.setup, Some(WorktreeSetupState::Failed { .. }))
    })
    .await;
    assert_eq!(
        status.setup,
        Some(WorktreeSetupState::Failed {
            step: 2,
            exit_code: 7,
            command: "echo broken >&2; exit 7".into(),
        })
    );
    let terminal = created.setup_action.unwrap().terminal;
    let output = fixture
        .terminal_output(&terminal, "step 2 of 3 failed")
        .await;
    assert!(output.contains("exit 7"), "{output}");
    let path = PathBuf::from(&created.worktree.path);
    assert!(path.is_dir());
    assert!(!path.join("never-runs").exists());

    // A failing teardown is reported, and the worktree still goes.
    fixture
        .set_environment(serde_json::json!({ "teardown": ["echo db busy >&2; exit 3"] }))
        .await;
    let deleted = fixture
        .client
        .call(
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": fixture.repo,
                "worktreePath": created.worktree.path,
                "force": true,
            }),
        )
        .await
        .unwrap();
    let teardown = deleted["teardownError"].as_str().expect("teardown error");
    assert!(
        teardown.contains("exit 3") && teardown.contains("db busy"),
        "{teardown}"
    );
    assert!(!path.exists());

    // Invalid templates are refused when saved.
    let refused = fixture
        .client
        .call(
            methods::SET_WORKTREE_ENVIRONMENT,
            serde_json::json!({
                "spaceId": SPACE,
                "environment": { "setup": ["createdb app_{{port}}"] },
            }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("port block"), "{refused}");

    // A copy that fails removes the new worktree and says so.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let secret = fixture.repo.join(".env");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        fixture
            .set_environment(serde_json::json!({ "copy": [".env"], "ports": 2 }))
            .await;
        let before = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
        let error = fixture
            .client
            .call(
                methods::CREATE_WORKTREE,
                serde_json::json!({
                    "repoPath": fixture.repo,
                    "branch": "main",
                    "spaceId": SPACE,
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            error.contains("was removed") && error.contains(".env"),
            "{error}"
        );
        assert_eq!(
            git(&fixture.repo, &["worktree", "list", "--porcelain"]),
            before
        );
        let leftovers: Vec<_> = std::fs::read_dir(fixture.root.join("worktrees/app"))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
    fixture.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_variables_reach_terminals_actions_and_agent_runs() {
    let fixture = fixture().await;
    fixture
        .set_environment(serde_json::json!({
            "ports": { "count": 2, "names": { "web": 1 } },
        }))
        .await;

    // An agent run that materializes its worktree gets the variables.
    let chat = "chat-env-run";
    fixture
        .client
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": chat, "deviceId": fixture.core.device_id }),
        )
        .await
        .unwrap();
    fixture
        .core
        .workspace
        .rename_chat(chat, "Pre-titled")
        .unwrap();
    let request = RunRequest {
        instance: None,
        mcp: None,
        prompt: "go".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: fixture.repo.to_string_lossy().into_owned(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
        worktree: Some(WorktreeSpec {
            repo_path: fixture.repo.to_string_lossy().into_owned(),
            base: "main".into(),
            space_id: Some(SPACE.into()),
        }),
        access: None,
        env: Default::default(),
    };
    fixture
        .core
        .doc_host
        .queue_command(
            chat,
            SessionCommandPayload::Run {
                request,
                message_id: "msg-env-1".into(),
            },
        )
        .unwrap();
    wait_for("the agent run", || !fixture.runs.lock().unwrap().is_empty()).await;
    let run = fixture.runs.lock().unwrap()[0].clone();
    let worktree = PathBuf::from(&run.cwd);
    assert_ne!(worktree, fixture.repo);
    let status = fixture
        .status(&run.cwd)
        .await
        .expect("run worktree has an environment");
    let ports = status.ports.unwrap();
    assert_eq!(run.env["GODSPEED_PORT"], ports.base.to_string());
    assert_eq!(run.env["GODSPEED_PORT_1"], (ports.base + 1).to_string());
    assert_eq!(run.env["PORT_WEB"], (ports.base + 1).to_string());
    assert_eq!(run.env["GODSPEED_WORKTREE_PATH"], run.cwd);
    wait_for("the turn to finish", || {
        fixture
            .core
            .doc_host
            .open(chat)
            .ok()
            .and_then(|handle| handle.doc().read_entries().ok())
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.role == MessageRole::Assistant
                        && entry.status == Some(MessageStatus::Complete)
                })
            })
    })
    .await;

    // A terminal opened in the worktree sees them.
    let terminal: TerminalSession = fixture
        .client
        .call_as(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "chatId": chat, "cols": 80, "rows": 24, "cwd": run.cwd }),
        )
        .await
        .unwrap();
    fixture
        .core
        .terminals
        .write_bytes(&terminal.id, b"printf 'P=<%s>\\n' \"$GODSPEED_PORT\"\r")
        .unwrap();
    fixture
        .terminal_output(&terminal, &format!("P=<{}>", ports.base))
        .await;

    // So does a project Action run in a chat on that worktree.
    fixture
        .core
        .workspace
        .create_chat(
            "chat-env-action",
            Some(SPACE),
            None,
            None,
            Some(run.cwd.clone()),
        )
        .unwrap();
    let saved = fixture
        .core
        .project_actions
        .upsert(
            SPACE,
            &fixture.repo,
            None,
            ProjectActionDraft {
                name: "Dev".into(),
                command: "printf 'WEB=<%s>\\n' \"$PORT_WEB\"".into(),
                icon: ProjectActionIcon::Play,
                run_on_worktree_create: false,
            },
        )
        .unwrap();
    let action: ProjectActionRun = fixture
        .client
        .call_as(
            methods::RUN_PROJECT_ACTION,
            serde_json::json!({
                "spaceId": SPACE,
                "chatId": "chat-env-action",
                "actionId": saved.actions[0].id,
                "cols": 80,
                "rows": 24,
            }),
        )
        .await
        .unwrap();
    fixture
        .terminal_output(&action.terminal, &format!("WEB=<{}>", ports.base + 1))
        .await;

    // Outside any worktree nothing is added.
    let plain: TerminalSession = fixture
        .client
        .call_as(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "chatId": chat, "cols": 80, "rows": 24, "cwd": fixture.repo }),
        )
        .await
        .unwrap();
    fixture
        .core
        .terminals
        .write_bytes(&plain.id, b"printf 'P=<%s>\\n' \"${GODSPEED_PORT-none}\"\r")
        .unwrap();
    fixture.terminal_output(&plain, "P=<none>").await;
    fixture.core.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleep_stops_dev_servers_and_frees_ports_and_wake_restores() {
    let fixture = fixture().await;
    fixture
        .set_environment(serde_json::json!({
            "ports": 1,
            "wake": ["printf '%s' \"$GODSPEED_PORT\" > woke.txt"],
        }))
        .await;
    let created = fixture.create().await;
    let path = created.worktree.path.clone();
    let port = fixture.status(&path).await.unwrap().ports.unwrap().base;

    // A dev server in a terminal of the worktree, in the background so it
    // sits in its own process group like a real job.
    let terminal: TerminalSession = fixture
        .client
        .call_as(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "chatId": "dev", "cols": 80, "rows": 24, "cwd": path }),
        )
        .await
        .unwrap();
    fixture
        .core
        .terminals
        .write_bytes(
            &terminal.id,
            b"python3 -m http.server \"$GODSPEED_PORT\" --bind 127.0.0.1 >/dev/null 2>&1 &\r",
        )
        .unwrap();
    wait_for("the dev server", || listening(port)).await;

    let asleep: WorktreeEnvironmentStatus = fixture
        .client
        .call_as(methods::SLEEP_WORKTREE, serde_json::json!({ "path": path }))
        .await
        .unwrap();
    assert!(asleep.sleeping);
    wait_for("the port to free up", || !listening(port)).await;
    assert!(
        fixture
            .core
            .terminals
            .subscribe(&terminal.id, None)
            .is_err()
    );
    let refs: Vec<RepoRef> = fixture
        .client
        .call_as(
            methods::LIST_REFS,
            serde_json::json!({ "repoPath": fixture.repo }),
        )
        .await
        .unwrap();
    assert!(refs.iter().any(|r| r.worktree_sleeping));

    let woke: Option<WakeWorktreeOutcome> = fixture
        .client
        .call_as(methods::WAKE_WORKTREE, serde_json::json!({ "path": path }))
        .await
        .unwrap();
    let woke = woke.expect("worktree known");
    assert!(!woke.status.sleeping);
    assert_eq!(
        woke.status.ports.as_ref().map(|ports| ports.base),
        Some(port)
    );
    assert_eq!(woke.wake_action.as_ref().unwrap().action_name, "Wake");
    let marker = PathBuf::from(&path).join("woke.txt");
    wait_for("the wake command", || {
        std::fs::read_to_string(&marker).is_ok_and(|text| text == port.to_string())
    })
    .await;

    // Waking an awake worktree runs nothing again.
    std::fs::remove_file(&marker).unwrap();
    let again: Option<WakeWorktreeOutcome> = fixture
        .client
        .call_as(methods::WAKE_WORKTREE, serde_json::json!({ "path": path }))
        .await
        .unwrap();
    assert!(again.unwrap().wake_action.is_none());

    // Sending to a chat in a sleeping worktree wakes it.
    fixture
        .client
        .call_as::<WorktreeEnvironmentStatus>(
            methods::SLEEP_WORKTREE,
            serde_json::json!({ "path": path }),
        )
        .await
        .unwrap();
    let chat = "chat-sleepy";
    fixture
        .core
        .workspace
        .create_chat(chat, Some(SPACE), None, None, Some(path.clone()))
        .unwrap();
    fixture
        .core
        .workspace
        .rename_chat(chat, "Pre-titled")
        .unwrap();
    let request = RunRequest {
        instance: None,
        mcp: None,
        prompt: "wake up".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: path.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
        worktree: None,
        access: None,
        env: BTreeMap::new(),
    };
    fixture
        .core
        .doc_host
        .queue_command(
            chat,
            SessionCommandPayload::Run {
                request,
                message_id: "msg-sleepy".into(),
            },
        )
        .unwrap();
    wait_for("the wake on send", || marker.is_file()).await;
    assert!(!fixture.status(&path).await.unwrap().sleeping);

    // The main checkout can't sleep.
    assert!(
        fixture
            .client
            .call(
                methods::SLEEP_WORKTREE,
                serde_json::json!({ "path": fixture.repo })
            )
            .await
            .is_err()
    );
    fixture.core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn godspeed_json_env_is_importable_and_old_files_still_work() {
    let fixture = fixture().await;
    std::fs::write(
        fixture.repo.join("godspeed.json"),
        r#"{"actions":[{"name":"Dev","command":"pnpm dev","icon":"play"}]}"#,
    )
    .unwrap();
    let snapshot: WorktreeEnvironmentSnapshot = fixture
        .client
        .call_as(
            methods::GET_WORKTREE_ENVIRONMENT,
            serde_json::json!({ "spaceId": SPACE }),
        )
        .await
        .unwrap();
    assert!(snapshot.importable.is_none() && snapshot.project_file_issue.is_none());
    let actions: zeron_proto::ProjectActionsSnapshot = fixture
        .client
        .call_as(
            methods::LIST_PROJECT_ACTIONS,
            serde_json::json!({ "spaceId": SPACE }),
        )
        .await
        .unwrap();
    assert_eq!(actions.importable_actions.len(), 1);

    std::fs::write(
        fixture.repo.join("godspeed.json"),
        r#"{
            "actions": [{"name":"Dev","command":"pnpm dev","icon":"play"}],
            "env": {
                "copy": [".env*", {"glob": "node_modules", "symlink": true}],
                "ports": {"count": 4, "names": {"web": 0}},
                "setup": "createdb app_{{slug}} -T app_dev",
                "teardown": "dropdb --if-exists app_{{slug}}",
                "branch": "{{prefix}}{{slug}}"
            }
        }"#,
    )
    .unwrap();
    let snapshot: WorktreeEnvironmentSnapshot = fixture
        .client
        .call_as(
            methods::GET_WORKTREE_ENVIRONMENT,
            serde_json::json!({ "spaceId": SPACE }),
        )
        .await
        .unwrap();
    let importable = snapshot.importable.expect("env offered for import");
    assert!(
        snapshot.environment.is_empty(),
        "nothing applies until imported"
    );
    assert_eq!(importable.setup, ["createdb app_{{slug}} -T app_dev"]);
    assert!(importable.copy[1].symlink);

    let saved = fixture
        .set_environment(serde_json::to_value(&importable).unwrap())
        .await;
    assert_eq!(saved.environment, importable);
    assert!(saved.importable.is_none(), "already imported");

    std::fs::write(
        fixture.repo.join("godspeed.json"),
        r#"{"env": {"ports": 0}}"#,
    )
    .unwrap();
    let invalid: WorktreeEnvironmentSnapshot = fixture
        .client
        .call_as(
            methods::GET_WORKTREE_ENVIRONMENT,
            serde_json::json!({ "spaceId": SPACE }),
        )
        .await
        .unwrap();
    assert!(invalid.project_file_issue.unwrap().contains("ports"));
    assert_eq!(
        invalid.environment, importable,
        "the saved environment is kept"
    );
    fixture.core.shutdown().await;
}
