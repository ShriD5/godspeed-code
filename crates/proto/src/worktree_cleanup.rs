//! Worktree cleanup: the device's policy for deleting Godspeed worktrees on
//! its own (merged branch, untouched for days), the storage listing behind
//! Settings → General → Worktrees, and the one decision both share.
//!
//! The decision is a pure function of facts the engine gathers per worktree,
//! so the table of cases is tested here once: a worktree is eligible for a
//! reason, and then kept anyway when deleting it could lose work or pull the
//! floor out from under a running agent.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

/// The idle spans offered for "untouched for N days".
pub const WORKTREE_STALE_DAY_CHOICES: [u32; 3] = [7, 14, 30];

/// Device setting (`GetWorktreeCleanupSettings`). Everything off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorktreeCleanupSettings {
    /// Delete worktrees whose branch is merged into the default branch.
    pub delete_merged: bool,
    /// Delete worktrees untouched for this many days (one of
    /// [`WORKTREE_STALE_DAY_CHOICES`]); `None` is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_days: Option<u32>,
}

impl WorktreeCleanupSettings {
    /// Whether the background sweep has anything to do.
    pub fn enabled(&self) -> bool {
        self.delete_merged || self.delete_after_days.is_some()
    }

    /// Why these settings can't be saved, if they can't.
    pub fn check(&self) -> Result<(), String> {
        match self.delete_after_days {
            Some(days) if !WORKTREE_STALE_DAY_CHOICES.contains(&days) => Err(format!(
                "Choose 7, 14 or 30 days for untouched worktrees, not {days}"
            )),
            _ => Ok(()),
        }
    }
}

/// Why a worktree may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeCleanupReason {
    /// Its branch is merged into the default branch.
    Merged,
    /// Nothing happened in it for the policy's number of days.
    Stale,
    /// No chat uses it any more. Only "Delete all eligible" acts on this;
    /// the background sweep follows the policy alone.
    Unused,
}

impl WorktreeCleanupReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Merged => "Merged",
            Self::Stale => "Untouched",
            Self::Unused => "Unused",
        }
    }
}

/// Why an eligible worktree stays. The order is the order of the checks:
/// work that could be lost first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeKeepReason {
    /// Uncommitted or untracked changes (or a status git couldn't read).
    Uncommitted,
    /// Commits on its branch that are on no remote and not in the default
    /// branch.
    Unpushed,
    /// A chat working in it is running or waiting on input.
    Running,
}

impl WorktreeKeepReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Uncommitted => "Kept: has changes",
            Self::Unpushed => "Kept: unpushed commits",
            Self::Running => "Kept: chat running",
        }
    }
}

/// What the engine knows about one worktree when it decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeFacts {
    pub merged: bool,
    pub dirty: bool,
    pub unpushed: bool,
    pub running: bool,
    /// No chat's folder lies inside it.
    pub unused: bool,
    /// Newest of: a commit, an index or HEAD change, a chat message. `None`
    /// when unknown, which never counts as stale.
    pub last_activity: Option<DateTime<Utc>>,
}

/// Whether the background sweep or "Delete all eligible" is deciding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    /// The policy's rules only.
    Automatic,
    /// The policy's rules plus worktrees no chat uses.
    Manual,
}

/// The verdict for one worktree: eligible for `reason`, unless `kept`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCleanupDecision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<WorktreeCleanupReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<WorktreeKeepReason>,
}

impl WorktreeCleanupDecision {
    /// Delete it.
    pub fn deletes(&self) -> bool {
        self.reason.is_some() && self.kept.is_none()
    }
}

/// Decide one worktree. Eligibility: merged (when the policy deletes
/// merged), then untouched past the policy's days, then, for a manual clean,
/// unused. An eligible worktree with uncommitted changes, unpushed commits or
/// a running chat is kept.
pub fn decide_worktree_cleanup(
    settings: &WorktreeCleanupSettings,
    facts: &WorktreeFacts,
    now: DateTime<Utc>,
    mode: CleanupMode,
) -> WorktreeCleanupDecision {
    let stale = settings
        .delete_after_days
        .zip(facts.last_activity)
        .is_some_and(|(days, at)| now - at >= TimeDelta::days(i64::from(days)));
    let reason = if settings.delete_merged && facts.merged {
        Some(WorktreeCleanupReason::Merged)
    } else if stale {
        Some(WorktreeCleanupReason::Stale)
    } else if mode == CleanupMode::Manual && facts.unused {
        Some(WorktreeCleanupReason::Unused)
    } else {
        None
    };
    let kept = reason.and_then(|_| {
        if facts.dirty {
            Some(WorktreeKeepReason::Uncommitted)
        } else if facts.unpushed {
            Some(WorktreeKeepReason::Unpushed)
        } else if facts.running {
            Some(WorktreeKeepReason::Running)
        } else {
            None
        }
    });
    WorktreeCleanupDecision { reason, kept }
}

