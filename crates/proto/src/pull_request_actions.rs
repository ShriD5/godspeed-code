//! Pull request review, merge and watch contracts (`PerformChangeRequestAction`,
//! `GetChangeRequestMergeOptions`, `ListPullRequestWatches`,
//! `SetPullRequestWatch`). GitHub only for now; every write is an explicit
//! user action, and GitHub stays the authority on permissions and checks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Longest review, reply or comment body a write accepts.
pub const MAX_ACTION_BODY_BYTES: usize = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewEvent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::RequestChanges => "Request changes",
            Self::Comment => "Comment",
        }
    }

    /// Approving may say nothing; the other verdicts need words.
    pub fn needs_body(self) -> bool {
        !matches!(self, Self::Approve)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] = [Self::Squash, Self::Merge, Self::Rebase];

    /// The `gh pr merge` flag.
    pub fn gh_flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Create a merge commit",
            Self::Squash => "Squash and merge",
            Self::Rebase => "Rebase and merge",
        }
    }

    /// GitHub's GraphQL `PullRequestMergeMethod`.
    pub fn from_graphql(value: &str) -> Option<Self> {
        match value {
            "MERGE" => Some(Self::Merge),
            "SQUASH" => Some(Self::Squash),
            "REBASE" => Some(Self::Rebase),
            _ => None,
        }
    }
}

/// One explicit write on a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ChangeRequestAction {
    Review { event: ReviewEvent, body: String },
    ReplyToThread { thread_id: String, body: String },
    ResolveThread { thread_id: String },
    UnresolveThread { thread_id: String },
    /// Merges only if the head is still `head_ref_oid`, the commit the user
    /// confirmed: a push in between must never merge unseen code.
    Merge {
        method: MergeMethod,
        head_ref_oid: String,
    },
    EnableAutoMerge { method: MergeMethod },
    DisableAutoMerge,
    MarkReady,
    Close,
    Reopen,
}

impl ChangeRequestAction {
    /// The progress line while `gh` runs.
    pub fn progress_label(&self) -> &'static str {
        match self {
            Self::Review {
                event: ReviewEvent::Approve,
                ..
            } => "Approving…",
            Self::Review {
                event: ReviewEvent::RequestChanges,
                ..
            } => "Requesting changes…",
            Self::Review { .. } => "Submitting review…",
            Self::ReplyToThread { .. } => "Posting reply…",
            Self::ResolveThread { .. } => "Resolving thread…",
            Self::UnresolveThread { .. } => "Reopening thread…",
            Self::Merge { .. } => "Merging…",
            Self::EnableAutoMerge { .. } => "Enabling auto-merge…",
            Self::DisableAutoMerge => "Disabling auto-merge…",
            Self::MarkReady => "Marking ready for review…",
            Self::Close => "Closing…",
            Self::Reopen => "Reopening…",
        }
    }

    /// What happened, once GitHub accepted it.
    pub fn done_label(&self) -> &'static str {
        match self {
            Self::Review {
                event: ReviewEvent::Approve,
                ..
            } => "Approved",
            Self::Review {
                event: ReviewEvent::RequestChanges,
                ..
            } => "Changes requested",
            Self::Review { .. } => "Review submitted",
            Self::ReplyToThread { .. } => "Reply posted",
            Self::ResolveThread { .. } => "Thread resolved",
            Self::UnresolveThread { .. } => "Thread reopened",
            Self::Merge { .. } => "Merged",
            Self::EnableAutoMerge { .. } => "Auto-merge enabled",
            Self::DisableAutoMerge => "Auto-merge disabled",
            Self::MarkReady => "Marked ready for review",
            Self::Close => "Closed",
            Self::Reopen => "Reopened",
        }
    }

    /// Reject input GitHub would reject anyway, before any process runs.
    pub fn validate(&self) -> Result<(), String> {
        let body_ok = |body: &str, required: bool| {
            if body.len() > MAX_ACTION_BODY_BYTES {
                Err("The text is too long (maximum 60,000 bytes).".to_owned())
            } else if required && body.trim().is_empty() {
                Err("Write something first.".to_owned())
            } else {
                Ok(())
            }
        };
        match self {
            Self::Review { event, body } => body_ok(body, event.needs_body()),
            Self::ReplyToThread { thread_id, body } => {
                valid_node_id(thread_id)?;
                body_ok(body, true)
            }
            Self::ResolveThread { thread_id } | Self::UnresolveThread { thread_id } => {
                valid_node_id(thread_id)
            }
            Self::Merge { head_ref_oid, .. } => {
                if crate::change_request_assessment::valid_head_oid(head_ref_oid) {
                    Ok(())
                } else {
                    Err("Reload the pull request before merging.".to_owned())
                }
            }
            Self::EnableAutoMerge { .. }
            | Self::DisableAutoMerge
            | Self::MarkReady
            | Self::Close
            | Self::Reopen => Ok(()),
        }
    }
}

