//! Signing the server into the user's workspace.
//!
//! The server runs `godspeed login --stdin-code [--org <id>]` over SSH. It
//! prints the sign-in URL (`godspeed-login: url …`); the desktop opens it in
//! the user's browser, where they sign in as usual and get a one-time code;
//! the code goes back over the SSH channel's stdin. The server exchanges it
//! itself and keeps its own session, so no token ever leaves the Mac and the
//! server's refresh token never touches it. `--org` selects the same
//! workspace the desktop is in.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use crate::errors::{SshError, classify, last_lines};
use crate::scripts::shell_quote;
use crate::ssh::SshRunner;
use crate::target::SshTarget;

/// The remote command (run by the login shell, so kept to one line of
/// portable quoting).
pub fn login_command(org_id: Option<&str>) -> String {
    let mut command = String::from("\"$HOME/.godspeed/app/current/godspeed\" login --stdin-code");
    if let Some(org) = org_id {
        command.push_str(" --org ");
        command.push_str(&shell_quote(org));
    }
    command
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    /// Open this in the browser.
    Url(String),
    /// The pasted code was rejected; another can be sent.
    CodeRejected(String),
    /// Done: signed in (or already was) as `email`, in `org`.
    SignedIn { email: String, org: Option<String> },
    /// The server has no sign-in configured (dev build): nothing to do.
    NotNeeded,
    /// The command ended without signing in.
    Failed(String),
}

/// Parse one stdout line of `godspeed login --stdin-code`.
pub fn parse_line(line: &str) -> Option<LoginEvent> {
    let rest = line.trim().strip_prefix("godspeed-login: ")?;
    let (kind, value) = rest.split_once(' ').unwrap_or((rest, ""));
    let value = value.trim();
    match kind {
        "url" if !value.is_empty() => Some(LoginEvent::Url(value.to_owned())),
        "error" => Some(LoginEvent::CodeRejected(value.to_owned())),
        "signed-in" | "already-signed-in" => {
            let mut parts = value.split_whitespace();
            let email = parts.next()?.to_owned();
            let org = parts.next().filter(|org| *org != "-").map(str::to_owned);
            Some(LoginEvent::SignedIn { email, org })
        }
        "dev-mode" => Some(LoginEvent::NotNeeded),
        _ => None,
    }
}

