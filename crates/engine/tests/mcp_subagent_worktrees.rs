//! MCP `create_chat` / `create_chats` with `worktree`, against a real engine
//! and a temporary repository: "new" (a set-up worktree off the parent's
//! branch or a named base), "parent" (the parent's checkout), an existing
//! worktree by path or branch, the refusals that come before anything is made
//! on disk, and the cleanup when the engine refuses the chat afterwards
//! (the conductor's sub-agent cap).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_mcp::{Godspeed, Origin, Tools};
use zeron_proto::{
    AgentEvent, CreateConductorParams, CreateWorktreeOutcome, DoneStatus, HarnessId, Model,
    ReasoningLevel, RunRequest, SteeringMode,
};
use zeron_rpc::{RpcClient, methods};

const SPACE: &str = "space-app";

struct Instant;

#[async_trait]
impl Harness for Instant {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }
    fn display_name(&self) -> &str {
        "Instant"
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
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Codex,
                model: "instant-1".into(),
                tools: vec![],
                cwd: request.cwd,
                session_id: "hs-1".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-1".into()),
            }),
        ])
        .boxed())
    }
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

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    core: EngineCore,
    client: RpcClient,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();

    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("device-id"), "dev-a").unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(Instant));
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Codex, None)
        .expect("engine assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            methods::SET_WORKTREE_SETTINGS,
            json!({ "useCustomDirectory": true, "customDirectory": root.join("worktrees") }),
        )
        .await
        .expect("worktrees stay in the temp dir");
    core.workspace
        .create_space(SPACE, &core.device_id, &repo.to_string_lossy(), None, true)
        .unwrap();
    client
        .call(
            methods::SET_WORKTREE_ENVIRONMENT,
            json!({ "spaceId": SPACE, "environment": { "copy": [".env"] } }),
        )
        .await
        .expect("environment saved");
    Fixture {
        _tmp: tmp,
        root,
        repo,
        core,
        client,
    }
}

impl Fixture {
    /// MCP tools speaking for `origin`.
    fn tools(&self, origin: &str) -> Tools {
        Tools::new(Arc::new(Godspeed::with_client(
            zeron_rpc::memory_client(self.core.rpc_service()),
            Origin {
                chat_id: Some(origin.into()),
                device_id: Some(self.core.device_id.clone()),
            },
        )))
    }

    /// A top-level chat in the project, working in `cwd`.
    fn lead(&self, id: &str, cwd: &Path, branch: Option<&str>) {
        self.core
            .workspace
            .create_chat(
                id,
                Some(SPACE),
                Some(&self.core.device_id),
                None,
                Some(cwd.to_string_lossy().into_owned()),
            )
            .unwrap();
        if let Some(branch) = branch {
            self.core.workspace.set_chat_branch(id, branch).unwrap();
        }
    }

