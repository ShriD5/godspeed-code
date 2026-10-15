//! Pull request creation and repository publishing through a fake `gh` on
//! PATH that asserts its arguments and standard input.
//!
//! This file must stay a single test: it mutates the process PATH, so it
//! needs its own test binary with no parallel siblings.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::mock::MockHarness;
use zeron_proto::{
    AgentEvent, ChatConfig, CreatedPullRequest, GeneratedPullRequest, HarnessId,
    PublishedRepository, SandboxLevel,
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

fn configure(repo: &Path) {
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(repo, &["config", key, value]);
    }
}

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
                access_mode: None,
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

/// The fake records each invocation (args, one per line, then stdin) and
/// fails loudly on anything it was not told to expect.
const FAKE_GH: &str = r##"#!/bin/sh
log="$GH_FAKE_LOG"
n=$(ls "$log" 2>/dev/null | grep -c '\.args$')
out="$log/$n"
: > "$out.args"
for arg in "$@"; do printf '%s\n' "$arg" >> "$out.args"; done
if [ "$GH_PROMPT_DISABLED" != "1" ]; then
  echo "interactive prompts were not disabled" >&2
  exit 3
fi
case "$1 $2" in
  "pr create")
    cat > "$out.stdin"
    for arg in "$@"; do
      if [ "$arg" = "dup-branch" ]; then
        echo 'a pull request for branch "dup-branch" into branch "main" already exists:' >&2
        echo 'https://github.com/acme/widgets/pull/7' >&2
        exit 1
      fi
    done
    echo "https://github.com/acme/widgets/pull/42"
    ;;
  "repo create")
    # Play gh's part: add the remote and push, as --source/--push would.
    git remote add origin "$GH_FAKE_REMOTE" && git push --quiet -u origin HEAD >&2 || exit 1
    echo "https://github.com/acme/$3"
    ;;
  "repo view")
    echo '{"defaultBranchRef":{"name":"main"}}'
    ;;
  *)
    echo "unexpected gh invocation: $*" >&2
    exit 2
    ;;
esac
"##;

fn invocations(log: &Path) -> Vec<(Vec<String>, Option<String>)> {
    let mut out = Vec::new();
    for n in 0.. {
        let args = log.join(format!("{n}.args"));
        let Ok(args) = std::fs::read_to_string(&args) else {
            break;
        };
        let stdin = std::fs::read_to_string(log.join(format!("{n}.stdin"))).ok();
        out.push((args.lines().map(str::to_string).collect(), stdin));
    }
    out
}

fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|ix| args.get(ix + 1))
        .map(String::as_str)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_requests_and_publishing_go_through_gh() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("gh"), FAKE_GH).unwrap();
    std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = root.join("gh-log");
    std::fs::create_dir_all(&log).unwrap();
    let publish_remote = root.join("published.git");
    git(root, &["init", "--quiet", "--bare", "-b", "main", publish_remote.to_str().unwrap()]);
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin.clone()];
        paths.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("GH_FAKE_LOG", &log);
        std::env::set_var("GH_FAKE_REMOTE", &publish_remote);
        std::env::set_var("GODSPEED_NO_LOGIN_SHELL", "1");
    }

    // A GitHub-looking repository whose "origin" is really a local bare repo
    // (pushes work offline; `gh` is the fake).
    let remote = root.join("remote.git");
    git(root, &["init", "--quiet", "--bare", "-b", "main", remote.to_str().unwrap()]);
    let repo: PathBuf = root.join("widgets");
    std::fs::create_dir_all(repo.join(".github")).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    configure(&repo);
    std::fs::write(repo.join("README.md"), "widgets\n").unwrap();
    std::fs::write(
        repo.join(".github/pull_request_template.md"),
        "## Summary\n\n## Testing\n- [ ] tests pass\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "chore: init"]);
    git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&repo, &["push", "--quiet", "-u", "origin", "main"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    git(&repo, &["checkout", "--quiet", "-b", "feature/widgets"]);
    std::fs::write(repo.join("widget.rs"), "pub fn widget() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "feat: add widget"]);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: replying(vec![AgentEvent::TextDelta {
            text: "feat: add widget\n\n## Summary\nAdds the widget.\n\n## Testing\n- [ ] tests pass\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)".into(),
        }]),
    }));
    let core = EngineCore::assemble(&root.join("data"), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine assembles");
    register(&core, "widgets", &repo);
    let client = zeron_rpc::memory_client(core.rpc_service());

    // Generated text follows the template; the credit line is stripped.
    let generated: GeneratedPullRequest = call(
        &client,
        methods::GENERATE_PULL_REQUEST,
        serde_json::json!({ "cwd": repo }),
    )
    .await;
    assert_eq!(generated.base, "main");
    assert_eq!(generated.title, "feat: add widget");
    assert_eq!(
        generated.template.as_deref(),
        Some(".github/pull_request_template.md")
    );
    assert!(generated.body.starts_with("## Summary\nAdds the widget."));
    assert!(!generated.body.contains("Generated with"));
    assert!(invocations(&log).is_empty(), "generation never calls gh");

    // Create: the unpublished branch is pushed first, then `gh pr create`
    // gets exactly the edited text, the base, the head and --draft.
    let created: CreatedPullRequest = call(
        &client,
        methods::CREATE_PULL_REQUEST,
        serde_json::json!({
            "cwd": repo,
            "title": "Add widget",
            "body": "Adds the widget.\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
            "base": "main",
            "draft": true,
        }),
    )
    .await;
    assert_eq!(created.url, "https://github.com/acme/widgets/pull/42");
    assert_eq!(created.number, Some(42));
    assert!(created.pushed);
    assert_eq!(created.head, "feature/widgets");
    assert_eq!(
        git(&remote, &["rev-parse", "feature/widgets"]),
        git(&repo, &["rev-parse", "HEAD"])
    );
    let calls = invocations(&log);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let (args, stdin) = &calls[0];
    assert_eq!(&args[..2], ["pr", "create"]);
    assert_eq!(arg_after(args, "--title"), Some("Add widget"));
    assert_eq!(arg_after(args, "--body-file"), Some("-"));
    assert_eq!(arg_after(args, "--base"), Some("main"));
    assert_eq!(arg_after(args, "--head"), Some("feature/widgets"));
    assert!(args.iter().any(|arg| arg == "--draft"));
    assert_eq!(stdin.as_deref(), Some("Adds the widget."));

    // An existing pull request comes back as a specific, linkable error.
    git(&repo, &["checkout", "--quiet", "-b", "dup-branch"]);
    let error = client
        .call(
            methods::CREATE_PULL_REQUEST,
            serde_json::json!({ "cwd": repo, "title": "Dup", "body": "Dup", "base": "main" }),
        )
        .await
        .expect_err("duplicate pull request");
    assert!(
        error
            .to_string()
            .contains("already exists: https://github.com/acme/widgets/pull/7"),
        "{error}"
    );

    // Opening a PR from the base branch is refused before gh runs.
    git(&repo, &["checkout", "--quiet", "main"]);
    let before = invocations(&log).len();
    let error = client
        .call(
            methods::CREATE_PULL_REQUEST,
            serde_json::json!({ "cwd": repo, "title": "x", "body": "y" }),
        )
        .await
        .expect_err("base branch");
    assert!(error.to_string().contains("base branch main"), "{error}");
    assert_eq!(invocations(&log).len(), before);

    // Publish: a local-only repository becomes a private GitHub repository.
    let local = root.join("Local Tool");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "--quiet", "-b", "main"]);
    configure(&local);
    std::fs::write(local.join("tool.sh"), "echo hi\n").unwrap();
    git(&local, &["add", "."]);
    git(&local, &["commit", "--quiet", "-m", "Initial commit"]);
    register(&core, "local", &local);
    let published: PublishedRepository = call(
        &client,
        methods::PUBLISH_REPOSITORY,
        serde_json::json!({ "cwd": local, "visibility": "private" }),
    )
    .await;
    assert_eq!(published.url, "https://github.com/acme/Local-Tool");
    assert_eq!(published.status.remote.as_deref(), Some("origin"));
    assert_eq!(published.status.upstream.as_deref(), Some("origin/main"));
    let calls = invocations(&log);
    let (args, _) = calls.last().unwrap();
    assert_eq!(
        args,
        &[
            "repo",
            "create",
            "Local-Tool",
            "--private",
            "--source",
            &std::fs::canonicalize(&local).unwrap().to_string_lossy(),
            "--remote",
            "origin",
            "--push",
        ]
    );

    // Publishing twice is refused: the repository already has a remote.
    let error = client
        .call(
            methods::PUBLISH_REPOSITORY,
            serde_json::json!({ "cwd": local, "visibility": "public" }),
        )
        .await
        .expect_err("already published");
    assert!(error.to_string().contains("already has a remote"), "{error}");

    core.shutdown().await;
}