/// One Godspeed-created worktree in the storage listing
/// (`ListWorktreeStorage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStorageEntry {
    pub repo_path: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Bytes on disk of its checkout (the shared object store isn't
    /// counted). `None` until measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<DateTime<Utc>>,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub unpushed: bool,
    #[serde(default)]
    pub running: bool,
    /// The newest chat working in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_title: Option<String>,
    /// The verdict "Delete all eligible" would act on.
    #[serde(default)]
    pub decision: WorktreeCleanupDecision,
}

/// `ListWorktreeStorage` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListWorktreeStorageParams {
    /// One repository; every git project on the device when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// Measure sizes not yet cached (slow on big trees). Without it, cached
    /// sizes only.
    pub sizes: bool,
}

/// `CleanWorktrees` params: delete every eligible worktree now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CleanWorktreesParams {
    /// Only these worktrees (still decided afresh); all when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedWorktree {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub reason: WorktreeCleanupReason,
    /// The environment's teardown failed; the worktree is gone regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptWorktree {
    pub path: String,
    pub reason: WorktreeKeepReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedWorktree {
    pub path: String,
    pub error: String,
}

/// What a clean did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorktreeCleanupReport {
    pub deleted: Vec<DeletedWorktree>,
    pub kept: Vec<KeptWorktree>,
    pub failed: Vec<FailedWorktree>,
}

/// `ResolveWorktree` params: a checkout of `repo_path` named by its folder
/// (or any folder inside it) or by the branch it has checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveWorktreeParams {
    pub repo_path: String,
    pub target: String,
}

/// A checkout of a repository: the main one or a linked worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedWorktree {
    pub path: String,
    /// `None` when HEAD is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Commit HEAD points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The repository's main checkout rather than a linked worktree.
    #[serde(default)]
    pub main: bool,
}