/// A GraphQL node id: short, URL-safe base64 with GitHub's `_` prefixes.
fn valid_node_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'=' | b'+' | b'/'))
    {
        Ok(())
    } else {
        Err("That review thread can't be found. Refresh and try again.".to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRequestActionResult {
    pub message: String,
}

/// What the repository allows, read when the pull request opens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChangeRequestMergeOptions {
    /// In the order the merge menu offers them.
    pub allowed_methods: Vec<MergeMethod>,
    pub auto_merge_allowed: bool,
    /// Set when auto-merge is already on for this pull request.
    pub auto_merge_method: Option<MergeMethod>,
    /// `ADMIN`, `MAINTAIN`, `WRITE`, `TRIAGE`, `READ`, or empty when unknown.
    pub viewer_permission: String,
}

impl ChangeRequestMergeOptions {
    pub fn viewer_can_write(&self) -> bool {
        matches!(
            self.viewer_permission.as_str(),
            "ADMIN" | "MAINTAIN" | "WRITE" | ""
        )
    }

    /// The method a watcher uses for auto-merge: squash, then merge, then rebase.
    pub fn preferred_method(&self) -> Option<MergeMethod> {
        MergeMethod::ALL
            .into_iter()
            .find(|method| self.allowed_methods.contains(method))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PullRequestWatchStatus {
    /// Polling on schedule.
    #[default]
    Watching,
    /// Polling slower or paused: nobody has used the app for a while.
    Idle,
    /// The last poll failed; retrying with backoff.
    BackingOff,
    /// Merged or closed: nothing left to watch.
    Stopped,
    /// Turned off by the user.
    Off,
}

impl PullRequestWatchStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Watching => "Watching",
            Self::Idle => "Paused while idle",
            Self::BackingOff => "Retrying",
            Self::Stopped => "Stopped",
            Self::Off => "Off",
        }
    }
}

/// One pull request's watch, as the PR pane and chat rows show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestWatch {
    pub url: String,
    /// The chat that wakes on events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub enabled: bool,
    /// Turn on GitHub auto-merge once checks pass and the PR is approved.
    #[serde(default)]
    pub auto_merge: bool,
    /// Registered by Godspeed for a PR one of its chats opened.
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub status: PullRequestWatchStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The latest event, one line ("Checks failed: build").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    /// Chat wakes in the last hour (capped per PR).
    #[serde(default)]
    pub wakes_last_hour: u32,
}

impl PullRequestWatch {
    pub fn active(&self) -> bool {
        self.enabled && self.status != PullRequestWatchStatus::Stopped
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPullRequestWatch {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip_with_camel_case_tags() {
        let action = ChangeRequestAction::Merge {
            method: MergeMethod::Squash,
            head_ref_oid: "a".repeat(40),
        };
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(value["kind"], "merge");
        assert_eq!(value["method"], "squash");
        assert_eq!(value["headRefOid"], "a".repeat(40));
        assert_eq!(
            serde_json::from_value::<ChangeRequestAction>(value).unwrap(),
            action
        );
        let reply: ChangeRequestAction = serde_json::from_value(serde_json::json!({
            "kind": "replyToThread", "threadId": "PRRT_kwDO", "body": "done"
        }))
        .unwrap();
        assert_eq!(
            reply,
            ChangeRequestAction::ReplyToThread {
                thread_id: "PRRT_kwDO".into(),
                body: "done".into()
            }
        );
    }

    #[test]
    fn validation_requires_words_except_for_approvals() {
        let review = |event, body: &str| ChangeRequestAction::Review {
            event,
            body: body.into(),
        };
        assert!(review(ReviewEvent::Approve, "").validate().is_ok());
        assert!(review(ReviewEvent::RequestChanges, "  ").validate().is_err());
        assert!(review(ReviewEvent::Comment, "ok").validate().is_ok());
        assert!(
            review(ReviewEvent::Comment, &"x".repeat(60_001))
                .validate()
                .is_err()
        );
        assert!(
            ChangeRequestAction::ResolveThread {
                thread_id: "id; rm -rf".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            ChangeRequestAction::Merge {
                method: MergeMethod::Merge,
                head_ref_oid: "short".into()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn merge_options_prefer_squash_then_merge_then_rebase() {
        let options = ChangeRequestMergeOptions {
            allowed_methods: vec![MergeMethod::Rebase, MergeMethod::Merge],
            ..Default::default()
        };
        assert_eq!(options.preferred_method(), Some(MergeMethod::Merge));
        assert!(options.viewer_can_write());
        assert!(
            !ChangeRequestMergeOptions {
                viewer_permission: "READ".into(),
                ..Default::default()
            }
            .viewer_can_write()
        );
    }
}
