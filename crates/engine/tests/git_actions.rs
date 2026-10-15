//! Git write actions over the engine RPC (memory transport): commit, push
//! with upstream creation, fast-forward pull, divergence and non-fast-forward
//! errors, sync, init, and generated commit messages from the mock harness.
//! Every repository is a temp dir; "the remote" is a local bare repository.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::mock::MockHarness;
use zeron_proto::{
    AgentEvent, ChatConfig, GitBranchStatus, GitCommitResult, GitSyncResult, HarnessId,
    SandboxLevel,
};
use zeron_rpc::{RpcClient, methods};

/// A scripted mock reply ending in a completed `Done`.
fn replying(mut script: Vec<AgentEvent>) -> Vec<AgentEvent> {
    script.push(AgentEvent::Done {
        status: zeron_proto::DoneStatus::Completed,
        result: None,
        error: None,
        session_id: None,
    });
    script
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Repo-local identity and no signing or hooks, whatever the machine's
/// global configuration says.
fn configure(repo: &Path) {
    let hooks = repo.join(".no-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("core.hooksPath", hooks.to_str().unwrap()),
    ] {
        git(repo, &["config", key, value]);
    }
    // Keep the hooks dir out of every diff and commit.
    std::fs::write(repo.join(".git/info/exclude"), ".no-hooks/\n").unwrap();
}

/// A bare `remote.git` seeded with one commit on main, plus clones `names`.
fn remote_with_clones(root: &Path, names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let remote = root.join("remote.git");
    git(root, &["init", "--quiet", "--bare", "-b", "main", remote.to_str().unwrap()]);
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "--quiet", "-b", "main"]);
    configure(&seed);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "--quiet", "-m", "chore: seed"]);
    git(&seed, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let clones = names
        .iter()
        .map(|name| {
            let path = root.join(name);
            git(
                root,
                &["clone", "--quiet", remote.to_str().unwrap(), path.to_str().unwrap()],
            );
            configure(&path);
            path
        })
        .collect();
    (remote, clones)
}

fn assemble(dir: &Path, script: Vec<AgentEvent>) -> EngineCore {
    std::fs::create_dir_all(dir).unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script: replying(script) }));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

/// Register `path` as a space plus a chat on it (running the mock agent), so
/// the engine accepts it as a known checkout.
fn register(core: &EngineCore, id: &str, path: &Path) {
    let space = format!("space-{id}");
    core.workspace
        .create_space(&space, &core.device_id, path.to_str().unwrap(), None, true)
        .unwrap();
    core.workspace
        .create_chat(
            &format!("chat-{id}"),
            Some(&space),
            None,
            Some(ChatConfig {
                harness: HarnessId::Mock,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            Some(path.to_string_lossy().into_owned()),
        )
        .unwrap();
}

async fn call<T: serde::de::DeserializeOwned>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
) -> T {
    let value = client
        .call(method, params)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e}"));
    serde_json::from_value(value).expect("typed reply")
}

async fn call_err(client: &RpcClient, method: &str, params: serde_json::Value) -> String {
    match client.call(method, params).await {
        Ok(value) => panic!("{method} unexpectedly succeeded: {value}"),
        Err(error) => error.to_string(),
    }
}