/// "1.4 GB", "820 KB": decimal units, as Finder shows sizes.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-28T12:00:00Z".parse().unwrap()
    }

    fn days_ago(days: i64) -> Option<DateTime<Utc>> {
        Some(now() - TimeDelta::days(days))
    }

    const MERGED_ONLY: WorktreeCleanupSettings = WorktreeCleanupSettings {
        delete_merged: true,
        delete_after_days: None,
    };
    const BOTH: WorktreeCleanupSettings = WorktreeCleanupSettings {
        delete_merged: true,
        delete_after_days: Some(14),
    };

    #[test]
    fn defaults_are_off() {
        let settings = WorktreeCleanupSettings::default();
        assert!(!settings.enabled());
        assert_eq!(
            serde_json::to_value(settings).unwrap(),
            serde_json::json!({ "deleteMerged": false })
        );
        let parsed: WorktreeCleanupSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, settings);
        assert!(MERGED_ONLY.enabled());
        assert!(
            WorktreeCleanupSettings {
                delete_merged: false,
                delete_after_days: Some(7)
            }
            .enabled()
        );
    }

    #[test]
    fn only_the_offered_spans_save() {
        for days in WORKTREE_STALE_DAY_CHOICES {
            let settings = WorktreeCleanupSettings {
                delete_merged: false,
                delete_after_days: Some(days),
            };
            assert!(settings.check().is_ok());
        }
        let odd = WorktreeCleanupSettings {
            delete_merged: false,
            delete_after_days: Some(3),
        };
        assert!(odd.check().unwrap_err().contains("not 3"));
    }

    /// (settings, facts, mode) → (reason, kept).
    #[test]
    fn the_decision_table() {
        use CleanupMode::{Automatic as Auto, Manual};
        use WorktreeCleanupReason::{Merged, Stale, Unused};
        use WorktreeKeepReason::{Running, Uncommitted, Unpushed};
        let off = WorktreeCleanupSettings::default();
        let fresh = WorktreeFacts {
            last_activity: days_ago(1),
            ..Default::default()
        };
        let old = WorktreeFacts {
            last_activity: days_ago(20),
            ..Default::default()
        };
        let merged = WorktreeFacts {
            merged: true,
            ..fresh.clone()
        };
        let cases: Vec<(
            &str,
            WorktreeCleanupSettings,
            WorktreeFacts,
            CleanupMode,
            Option<WorktreeCleanupReason>,
            Option<WorktreeKeepReason>,
        )> = vec![
            (
                "policy off deletes nothing",
                off,
                merged.clone(),
                Auto,
                None,
                None,
            ),
            (
                "policy off, merged and old",
                off,
                WorktreeFacts {
                    merged: true,
                    ..old.clone()
                },
                Auto,
                None,
                None,
            ),
            (
                "merged goes",
                MERGED_ONLY,
                merged.clone(),
                Auto,
                Some(Merged),
                None,
            ),
            (
                "unmerged fresh stays",
                BOTH,
                fresh.clone(),
                Auto,
                None,
                None,
            ),
            (
                "old goes when the span is on",
                BOTH,
                old.clone(),
                Auto,
                Some(Stale),
                None,
            ),
            (
                "old stays when only merged is on",
                MERGED_ONLY,
                old.clone(),
                Auto,
                None,
                None,
            ),
            (
                "exactly at the span is stale",
                BOTH,
                WorktreeFacts {
                    last_activity: days_ago(14),
                    ..Default::default()
                },
                Auto,
                Some(Stale),
                None,
            ),
            (
                "unknown activity is never stale",
                BOTH,
                WorktreeFacts::default(),
                Auto,
                None,
                None,
            ),
            (
                "merged wins over stale",
                BOTH,
                WorktreeFacts {
                    merged: true,
                    ..old.clone()
                },
                Auto,
                Some(Merged),
                None,
            ),
            (
                "dirty is kept",
                MERGED_ONLY,
                WorktreeFacts {
                    dirty: true,
                    ..merged.clone()
                },
                Auto,
                Some(Merged),
                Some(Uncommitted),
            ),
            (
                "unpushed is kept",
                BOTH,
                WorktreeFacts {
                    unpushed: true,
                    ..old.clone()
                },
                Auto,
                Some(Stale),
                Some(Unpushed),
            ),
            (
                "running is kept",
                MERGED_ONLY,
                WorktreeFacts {
                    running: true,
                    ..merged.clone()
                },
                Auto,
                Some(Merged),
                Some(Running),
            ),
            (
                "dirty reported before running",
                MERGED_ONLY,
                WorktreeFacts {
                    dirty: true,
                    running: true,
                    unpushed: true,
                    ..merged.clone()
                },
                Auto,
                Some(Merged),
                Some(Uncommitted),
            ),
            (
                "unpushed reported before running",
                BOTH,
                WorktreeFacts {
                    running: true,
                    unpushed: true,
                    ..old.clone()
                },
                Auto,
                Some(Stale),
                Some(Unpushed),
            ),
            (
                "the sweep leaves unused alone",
                off,
                WorktreeFacts {
                    unused: true,
                    ..fresh.clone()
                },
                Auto,
                None,
                None,
            ),
            (
                "delete all eligible takes unused",
                off,
                WorktreeFacts {
                    unused: true,
                    ..fresh.clone()
                },
                Manual,
                Some(Unused),
                None,
            ),
            (
                "unused but dirty is kept",
                off,
                WorktreeFacts {
                    unused: true,
                    dirty: true,
                    ..fresh.clone()
                },
                Manual,
                Some(Unused),
                Some(Uncommitted),
            ),
            (
                "a used, unmerged, fresh worktree is not eligible",
                BOTH,
                fresh.clone(),
                Manual,
                None,
                None,
            ),
            (
                "not eligible: changes don't matter",
                BOTH,
                WorktreeFacts {
                    dirty: true,
                    running: true,
                    ..fresh
                },
                Manual,
                None,
                None,
            ),
        ];
        for (name, settings, facts, mode, reason, kept) in cases {
            let decision = decide_worktree_cleanup(&settings, &facts, now(), mode);
            assert_eq!(decision, WorktreeCleanupDecision { reason, kept }, "{name}");
            assert_eq!(
                decision.deletes(),
                reason.is_some() && kept.is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn bytes_read_like_finder() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(999), "999 bytes");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(820_000), "820 KB");
        assert_eq!(format_bytes(1_400_000_000), "1.4 GB");
    }
}
