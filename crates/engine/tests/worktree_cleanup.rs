//! Worktree cleanup over the RPC surface, in a temporary repository: the
//! policy's defaults and validation, the storage listing (size, branch,
//! merged / dirty / unpushed state, owning chat), and the safety of "Delete
//! all eligible": merged and unused worktrees go with their teardown run,
//! while uncommitted changes, unpushed commits and running chats keep theirs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::json;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::{
    CreateWorktreeOutcome, HarnessId, WorktreeCleanupReason, WorktreeCleanupReport,
    WorktreeCleanupSettings, WorktreeKeepReason, WorktreeStorageEntry,
};
use zeron_rpc::{RpcClient, methods};

const SPACE: &str = "space-app";

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
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    let core = EngineCore::assemble(
        &root.join("data"),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .expect("engine assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            methods::SET_WORKTREE_SETTINGS,
            json!({ "useCustomDirectory": true, "customDirectory": root.join("worktrees") }),
        )
        .await
        .unwrap();
    core.workspace
        .create_space(SPACE, &core.device_id, &repo.to_string_lossy(), None, true)
        .unwrap();
    Fixture {
        _tmp: tmp,
        root,
        repo,
        core,
        client,
    }
}

impl Fixture {
    async fn worktree(&self) -> PathBuf {
        let outcome: CreateWorktreeOutcome = self
            .client
            .call_as(
                methods::CREATE_WORKTREE,
                json!({ "repoPath": self.repo, "branch": "main", "spaceId": SPACE }),
            )
            .await
            .unwrap();
        PathBuf::from(outcome.worktree.path)
    }

    async fn settings(
        &self,
        settings: serde_json::Value,
    ) -> Result<WorktreeCleanupSettings, String> {
        self.client
            .call_as(methods::SET_WORKTREE_CLEANUP_SETTINGS, settings)
            .await
            .map_err(|e| e.to_string())
    }

    async fn list(&self, sizes: bool) -> Vec<WorktreeStorageEntry> {
        self.client
            .call_as(methods::LIST_WORKTREE_STORAGE, json!({ "sizes": sizes }))
            .await
            .unwrap()
    }

    async fn clean(&self) -> WorktreeCleanupReport {
        self.client
            .call_as(methods::CLEAN_WORKTREES, json!({}))
            .await
            .unwrap()
    }

    fn chat_in(&self, id: &str, cwd: &Path) {
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
    }
}

fn entry<'a>(entries: &'a [WorktreeStorageEntry], path: &Path) -> &'a WorktreeStorageEntry {
    entries
        .iter()
        .find(|e| same_file::is_same_file(&e.path, path).unwrap_or(false))
        .unwrap_or_else(|| panic!("{} not listed in {entries:#?}", path.display()))
}

