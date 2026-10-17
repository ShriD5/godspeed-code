//! Viewport-local state for worktree environments: each worktree's ports and
//! sleep state as its host reports them, and the project environment editor.

use std::collections::{BTreeMap, HashMap};

use gpui::{Entity, Subscription, Task};
use zeron_proto::{
    WorktreeCopyRule, WorktreeEnvironment, WorktreeEnvironmentSnapshot, WorktreeEnvironmentStatus,
    WorktreePortBlock,
};

use crate::composer::ComposerInput;

/// One worktree on one device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorktreeKey {
    pub device_id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorktreeStatusEntry {
    Loading,
    /// `None`: a worktree without an environment record.
    Ready(Option<WorktreeEnvironmentStatus>),
    /// The host predates worktree environments, or the request failed.
    Unavailable,
}

/// The editor's text fields, one entry per line (ports names as
/// `web=0, api=1`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentDraft {
    pub copy: String,
    pub symlink: String,
    pub ports: String,
    pub port_names: String,
    pub setup: String,
    pub teardown: String,
    pub wake: String,
    pub branch: String,
}

impl EnvironmentDraft {
    pub fn from_environment(environment: &WorktreeEnvironment) -> Self {
        let globs = |symlink: bool| {
            environment
                .copy
                .iter()
                .filter(|rule| rule.symlink == symlink)
                .map(|rule| rule.glob.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let (ports, port_names) = match &environment.ports {
            Some(block) => {
                let mut names: Vec<(&String, &u16)> = block.names.iter().collect();
                names.sort_by_key(|(name, offset)| (**offset, (*name).clone()));
                (
                    block.count.to_string(),
                    names
                        .into_iter()
                        .map(|(name, offset)| format!("{name}={offset}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            }
            None => (String::new(), String::new()),
        };
        Self {
            copy: globs(false),
            symlink: globs(true),
            ports,
            port_names,
            setup: environment.setup.join("\n"),
            teardown: environment.teardown.join("\n"),
            wake: environment.wake.join("\n"),
            branch: environment.branch.clone().unwrap_or_default(),
        }
    }

    /// The environment these fields describe, checked like the host checks it.
    pub fn to_environment(&self) -> Result<WorktreeEnvironment, String> {
        let lines = |text: &str| -> Vec<String> {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        };
        let rules = |text: &str, symlink: bool| {
            lines(text)
                .into_iter()
                .map(move |glob| WorktreeCopyRule { glob, symlink })
        };
        let mut copy: Vec<WorktreeCopyRule> = rules(&self.copy, false).collect();
        copy.extend(rules(&self.symlink, true));
        let ports = match self.ports.trim() {
            "" if self.port_names.trim().is_empty() => None,
            "" => return Err("Set how many ports each worktree gets before naming them".into()),
            count => Some(WorktreePortBlock {
                count: count
                    .parse()
                    .map_err(|_| format!("\"{count}\" isn't a number of ports"))?,
                names: parse_port_names(&self.port_names)?,
            }),
        };
        WorktreeEnvironment {
            copy,
            ports,
            setup: lines(&self.setup),
            teardown: lines(&self.teardown),
            wake: lines(&self.wake),
            branch: Some(self.branch.trim().to_string()),
        }
        .normalized()
    }
}

fn parse_port_names(text: &str) -> Result<BTreeMap<String, u16>, String> {
    let mut names = BTreeMap::new();
    for entry in text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let (name, offset) = entry
            .split_once('=')
            .ok_or_else(|| format!("Write port names as name=offset (\"{entry}\")"))?;
        let offset = offset
            .trim()
            .parse()
            .map_err(|_| format!("\"{}\" isn't a port offset", offset.trim()))?;
        names.insert(name.trim().to_string(), offset);
    }
    Ok(names)
}

pub struct EnvironmentEditor {
    pub key: crate::project_actions::ProjectActionsKey,
    pub snapshot: Option<WorktreeEnvironmentSnapshot>,
    pub load_error: Option<String>,
    pub fields: EnvironmentFields,
    pub error: Option<String>,
    pub saving: bool,
    pub focus_pending: bool,
    pub _events: Vec<Subscription>,
}

pub struct EnvironmentFields {
    pub copy: Entity<ComposerInput>,
    pub symlink: Entity<ComposerInput>,
    pub ports: Entity<ComposerInput>,
    pub port_names: Entity<ComposerInput>,
    pub setup: Entity<ComposerInput>,
    pub teardown: Entity<ComposerInput>,
    pub wake: Entity<ComposerInput>,
    pub branch: Entity<ComposerInput>,
}

#[derive(Default)]
pub struct WorktreeEnvController {
    pub statuses: HashMap<WorktreeKey, WorktreeStatusEntry>,
    /// The chat the card last showed; selecting another one re-reads its
    /// worktree and lets opening it wake a sleeping one.
    pub active_chat: Option<String>,
    pub wake_on_open: bool,
    pub busy: bool,
    pub editor: Option<EnvironmentEditor>,
    pub status_task: Option<Task<()>>,
    pub action_task: Option<Task<()>>,
    pub editor_task: Option<Task<()>>,
}

impl WorktreeEnvController {
    /// Note the selected chat; `true` when it changed.
    pub fn select(&mut self, chat_id: Option<&str>) -> bool {
        if self.active_chat.as_deref() == chat_id {
            return false;
        }
        self.active_chat = chat_id.map(str::to_string);
        self.wake_on_open = chat_id.is_some();
        true
    }

    pub fn status(&self, key: &WorktreeKey) -> Option<&WorktreeEnvironmentStatus> {
        match self.statuses.get(key)? {
            WorktreeStatusEntry::Ready(status) => status.as_ref(),
            _ => None,
        }
    }

    /// Store a reply; `true` when the worktree is asleep and was just opened,
    /// so the caller wakes it.
    pub fn accept_status(
        &mut self,
        key: WorktreeKey,
        status: Result<Option<WorktreeEnvironmentStatus>, String>,
    ) -> bool {
        let entry = match status {
            Ok(status) => WorktreeStatusEntry::Ready(status),
            Err(_) => WorktreeStatusEntry::Unavailable,
        };
        let sleeping =
            matches!(&entry, WorktreeStatusEntry::Ready(Some(status)) if status.sleeping);
        self.statuses.insert(key, entry);
        let wake = sleeping && self.wake_on_open;
        self.wake_on_open = false;
        wake
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(sleeping: bool) -> WorktreeEnvironmentStatus {
        WorktreeEnvironmentStatus {
            path: "/wt".into(),
            ports: None,
            sleeping,
            setup: None,
        }
    }

    fn key() -> WorktreeKey {
        WorktreeKey {
            device_id: "d".into(),
            path: "/wt".into(),
        }
    }

    #[test]
    fn drafts_round_trip_and_validate() {
        let environment: WorktreeEnvironment = serde_json::from_str(
            r#"{
                "copy": [".env*", {"glob": "node_modules", "symlink": true}],
                "ports": {"count": 3, "names": {"api": 1, "web": 0}},
                "setup": ["createdb app_{{slug}}", "make seed"],
                "teardown": "dropdb app_{{slug}}",
                "branch": "{{prefix}}{{slug}}"
            }"#,
        )
        .unwrap();
        let draft = EnvironmentDraft::from_environment(&environment);
        assert_eq!(draft.port_names, "web=0, api=1");
        assert_eq!(draft.symlink, "node_modules");
        assert_eq!(draft.to_environment().unwrap(), environment);

        let empty = EnvironmentDraft::default().to_environment().unwrap();
        assert!(empty.is_empty());

        let bad = |change: fn(&mut EnvironmentDraft)| {
            let mut draft = draft.clone();
            change(&mut draft);
            draft.to_environment().unwrap_err()
        };
        assert!(bad(|d| d.ports = "lots".into()).contains("number"));
        assert!(bad(|d| d.port_names = "web".into()).contains("name=offset"));
        assert!(bad(|d| d.port_names = "web=9".into()).contains("outside"));
        assert!(bad(|d| d.setup = "echo {{nope}}".into()).contains("unknown"));
        assert!(bad(|d| d.copy = "../up".into()).contains("inside"));
        assert!(
            bad(|d| {
                d.ports.clear();
                d.port_names = "web=0".into();
            })
            .contains("before naming")
        );
    }

    #[test]
    fn opening_a_sleeping_worktree_wakes_it_once() {
        let mut controller = WorktreeEnvController::default();
        assert!(controller.select(Some("chat")));
        assert!(controller.accept_status(key(), Ok(Some(status(true)))));
        assert!(!controller.accept_status(key(), Ok(Some(status(true)))));
        assert!(!controller.select(Some("chat")));

        controller.select(Some("other"));
        assert!(!controller.accept_status(key(), Ok(Some(status(false)))));
        assert!(!controller.accept_status(key(), Ok(Some(status(true)))));
        controller.select(Some("chat"));
        assert!(!controller.accept_status(key(), Err("offline".into())));
        assert!(controller.status(&key()).is_none());
    }
}