fn cwd(path: &Path) -> serde_json::Value {
    serde_json::json!({ "cwd": path.to_string_lossy() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_push_pull_sync_and_divergence() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, clones) = remote_with_clones(tmp.path(), &["a", "b"]);
    let (a, b) = (&clones[0], &clones[1]);
    let core = assemble(&tmp.path().join("data"), vec![]);
    register(&core, "a", a);
    register(&core, "b", b);
    let client = zeron_rpc::memory_client(core.rpc_service());

    let status: GitBranchStatus = call(&client, methods::GIT_BRANCH_STATUS, cwd(a)).await;
    assert!(status.is_repo);
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.ahead, status.behind, status.changed_files), (0, 0, 0));
    assert_eq!(status.remote.as_deref(), Some("origin"));

    // Unknown folders are refused before git ever runs.
    let outside = tmp.path().join("seed");
    assert!(
        call_err(&client, methods::GIT_BRANCH_STATUS, cwd(&outside))
            .await
            .contains("not a known checkout")
    );

    // Nothing to commit.
    let error = call_err(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": a, "message": "noop" }),
    )
    .await;
    assert!(error.contains("Nothing to commit"), "{error}");

    // Commit everything; the agent credit trailer is stripped by default.
    std::fs::write(a.join("one.txt"), "1\n").unwrap();
    let committed: GitCommitResult = call(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({
            "cwd": a,
            "message": "feat: add one\n\nCo-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>\n",
        }),
    )
    .await;
    assert_eq!(committed.subject, "feat: add one");
    assert!(!committed.message.contains("Co-Authored-By"), "{}", committed.message);
    assert!(!committed.generated);
    assert_eq!(committed.status.ahead, 1);
    assert_eq!(git(a, &["log", "-1", "--format=%B"]), "feat: add one");

    // Commit a chosen file set only.
    std::fs::write(a.join("two.txt"), "2\n").unwrap();
    std::fs::write(a.join("three.txt"), "3\n").unwrap();
    let partial: GitCommitResult = call(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": a, "message": "feat: add two", "files": ["two.txt"] }),
    )
    .await;
    assert_eq!(partial.status.ahead, 2);
    assert_eq!(partial.status.changed_files, 1, "three.txt stays uncommitted");
    assert_eq!(git(a, &["show", "--name-only", "--format=", "HEAD"]), "two.txt");
    std::fs::remove_file(a.join("three.txt")).unwrap();

    // Push to the existing upstream.
    let pushed: GitSyncResult = call(&client, methods::GIT_PUSH, cwd(a)).await;
    assert_eq!(pushed.pushed, 2);
    assert!(!pushed.published_branch);
    assert_eq!(pushed.status.ahead, 0);
    assert_eq!(
        git(&remote, &["rev-parse", "main"]),
        git(a, &["rev-parse", "HEAD"])
    );

    // b fast-forwards.
    let pulled: GitSyncResult = call(&client, methods::GIT_PULL, cwd(b)).await;
    assert_eq!(pulled.pulled, 2);
    assert!(b.join("two.txt").exists());
    assert_eq!((pulled.status.ahead, pulled.status.behind), (0, 0));

    // Pull when already up to date is a no-op.
    let again: GitSyncResult = call(&client, methods::GIT_PULL, cwd(b)).await;
    assert_eq!(again.pulled, 0);

    // Divergence: both sides commit. Pull refuses to merge; push is rejected.
    std::fs::write(a.join("a-side.txt"), "a\n").unwrap();
    call::<GitCommitResult>(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": a, "message": "feat: a side" }),
    )
    .await;
    call::<GitSyncResult>(&client, methods::GIT_PUSH, cwd(a)).await;
    std::fs::write(b.join("b-side.txt"), "b\n").unwrap();
    call::<GitCommitResult>(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": b, "message": "feat: b side" }),
    )
    .await;
    let error = call_err(&client, methods::GIT_PULL, cwd(b)).await;
    assert!(error.contains("have diverged"), "{error}");
    assert!(error.contains("1 local and 1 remote"), "{error}");
    assert!(!b.join("a-side.txt").exists(), "nothing was merged");
    let error = call_err(&client, methods::GIT_PUSH, cwd(b)).await;
    assert!(error.contains("Push rejected"), "{error}");
    let error = call_err(&client, methods::GIT_SYNC, cwd(b)).await;
    assert!(error.contains("have diverged"), "{error}");

    // Sync: drop b's local commit; it is now behind only. Sync pulls, then a
    // new local commit makes the next sync push.
    git(b, &["reset", "--hard", "--quiet", "HEAD~1"]);
    let synced: GitSyncResult = call(&client, methods::GIT_SYNC, cwd(b)).await;
    assert_eq!((synced.pulled, synced.pushed), (1, 0));
    assert!(b.join("a-side.txt").exists());
    std::fs::write(b.join("b-again.txt"), "b\n").unwrap();
    call::<GitCommitResult>(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": b, "message": "feat: b again" }),
    )
    .await;
    let synced: GitSyncResult = call(&client, methods::GIT_SYNC, cwd(b)).await;
    assert_eq!((synced.pulled, synced.pushed), (0, 1));
    assert_eq!(
        git(&remote, &["rev-parse", "main"]),
        git(b, &["rev-parse", "HEAD"])
    );

    // A new branch has no upstream: pull explains, push publishes it.
    git(a, &["checkout", "--quiet", "-b", "feature/x"]);
    let error = call_err(&client, methods::GIT_PULL, cwd(a)).await;
    assert!(error.contains("no upstream"), "{error}");
    std::fs::write(a.join("x.txt"), "x\n").unwrap();
    call::<GitCommitResult>(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": a, "message": "feat: x" }),
    )
    .await;
    let status: GitBranchStatus = call(&client, methods::GIT_BRANCH_STATUS, cwd(a)).await;
    assert_eq!(status.upstream, None);
    assert_eq!(status.ahead, 1, "one unpublished commit");
    let published: GitSyncResult = call(&client, methods::GIT_PUSH, cwd(a)).await;
    assert!(published.published_branch);
    assert_eq!(published.status.upstream.as_deref(), Some("origin/feature/x"));
    assert_eq!(
        git(&remote, &["rev-parse", "feature/x"]),
        git(a, &["rev-parse", "HEAD"])
    );

    // A worktree-style branch that tracks origin/main is published under its
    // own name — never pushed onto main.
    let main_before = git(&remote, &["rev-parse", "main"]);
    git(a, &["checkout", "--quiet", "-b", "godspeed/topic", "--track", "origin/main"]);
    std::fs::write(a.join("topic.txt"), "t\n").unwrap();
    call::<GitCommitResult>(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": a, "message": "feat: topic" }),
    )
    .await;
    let status: GitBranchStatus = call(&client, methods::GIT_BRANCH_STATUS, cwd(a)).await;
    assert_eq!(status.upstream, None, "origin/main is not this branch's push upstream");
    let published: GitSyncResult = call(&client, methods::GIT_PUSH, cwd(a)).await;
    assert!(published.published_branch);
    assert_eq!(git(&remote, &["rev-parse", "main"]), main_before);
    assert_eq!(
        git(&remote, &["rev-parse", "godspeed/topic"]),
        git(a, &["rev-parse", "HEAD"])
    );

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_and_repository_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("plain");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("notes.txt"), "n\n").unwrap();
    let core = assemble(&tmp.path().join("data"), vec![]);
    register(&core, "plain", &folder);
    let client = zeron_rpc::memory_client(core.rpc_service());

    let status: GitBranchStatus = call(&client, methods::GIT_BRANCH_STATUS, cwd(&folder)).await;
    assert!(!status.is_repo);
    for method in [methods::GIT_COMMIT, methods::GIT_PUSH, methods::GIT_PULL] {
        let error = call_err(&client, method, cwd(&folder)).await;
        assert!(error.contains("Not a git repository"), "{method}: {error}");
    }

    let status: GitBranchStatus = call(
        &client,
        methods::GIT_INIT,
        serde_json::json!({ "path": folder }),
    )
    .await;
    assert!(status.is_repo);
    assert!(!status.has_commits);
    assert_eq!(status.remote, None);
    configure(&folder);
    let error = call_err(
        &client,
        methods::GIT_INIT,
        serde_json::json!({ "path": folder }),
    )
    .await;
    assert!(error.contains("already inside a git repository"), "{error}");

    // No commits yet: push and publish ask for a first commit.
    let error = call_err(&client, methods::GIT_PUSH, cwd(&folder)).await;
    assert!(error.contains("no commits yet"), "{error}");

    call::<GitCommitResult>(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": folder, "message": "Initial commit" }),
    )
    .await;
    // No remote: push explains how to publish.
    let error = call_err(&client, methods::GIT_PUSH, cwd(&folder)).await;
    assert!(error.contains("no remote"), "{error}");

    // Detached HEAD.
    git(&folder, &["checkout", "--quiet", "--detach"]);
    let error = call_err(&client, methods::GIT_PUSH, cwd(&folder)).await;
    assert!(error.contains("detached"), "{error}");

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_commit_messages_come_from_the_text_model() {
    let tmp = tempfile::tempdir().unwrap();
    let (_remote, clones) = remote_with_clones(tmp.path(), &["repo"]);
    let repo = &clones[0];
    // The mock agent stands in for the configured text model. Its reply is
    // fenced and carries an agent trailer, as real models sometimes do.
    let core = assemble(
        &tmp.path().join("data"),
        vec![AgentEvent::TextDelta {
            text: "```\nfeat: add greeting\n\nGreets the user on start.\n\nCo-Authored-By: Claude <noreply@anthropic.com>\n```".into(),
        }],
    );
    register(&core, "repo", repo);
    let client = zeron_rpc::memory_client(core.rpc_service());

    // Nothing changed: generation says so instead of inventing a message.
    let error = call_err(&client, methods::GENERATE_COMMIT_MESSAGE, cwd(repo)).await;
    assert!(error.contains("Nothing to commit"), "{error}");

    std::fs::write(repo.join("greet.txt"), "hi\n").unwrap();
    let generated: zeron_proto::GeneratedCommitMessage =
        call(&client, methods::GENERATE_COMMIT_MESSAGE, cwd(repo)).await;
    assert_eq!(generated.message, "feat: add greeting\n\nGreets the user on start.");
    // Previewing a message never stages anything.
    assert_eq!(git(repo, &["diff", "--cached", "--name-only"]), "");

    // An empty message commits with a generated one.
    let committed: GitCommitResult = call(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({ "cwd": repo, "message": "  " }),
    )
    .await;
    assert!(committed.generated);
    assert_eq!(committed.subject, "feat: add greeting");
    assert!(!committed.message.contains("Co-Authored-By"));

    // With "remove agent credits" off, a hand-written trailer is kept.
    core.registry
        .set_title_settings(zeron_engine::registry::TitleSettings {
            remove_agent_credits: false,
            ..Default::default()
        })
        .unwrap();
    std::fs::write(repo.join("more.txt"), "more\n").unwrap();
    let committed: GitCommitResult = call(
        &client,
        methods::GIT_COMMIT,
        serde_json::json!({
            "cwd": repo,
            "message": "feat: more\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
        }),
    )
    .await;
    assert!(committed.message.contains("Co-Authored-By: Claude"));

    // Branch names come from the same model, as `godspeed/<slug>`.
    let core_branch = assemble(
        &tmp.path().join("data-branch"),
        vec![AgentEvent::TextDelta {
            text: "Fix Login Redirect".into(),
        }],
    );
    register(&core_branch, "repo", repo);
    let client = zeron_rpc::memory_client(core_branch.rpc_service());
    let name: zeron_proto::GeneratedBranchName = call(
        &client,
        methods::GENERATE_BRANCH_NAME,
        serde_json::json!({ "cwd": repo, "description": "the login page loops" }),
    )
    .await;
    assert_eq!(name.name, "godspeed/fix-login-redirect");

    core.shutdown().await;
    core_branch.shutdown().await;
}