/// Commit on the worktree's branch, then merge it into main.
fn merge_work(repo: &Path, worktree: &Path, file: &str) {
    std::fs::write(worktree.join(file), "work\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-m", file]);
    let branch = git(worktree, &["branch", "--show-current"]);
    git(repo, &["merge", "--no-ff", "-m", "merge", &branch]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_default_off_and_refuse_odd_spans() {
    let f = fixture().await;
    let got: WorktreeCleanupSettings = f
        .client
        .call_as(methods::GET_WORKTREE_CLEANUP_SETTINGS, json!({}))
        .await
        .unwrap();
    assert_eq!(got, WorktreeCleanupSettings::default());
    assert!(!got.enabled());
    let saved = f
        .settings(json!({ "deleteMerged": true, "deleteAfterDays": 14 }))
        .await
        .unwrap();
    assert_eq!(saved.delete_after_days, Some(14));
    let err = f
        .settings(json!({ "deleteAfterDays": 5 }))
        .await
        .unwrap_err();
    assert!(err.contains("not 5"), "{err}");
    // The refused change didn't land, and the saved one survives a restart.
    let got: WorktreeCleanupSettings = f
        .client
        .call_as(methods::GET_WORKTREE_CLEANUP_SETTINGS, json!({}))
        .await
        .unwrap();
    assert_eq!(got, saved);
    assert!(f.root.join("data/worktree-cleanup.json").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_lists_state_and_clean_keeps_work() {
    let f = fixture().await;
    let merged = f.worktree().await;
    merge_work(&f.repo, &merged, "merged.txt");
    let merged_dirty = f.worktree().await;
    merge_work(&f.repo, &merged_dirty, "merged-dirty.txt");
    std::fs::write(merged_dirty.join("scratch.txt"), "unsaved").unwrap();
    let unpushed = f.worktree().await;
    std::fs::write(unpushed.join("wip.txt"), "wip\n").unwrap();
    git(&unpushed, &["add", "."]);
    git(&unpushed, &["commit", "-m", "wip"]);
    let busy = f.worktree().await;
    merge_work(&f.repo, &busy, "busy.txt");
    let fresh_in_use = f.worktree().await;
    let unused = f.worktree().await;
    f.chat_in("chat-busy", &busy);
    f.chat_in("chat-fresh", &fresh_in_use.join("src"));
    // The busy chat is working.
    let session: zeron_proto::Session = serde_json::from_value(json!({
        "chatId": "chat-busy", "deviceId": f.core.device_id, "status": "working",
        "startedAt": chrono::Utc::now(), "updatedAt": chrono::Utc::now(),
    }))
    .unwrap();
    f.core.workspace.record_session(&session);
    // Ignored build output counts toward size, not as a change.
    std::fs::write(f.repo.join(".git/info/exclude"), "*.bin\n").unwrap();
    std::fs::write(unused.join("big.bin"), vec![7u8; 256 * 1024]).unwrap();

    // Without sizes, nothing is measured yet; with them, every row is.
    let quick = f.list(false).await;
    assert_eq!(quick.len(), 6);
    assert!(quick.iter().all(|e| e.size_bytes.is_none()));
    let entries = f.list(true).await;
    assert!(entry(&entries, &unused).size_bytes.unwrap() >= 256 * 1024);
    assert!(entries.iter().all(|e| e.size_bytes.is_some()));

    let m = entry(&entries, &merged);
    assert!(m.merged && !m.dirty && !m.unpushed && m.chat_id.is_none());
    assert_eq!(m.decision.reason, Some(WorktreeCleanupReason::Unused));
    let md = entry(&entries, &merged_dirty);
    assert!(md.merged && md.dirty);
    assert_eq!(md.decision.kept, Some(WorktreeKeepReason::Uncommitted));
    let u = entry(&entries, &unpushed);
    assert!(!u.merged && u.unpushed);
    assert_eq!(u.decision.kept, Some(WorktreeKeepReason::Unpushed));
    let b = entry(&entries, &busy);
    assert!(b.merged && b.running);
    assert_eq!(b.chat_id.as_deref(), Some("chat-busy"));
    assert_eq!(b.decision.reason, None, "merged-delete is off; in use");
    let fresh = entry(&entries, &fresh_in_use);
    assert!(!fresh.merged, "a branch that never moved isn't merged");
    assert_eq!(fresh.chat_id.as_deref(), Some("chat-fresh"));
    assert!(fresh.branch.as_deref().unwrap().starts_with("godspeed/"));
    assert!(fresh.last_activity.is_some());

    // With merged-delete on, the busy one is kept for its running chat.
    f.settings(json!({ "deleteMerged": true })).await.unwrap();
    let report = f.clean().await;
    let deleted: Vec<&str> = report.deleted.iter().map(|d| d.path.as_str()).collect();
    let gone = |p: &Path| {
        deleted
            .iter()
            .any(|d| Path::new(d) == p || same_file::is_same_file(d, p).unwrap_or(false))
    };
    assert_eq!(report.deleted.len(), 2, "{report:#?}");
    assert!(!merged.exists() && !unused.exists(), "{report:#?}");
    assert!(report.deleted.iter().all(|d| d.teardown_error.is_none()));
    assert!(!gone(&busy) && busy.exists());
    assert!(merged_dirty.join("scratch.txt").exists());
    assert!(unpushed.join("wip.txt").exists());
    assert!(fresh_in_use.exists());
    let kept: Vec<WorktreeKeepReason> = report.kept.iter().map(|k| k.reason).collect();
    assert!(kept.contains(&WorktreeKeepReason::Uncommitted));
    assert!(kept.contains(&WorktreeKeepReason::Unpushed));
    assert!(kept.contains(&WorktreeKeepReason::Running));
    // The unpushed branch survives with its commit.
    assert_eq!(f.list(false).await.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cleaned_worktree_runs_its_teardown() {
    let f = fixture().await;
    let log = f.root.join("teardown.log");
    f.client
        .call(
            methods::SET_WORKTREE_ENVIRONMENT,
            json!({ "spaceId": SPACE, "environment": {
                "teardown": [format!("printf '%s\\n' '{{{{slug}}}}' >> '{}'", log.display())]
            } }),
        )
        .await
        .unwrap();
    let worktree = f.worktree().await;
    let report = f.clean().await;
    assert_eq!(report.deleted.len(), 1, "{report:#?}");
    assert_eq!(report.deleted[0].reason, WorktreeCleanupReason::Unused);
    assert!(!worktree.exists());
    let slug = worktree.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), slug);
}
