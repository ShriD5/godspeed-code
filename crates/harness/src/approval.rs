//! Shared approval plumbing for the drivers: ask through the engine's input
//! bridge, build diff previews, and decide what an access mode lets through
//! without asking.
//!
//! Each driver maps its own wire onto [`ApprovalRequest`] and back; this
//! module only holds what more than one of them needs.

use std::path::{Component, Path, PathBuf};

use tokio::sync::oneshot;
use zeron_proto::{
    AccessMode, ApprovalDecision, ApprovalKind, ApprovalRequest, UserInputAnswer,
    UserInputQuestion,
};

/// The engine's input bridge, as every driver holds it.
pub type RequestInputFn =
    dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync;

/// Ask the user and wait. There is no timer: an approval waits until someone
/// answers (on any device, or the orchestrating agent), or the run ends —
/// a dropped bridge then declines; never a silent allow.
pub async fn ask(request_input: &RequestInputFn, request: ApprovalRequest) -> ApprovalDecision {
    let question = request.clone().into_question(uuid::Uuid::new_v4().to_string());
    let answers = (request_input)(vec![question.clone()])
        .await
        .unwrap_or_default();
    request.decide(&question.id, &answers)
}

/// Whether `mode` lets `kind` run without asking. Drivers whose CLI already
/// enforces the mode only consult this for requests the CLI still raises.
/// `in_workspace` says every path the request touches is under the run's
/// workspace (or an extra writable root).
pub fn auto_decision(mode: AccessMode, kind: ApprovalKind, in_workspace: bool) -> bool {
    match mode {
        AccessMode::FullAccess => true,
        AccessMode::AutoAcceptEdits | AccessMode::Auto => kind.is_edit() && in_workspace,
        AccessMode::Supervised => false,
    }
}

/// Whether every one of `paths` (absolute, or relative to `cwd`) stays under
/// `cwd` or one of `roots`, lexically: `..` segments are resolved, symlinks
/// are not followed. An empty list is not "inside" — nothing proves it.
pub fn paths_within<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    cwd: &str,
    roots: &[String],
) -> bool {
    let cwd = normalize(Path::new(cwd));
    let mut bases = vec![cwd.clone()];
    bases.extend(roots.iter().map(|r| normalize(Path::new(r))));
    let mut any = false;
    for path in paths {
        any = true;
        let path = Path::new(path);
        let full = if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&cwd.join(path))
        };
        if !bases
            .iter()
            .any(|base| !base.as_os_str().is_empty() && full.starts_with(base))
        {
            return false;
        }
    }
    any
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A unified diff preview for an edit given as old/new text (`old: None` is
/// a new file).
pub fn unified_diff(path: &str, old: Option<&str>, new: &str) -> String {
    let old_text = old.unwrap_or("");
    let diff = similar::TextDiff::from_lines(old_text, new);
    let old_header = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    diff.unified_diff()
        .context_radius(3)
        .header(&old_header, &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::UserInputAnswer;

    #[test]
    fn auto_decisions_follow_the_mode() {
        use ApprovalKind::*;
        assert!(auto_decision(AccessMode::FullAccess, Command, false));
        assert!(auto_decision(AccessMode::AutoAcceptEdits, FileChange, true));
        assert!(!auto_decision(AccessMode::AutoAcceptEdits, FileChange, false));
        assert!(!auto_decision(AccessMode::AutoAcceptEdits, Command, true));
        assert!(!auto_decision(AccessMode::Supervised, FileChange, true));
    }

    #[test]
    fn paths_within_resolves_dot_dot_and_extra_roots() {
        let roots = vec!["/data/agents/a1".to_owned()];
        assert!(paths_within(["src/lib.rs"], "/repo", &roots));
        assert!(paths_within(["/repo/a/../b.rs"], "/repo", &roots));
        assert!(!paths_within(["../escape.rs"], "/repo", &roots));
        assert!(!paths_within(["/repo-other/x"], "/repo", &roots));
        assert!(paths_within(["/data/agents/a1/MEMORY.md"], "/repo", &roots));
        assert!(!paths_within(std::iter::empty(), "/repo", &roots));
    }

    #[test]
    fn unified_diff_marks_new_files() {
        let diff = unified_diff("a.txt", None, "hello\n");
        assert!(diff.starts_with("--- /dev/null\n+++ b/a.txt\n"));
        assert!(diff.contains("+hello"));
        let diff = unified_diff("a.txt", Some("one\ntwo\n"), "one\nthree\n");
        assert!(diff.contains("-two\n+three"));
    }

    #[tokio::test]
    async fn ask_declines_when_the_bridge_drops() {
        let bridge = |_: Vec<UserInputQuestion>| oneshot::channel().1;
        let decision = ask(&bridge, ApprovalRequest::new(ApprovalKind::Command, "x")).await;
        assert_eq!(decision, ApprovalDecision::Decline);

        let bridge = |questions: Vec<UserInputQuestion>| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(vec![UserInputAnswer {
                question_id: questions[0].id.clone(),
                labels: vec!["Approve for this session".into()],
            }]);
            rx
        };
        let decision = ask(&bridge, ApprovalRequest::new(ApprovalKind::Command, "x")).await;
        assert_eq!(decision, ApprovalDecision::ApproveForSession);
    }
}