/// A pasted code is `state.code`: one token, no whitespace. Anything else is
/// refused before it reaches the server.
pub fn valid_code(code: &str) -> bool {
    let code = code.trim();
    !code.is_empty()
        && code.len() <= 4096
        && code.contains('.')
        && !code.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A running remote sign-in.
pub struct LoginSession {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<LoginEvent>,
}

impl LoginSession {
    pub fn start(
        runner: &SshRunner,
        target: &SshTarget,
        org_id: Option<&str>,
    ) -> Result<Self, SshError> {
        let mut child = runner.spawn(target, &login_command(org_id))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, events) = channel();
        let stderr_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            text
        });
        std::thread::spawn(move || {
            let mut finished = false;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(event) = parse_line(&line) {
                    finished |=
                        matches!(event, LoginEvent::SignedIn { .. } | LoginEvent::NotNeeded);
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
            if !finished {
                let stderr = stderr_reader.join().unwrap_or_default();
                let _ = tx.send(LoginEvent::Failed(stderr));
            }
        });
        Ok(Self {
            child,
            stdin,
            events,
        })
    }

    /// The next event, or `None` if nothing arrived within `timeout`.
    pub fn next_event(&mut self, timeout: Duration) -> Option<LoginEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(LoginEvent::Failed(stderr)) => {
                let code = self.child.wait().ok().and_then(|status| status.code());
                Some(LoginEvent::Failed(failure_message(code, &stderr)))
            }
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                Some(LoginEvent::Failed("the sign-in ended unexpectedly".into()))
            }
        }
    }

    /// Send the code the browser showed. Never logged.
    pub fn send_code(&mut self, code: &str) -> Result<(), String> {
        if !valid_code(code) {
            return Err("Paste the whole code the browser shows (it contains a dot).".into());
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "the sign-in has ended".to_owned())?;
        stdin
            .write_all(format!("{}\n", code.trim()).as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("couldn't send the code: {err}"))
    }

    /// Stop the remote sign-in.
    pub fn cancel(mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LoginSession {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn failure_message(code: Option<i32>, stderr: &str) -> String {
    if code == Some(255) {
        return classify(code, stderr).user_message("the server");
    }
    if stderr.contains("while an engine is running") {
        return "The engine on the server is running, and it must be stopped to sign in. Stop it, sign in, then start it again.".into();
    }
    if stderr.contains("unexpected argument '--stdin-code'") {
        return "The Godspeed on the server is too old to sign in this way. Update it first."
            .into();
    }
    let tail = last_lines(stderr, 2);
    if tail.is_empty() {
        "Sign-in on the server didn't finish.".into()
    } else {
        format!("Sign-in on the server failed: {tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_quotes_the_org() {
        assert_eq!(
            login_command(None),
            "\"$HOME/.godspeed/app/current/godspeed\" login --stdin-code"
        );
        assert_eq!(
            login_command(Some("org_01ABC")),
            "\"$HOME/.godspeed/app/current/godspeed\" login --stdin-code --org org_01ABC"
        );
        assert!(login_command(Some("x; reboot")).ends_with("--org 'x; reboot'"));
    }

    #[test]
    fn parses_protocol_lines() {
        assert_eq!(
            parse_line("godspeed-login: url https://edge/auth?x=1"),
            Some(LoginEvent::Url("https://edge/auth?x=1".into()))
        );
        assert_eq!(
            parse_line("godspeed-login: signed-in me@example.com org_1"),
            Some(LoginEvent::SignedIn {
                email: "me@example.com".into(),
                org: Some("org_1".into())
            })
        );
        assert_eq!(
            parse_line("godspeed-login: already-signed-in me@example.com -"),
            Some(LoginEvent::SignedIn {
                email: "me@example.com".into(),
                org: None
            })
        );
        assert_eq!(
            parse_line("godspeed-login: error invalid code"),
            Some(LoginEvent::CodeRejected("invalid code".into()))
        );
        assert_eq!(
            parse_line("godspeed-login: dev-mode"),
            Some(LoginEvent::NotNeeded)
        );
        assert_eq!(parse_line("Welcome to Ubuntu"), None);
        assert_eq!(parse_line("godspeed-login: url"), None);
    }

    #[test]
    fn codes_are_single_tokens() {
        assert!(valid_code(" 3f2a-uuid.abcDEF123 \n"));
        assert!(!valid_code(""));
        assert!(!valid_code("nodot"));
        assert!(!valid_code("a.b c"));
        assert!(!valid_code("a.b\nrm -rf ~"));
    }

    #[test]
    fn failures_explain_themselves() {
        assert!(
            failure_message(
                Some(1),
                "Error: lock held\nCannot sign in while an engine is running"
            )
            .contains("must be stopped")
        );
        assert!(
            failure_message(Some(2), "error: unexpected argument '--stdin-code' found")
                .contains("too old")
        );
        assert!(failure_message(Some(255), "Permission denied (publickey).").contains("SSH keys"));
    }

    #[cfg(unix)]
    #[test]
    fn a_full_sign_in_through_a_fake_ssh() {
        let fake = crate::ssh::fake::FakeSsh::new();
        fake.set("interactive", "");
        let target = SshTarget::parse("deploy@box").unwrap();
        let mut session = LoginSession::start(&fake.runner(), &target, Some("org_1")).unwrap();
        assert_eq!(
            session.next_event(Duration::from_secs(10)),
            Some(LoginEvent::Url(
                "https://edge.example/auth?state=abc".into()
            ))
        );
        assert!(session.send_code("not a code").is_err());
        session.send_code("bad.code").unwrap();
        assert!(matches!(
            session.next_event(Duration::from_secs(10)),
            Some(LoginEvent::CodeRejected(_))
        ));
        session.send_code("good.code").unwrap();
        assert_eq!(
            session.next_event(Duration::from_secs(10)),
            Some(LoginEvent::SignedIn {
                email: "me@example.com".into(),
                org: Some("org_1".into())
            })
        );
        // The code went over stdin, never on the command line.
        assert_eq!(fake.stdin(), "bad.code\ngood.code\n");
        let call = &fake.calls()[0];
        assert!(!call.iter().any(|arg| arg.contains("good.code")));
        assert_eq!(
            call.last().unwrap(),
            "\"$HOME/.godspeed/app/current/godspeed\" login --stdin-code --org org_1"
        );
    }
}