    /// Wait until MCP sees chat `id` (registry rows reach the chat watch a
    /// moment after they are written).
    async fn visible(&self, id: &str) {
        let tools = self.tools(id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while tools.call("get_chat", json!({ "chat": id })).await.is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "chat {id} never became visible"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// A worktree made the way the app makes one.
    async fn app_worktree(&self) -> CreateWorktreeOutcome {
        self.client
            .call_as(
                methods::CREATE_WORKTREE,
                json!({ "repoPath": self.repo, "branch": "main", "spaceId": SPACE }),
            )
            .await
            .expect("create worktree")
    }

    /// Linked worktrees of the repository (the main checkout excluded).
    fn linked(&self) -> Vec<String> {
        git(&self.repo, &["worktree", "list", "--porcelain"])
            .split("\n\n")
            .skip(1)
            .filter_map(|block| {
                block
                    .lines()
                    .next()?
                    .strip_prefix("worktree ")
                    .map(str::to_owned)
            })
            .collect()
    }

    fn chat_ids(&self) -> Vec<String> {
        self.core
            .workspace
            .read_chats()
            .unwrap()
            .into_iter()
            .map(|chat| chat.id)
            .collect()
    }
}

fn same(a: &str, b: &str) -> bool {
    same_file::is_same_file(a, b).unwrap_or(false)
}

async fn create(tools: &Tools, args: Value) -> Result<Value, String> {
    tools.call("create_chat", args).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_starts_a_set_up_worktree_off_the_parents_branch() {
    let f = fixture().await;
    // The parent works in its own worktree, one commit ahead of main.
    let parent_tree = f.app_worktree().await.worktree;
    std::fs::write(Path::new(&parent_tree.path).join("lead.txt"), "lead\n").unwrap();
    git(Path::new(&parent_tree.path), &["add", "."]);
    git(Path::new(&parent_tree.path), &["commit", "-m", "lead work"]);
    let parent_head = git(Path::new(&parent_tree.path), &["rev-parse", "HEAD"]);
    f.lead(
        "lead",
        Path::new(&parent_tree.path),
        Some(&parent_tree.branch),
    );
    f.visible("lead").await;
    let tools = f.tools("lead");

    let created = create(
        &tools,
        json!({ "project": SPACE, "harness": "codex", "worktree": "new", "title": "Child" }),
    )
    .await
    .unwrap();
    assert_eq!(created["worktree"]["mode"], "new");
    let path = created["worktree"]["path"].as_str().unwrap().to_owned();
    let branch = created["worktree"]["branch"].as_str().unwrap().to_owned();
    assert_eq!(created["branch"], branch.as_str());
    assert!(branch.starts_with("godspeed/"), "{branch}");
    assert_ne!(branch, parent_tree.branch);
    assert!(!same(&path, &parent_tree.path));
    // Off the parent's branch, with the environment applied.
    assert_eq!(git(Path::new(&path), &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(
        std::fs::read_to_string(Path::new(&path).join(".env")).unwrap(),
        "SECRET=1\n"
    );
    let chat = f
        .core
        .workspace
        .chat(created["chatId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(same(chat.cwd.as_deref().unwrap(), &path));
    assert_eq!(chat.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(chat.parent_chat_id.as_deref(), Some("lead"));

    // A named base wins over the parent's branch.
    let based = create(
        &tools,
        json!({ "project": SPACE, "harness": "codex", "worktree": "new", "base": "main" }),
    )
    .await
    .unwrap();
    let based_path = based["worktree"]["path"].as_str().unwrap();
    assert_eq!(
        git(Path::new(based_path), &["rev-parse", "HEAD"]),
        git(&f.repo, &["rev-parse", "main"])
    );

    // Batch: each request gets a worktree of its own; project defaults to
    // the parent's.
    let batch = tools
        .call(
            "create_chats",
            json!({ "requests": [
                { "harness": "codex", "worktree": "new" },
                { "harness": "codex", "worktree": "new" }
            ] }),
        )
        .await
        .unwrap();
    let paths: Vec<String> = batch["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["result"]["worktree"]["path"]
                .as_str()
                .unwrap_or_else(|| panic!("{r}"))
                .to_owned()
        })
        .collect();
    assert_eq!(paths.len(), 2);
    assert!(!same(&paths[0], &paths[1]));
    assert_eq!(f.linked().len(), 5, "parent + 2 + 2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_shares_the_parents_checkout_and_existing_resolves_path_or_branch() {
    let f = fixture().await;
    let parent_tree = f.app_worktree().await.worktree;
    let other = f.app_worktree().await.worktree;
    f.lead(
        "lead",
        Path::new(&parent_tree.path),
        Some(&parent_tree.branch),
    );
    f.visible("lead").await;
    let tools = f.tools("lead");

    for args in [
        json!({ "project": SPACE, "harness": "codex" }),
        json!({ "project": SPACE, "harness": "codex", "worktree": "parent" }),
    ] {
        let created = create(&tools, args).await.unwrap();
        assert_eq!(created["worktree"]["mode"], "parent");
        let chat = f
            .core
            .workspace
            .chat(created["chatId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(same(chat.cwd.as_deref().unwrap(), &parent_tree.path));
        assert_eq!(chat.branch.as_deref(), Some(parent_tree.branch.as_str()));
    }

    // An existing worktree by folder (a subfolder works too) or by branch.
    std::fs::create_dir_all(Path::new(&other.path).join("src")).unwrap();
    let by_path = Path::new(&other.path)
        .join("src")
        .to_string_lossy()
        .into_owned();
    for target in [by_path, other.branch.clone()] {
        let created = create(
            &tools,
            json!({ "project": SPACE, "harness": "codex", "worktree": target }),
        )
        .await
        .unwrap();
        assert_eq!(created["worktree"]["mode"], "existing", "{created}");
        assert!(same(
            created["worktree"]["path"].as_str().unwrap(),
            &other.path
        ));
        assert_eq!(created["branch"], other.branch.as_str());
    }

    let before = f.chat_ids().len();
    let err = create(
        &tools,
        json!({ "project": SPACE, "harness": "codex", "worktree": "no-such-branch" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("No checkout"), "{err}");
    assert_eq!(f.chat_ids().len(), before, "no chat for a bad target");
    assert_eq!(f.linked().len(), 2, "nothing created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_come_before_anything_is_made() {
    let f = fixture().await;
    f.lead("lead", &f.repo, None);
    f.visible("lead").await;
    let tools = f.tools("lead");
    let cases = [
        (
            json!({ "project": SPACE, "harness": "codex", "worktree": "new", "cwd": "/tmp" }),
            "can't be combined",
        ),
        (
            json!({ "project": SPACE, "harness": "codex", "base": "main" }),
            "base only applies",
        ),
        (
            json!({ "project": SPACE, "harness": "codex", "worktree": "new", "access_mode": "supervised", "sandbox": "read-only" }),
            "read-only",
        ),
    ];
    for (args, needle) in cases {
        let err = create(&tools, args).await.unwrap_err();
        assert!(err.contains(needle), "{needle}: {err}");
    }
    // Without a project anywhere there is no repository to branch.
    f.core
        .workspace
        .create_chat("loose", None, Some(&f.core.device_id), None, None)
        .unwrap();
    f.visible("loose").await;
    let orphan_origin = f.tools("loose");
    let err = create(
        &orphan_origin,
        json!({ "harness": "codex", "worktree": "new" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("needs a project"), "{err}");
    assert!(f.linked().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conductor_caps_hold_and_a_refused_chat_leaves_no_worktree() {
    let f = fixture().await;
    let conductor = f
        .core
        .workspace
        .create_conductor(CreateConductorParams {
            chat_id: "main".into(),
            id: String::new(),
            folder: f.root.to_string_lossy().into_owned(),
            name: Some("Acme".into()),
            purpose: None,
            device_id: None,
            config: None,
        })
        .unwrap();
    assert!(
        conductor.space_ids.contains(&SPACE.to_string()),
        "{conductor:?}"
    );
    let tools = f.tools("main");

    // Depth: a member at the deepest level is refused before any checkout.
    let mut parent = "main".to_string();
    for depth in 1..=zeron_proto::MAX_CONDUCTOR_DEPTH {
        let id = format!("depth-{depth}");
        f.core
            .workspace
            .create_chat_with_parent(
                &id,
                Some(SPACE),
                Some(&f.core.device_id),
                None,
                None,
                Some(parent.clone()),
            )
            .unwrap();
        parent = id;
    }
    f.visible(&parent).await;
    let err = create(
        &f.tools(&parent),
        json!({ "project": SPACE, "harness": "codex", "worktree": "new" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("deepest level"), "{err}");
    assert!(f.linked().is_empty());

    // Fan-out: the conductor already runs its maximum of sub-agents (the
    // depth chain plus fresh ones count as running), so the engine refuses
    // the chat after the worktree was made, and the worktree goes again.
    let running = zeron_proto::MAX_CONDUCTOR_DEPTH as usize;
    for n in running..zeron_proto::conductor::MAX_RUNNING_SUB_AGENTS {
        f.core
            .workspace
            .create_chat_with_parent(
                &format!("busy-{n}"),
                Some(SPACE),
                Some(&f.core.device_id),
                None,
                None,
                Some("main".into()),
            )
            .unwrap();
    }
    f.visible(&format!(
        "busy-{}",
        zeron_proto::conductor::MAX_RUNNING_SUB_AGENTS - 1
    ))
    .await;
    let before = f.chat_ids().len();
    let err = create(
        &tools,
        json!({ "project": SPACE, "harness": "codex", "worktree": "new" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("already runs"), "{err}");
    assert!(err.contains("worktree was removed"), "{err}");
    assert!(f.linked().is_empty(), "{:?}", f.linked());
    assert_eq!(f.chat_ids().len(), before);
    let branches = git(&f.repo, &["branch", "--list", "godspeed/*"]);
    assert!(branches.is_empty(), "{branches}");
}
