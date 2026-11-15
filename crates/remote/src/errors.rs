//! Mapping `ssh` failures to errors a person can act on.
//!
//! ssh exits 255 for its own failures (connection, host key, auth); any other
//! code is the remote command's. Only 255 is matched against ssh's messages,
//! so a remote `mkdir: Permission denied` never reads as an auth failure.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshError {
    /// No `ssh` executable.
    SshMissing,
    /// ssh could not be started for another reason.
    Spawn(String),
    /// The command did not finish in time (it was stopped).
    Timeout { secs: u64 },
    /// The server's host key is not in `known_hosts` yet.
    HostKeyUnknown,
    /// The server's host key differs from the one in `known_hosts`.
    HostKeyChanged,
    /// No key the server accepts (or it only takes passwords).
    AuthFailed,
    /// DNS, refused, timed out, no route.
    Unreachable { detail: String },
    /// The connection dropped, or ssh failed some other way.
    ConnectionFailed { detail: String },
    /// The remote command failed.
    Remote { code: Option<i32>, stderr: String },
}

impl SshError {
    /// One message for the wizard: what happened and what to do next.
    /// `target` is the `user@host` the user entered.
    pub fn user_message(&self, target: &str) -> String {
        match self {
            Self::SshMissing => {
                "ssh isn't installed on this computer. Install OpenSSH, then try again.".into()
            }
            Self::Spawn(err) => format!("Couldn't start ssh: {err}"),
            Self::Timeout { secs } => format!(
                "{target} didn't answer within {secs} s. Check the server is up and try again."
            ),
            Self::HostKeyUnknown => format!(
                "This computer hasn't connected to {target} before. Run `ssh {target}` once in \
                 Terminal, check the fingerprint and answer yes, then try again."
            ),
            Self::HostKeyChanged => format!(
                "The host key of {target} has changed since you last connected. That can mean \
                 the server was rebuilt, or that someone is intercepting the connection. If you \
                 rebuilt it, remove the old key with `ssh-keygen -R <host>` and connect once in \
                 Terminal; otherwise stop here."
            ),
            Self::AuthFailed => format!(
                "{target} didn't accept any of your SSH keys. Godspeed never asks for passwords: \
                 add your public key to the server (`ssh-copy-id {target}`) or load it into the \
                 agent (`ssh-add`), then try again."
            ),
            Self::Unreachable { detail } => {
                format!(
                    "Couldn't reach {target}: {detail}. Check the address and that SSH is open."
                )
            }
            Self::ConnectionFailed { detail } => format!("The SSH connection failed: {detail}"),
            Self::Remote { code, stderr } => {
                let tail = last_lines(stderr, 3);
                match (code, tail.is_empty()) {
                    (Some(code), true) => {
                        format!("The command on the server failed (exit {code}).")
                    }
                    (None, true) => "The command on the server failed.".into(),
                    (_, false) => format!("The command on the server failed: {tail}"),
                }
            }
        }
    }
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message("the server"))
    }
}

impl std::error::Error for SshError {}

/// Classify a finished ssh process by exit code and stderr.
pub fn classify(code: Option<i32>, stderr: &str) -> SshError {
    if code != Some(255) {
        return SshError::Remote {
            code,
            stderr: stderr.trim().to_owned(),
        };
    }
    let lower = stderr.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("remote host identification has changed") || (has("host key for") && has("has changed"))
    {
        SshError::HostKeyChanged
    } else if has("host key verification failed") || has("host key is known for") {
        SshError::HostKeyUnknown
    } else if has("permission denied (")
        || has("too many authentication failures")
        || has("no supported authentication methods")
    {
        SshError::AuthFailed
    } else if has("could not resolve hostname")
        || has("name or service not known")
        || has("nodename nor servname")
    {
        SshError::Unreachable {
            detail: "the host name doesn't resolve".into(),
        }
    } else if has("connection refused") {
        SshError::Unreachable {
            detail: "the connection was refused (is SSH running on that port?)".into(),
        }
    } else if has("timed out") {
        SshError::Unreachable {
            detail: "the connection timed out".into(),
        }
    } else if has("no route to host") || has("network is unreachable") {
        SshError::Unreachable {
            detail: "there is no route to the host".into(),
        }
    } else {
        let detail = last_lines(stderr, 1);
        SshError::ConnectionFailed {
            detail: if detail.is_empty() {
                "ssh exited with status 255".into()
            } else {
                detail
            },
        }
    }
}

/// The last `n` non-empty lines, joined with spaces (bounded for display).
pub fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let joined = lines[lines.len().saturating_sub(n)..].join(" ");
    if joined.chars().count() > 400 {
        let cut: String = joined.chars().take(400).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_failures() {
        let unknown = "No ED25519 host key is known for box.example.com and you have requested strict checking.\nHost key verification failed.\n";
        assert_eq!(classify(Some(255), unknown), SshError::HostKeyUnknown);
        let changed = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@\nHost key verification failed.\n";
        assert_eq!(classify(Some(255), changed), SshError::HostKeyChanged);
        let msg = SshError::HostKeyUnknown.user_message("root@box");
        assert!(msg.contains("ssh root@box"), "{msg}");
    }

    #[test]
    fn auth_failures() {
        for stderr in [
            "root@203.0.113.7: Permission denied (publickey).",
            "me@100.64.0.1: Permission denied (publickey,password,keyboard-interactive).",
            "Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures",
        ] {
            assert_eq!(
                classify(Some(255), stderr),
                SshError::AuthFailed,
                "{stderr}"
            );
        }
        let msg = SshError::AuthFailed.user_message("root@box");
        assert!(msg.contains("never asks for passwords"));
        assert!(msg.contains("ssh-copy-id root@box"));
    }

    #[test]
    fn a_remote_permission_error_is_not_an_auth_failure() {
        let err = classify(
            Some(1),
            "mkdir: cannot create directory '/opt/x': Permission denied",
        );
        assert!(matches!(err, SshError::Remote { code: Some(1), .. }));
        assert!(err.user_message("x").contains("Permission denied"));
    }

    #[test]
    fn network_failures() {
        let cases = [
            (
                "ssh: Could not resolve hostname nope: Name or service not known",
                "resolve",
            ),
            (
                "ssh: connect to host 1.2.3.4 port 22: Connection refused",
                "refused",
            ),
            (
                "ssh: connect to host 1.2.3.4 port 22: Operation timed out",
                "timed out",
            ),
            (
                "ssh: connect to host 1.2.3.4 port 22: No route to host",
                "route",
            ),
        ];
        for (stderr, word) in cases {
            match classify(Some(255), stderr) {
                SshError::Unreachable { detail } => assert!(detail.contains(word), "{detail}"),
                other => panic!("{stderr}: {other:?}"),
            }
        }
        assert_eq!(
            classify(Some(255), "Connection closed by 1.2.3.4 port 22\n"),
            SshError::ConnectionFailed {
                detail: "Connection closed by 1.2.3.4 port 22".into()
            }
        );
        assert_eq!(
            classify(Some(255), ""),
            SshError::ConnectionFailed {
                detail: "ssh exited with status 255".into()
            }
        );
    }

    #[test]
    fn last_lines_bounds_output() {
        assert_eq!(last_lines("a\n\nb\n c \n", 2), "b c");
        assert_eq!(last_lines("", 3), "");
        assert!(last_lines(&"x".repeat(1000), 1).ends_with('…'));
    }
}
