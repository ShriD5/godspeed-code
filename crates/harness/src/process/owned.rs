//! Process ownership, including descendants that outlive their agent.
use std::ops::{Deref, DerefMut};

use crate::process::{Child as ProcessChild, Command};

/// A private process group, and none of the markers a Claude Code session
/// leaves in its children's environment. Every Claude Godspeed starts,
/// directly or through an adapter, reports Godspeed as its entrypoint.
pub(crate) fn configure(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    for key in [
        "CLAUDECODE",
        "CLAUDE_CODE_SSE_PORT",
        "CLAUDE_AGENT_SDK_VERSION",
    ] {
        command.env_remove(key);
    }
    command.env("CLAUDE_CODE_ENTRYPOINT", "godspeed");
}

pub(crate) struct Child {
    inner: ProcessChild,
    #[cfg(unix)]
    group: Option<i32>,
    /// Lets the next engine end this group if this one dies first
    /// ([`crate::process::registry`]).
    _registration: Option<crate::process::registry::Registration>,
}

impl Child {
    pub(crate) fn new(inner: ProcessChild) -> Self {
        Self {
            #[cfg(unix)]
            group: Some(-(inner.id().expect("newly spawned child") as i32)),
            _registration: crate::process::registry::register(inner.id(), "agent"),
            inner,
        }
    }

    pub(crate) async fn shutdown(&mut self, grace: std::time::Duration) {
        // Everything the agent started, taken while it is still attached:
        // a command the agent ran in its own session (Devin does) is not in
        // the agent's process group and would outlive the runtime.
        #[cfg(unix)]
        let tree = match (self.group, self.inner.id()) {
            (Some(_), Some(pid)) => crate::process::descendants(pid).await,
            _ => Vec::new(),
        };
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            crate::send_signal(&group, crate::Signal::Term);
            // Pi uses detached bash groups and cleans them in its TERM handler.
            // Give descendants their grace even when the adapter exited first.
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let _ = self.inner.try_wait();
                // SAFETY: signal 0 only checks the private process group.
                if unsafe { libc::kill(group, 0) } != 0 {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    crate::send_signal(&group, crate::Signal::Kill);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        crate::shutdown_child(&mut self.inner, grace).await;
        #[cfg(unix)]
        crate::process::terminate_tree(&tree, grace).await;
    }

    pub(crate) fn request_group_shutdown(&self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            crate::send_signal(&group, crate::Signal::Term);
        }
    }

    pub(crate) fn terminate_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            crate::send_signal(&group, crate::Signal::Kill);
        }
    }
}

impl Deref for Child {
    type Target = ProcessChild;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // Tokio's kill_on_drop covers the direct child. Keep the group id
        // even after wait() has reaped it, so descendants cannot escape cleanup.
        self.terminate_group();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn acp_environment_removes_nested_claude_markers() {
        let mut command = Command::new("unused");
        configure(&mut command);
        let removed: Vec<_> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_str().unwrap())
            .collect();
        assert_eq!(
            removed,
            vec![
                "CLAUDECODE",
                "CLAUDE_AGENT_SDK_VERSION",
                "CLAUDE_CODE_SSE_PORT"
            ]
        );
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_ENTRYPOINT")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("godspeed"))
        );
    }
}
