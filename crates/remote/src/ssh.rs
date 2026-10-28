//! Running commands on a server through the user's own `ssh`.
//!
//! Always `BatchMode=yes`: ssh uses the user's config, keys and agent, and
//! fails instead of prompting, so Godspeed never sees or stores a password.
//! Host keys are checked as the user's ssh config says (an unknown host fails
//! with [`SshError::HostKeyUnknown`]; nothing is trusted silently).

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::errors::{SshError, classify};
use crate::target::SshTarget;

/// Options every invocation gets (before the port and destination).
pub const BASE_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-T",
];

/// Spawns `ssh`. The program and its environment are configurable so tests can
/// put a fake `ssh` first on `PATH`.
#[derive(Debug, Clone)]
pub struct SshRunner {
    program: OsString,
    env: Vec<(OsString, OsString)>,
}

impl Default for SshRunner {
    fn default() -> Self {
        Self {
            program: OsString::from("ssh"),
            env: Vec::new(),
        }
    }
}

/// What a finished remote command printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

impl SshRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use another executable (resolved on `PATH` when it has no slash).
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Set an environment variable for ssh. Setting `PATH` also changes where
    /// a bare program name is looked up.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The full argument list for `remote_command` on `target`.
    pub fn args(&self, target: &SshTarget, remote_command: &str) -> Vec<String> {
        let mut args: Vec<String> = BASE_OPTIONS.iter().map(|s| (*s).to_owned()).collect();
        args.extend(target.port_args());
        args.push(target.destination());
        args.push(remote_command.to_owned());
        args
    }

    fn command(&self, target: &SshTarget, remote_command: &str) -> Command {
        let mut command = Command::new(&self.program);
        command.args(self.args(target, remote_command));
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    /// Run `script` with `sh -s` on the server (the script travels on stdin,
    /// so it never passes through the remote login shell's quoting). Returns
    /// stdout on exit 0.
    pub fn run_script(
        &self,
        target: &SshTarget,
        script: &str,
        timeout: Duration,
    ) -> Result<Output, SshError> {
        let mut child = self
            .command(target, "sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| spawn_error(&err, &self.program))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let script = script.to_owned();
        // A writer thread: a large script must not deadlock against a full
        // stdout pipe.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(script.as_bytes());
        });
        let result = wait_with_timeout(child, timeout);
        let _ = writer.join();
        let (code, output) = result?;
        if code == Some(0) {
            Ok(output)
        } else {
            Err(classify(code, &output.stderr))
        }
    }

    /// Start `remote_command` with stdin, stdout and stderr piped, for an
    /// exchange like sign-in. The caller owns the child.
    pub fn spawn(&self, target: &SshTarget, remote_command: &str) -> Result<Child, SshError> {
        self.command(target, remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| spawn_error(&err, &self.program))
    }
}

fn spawn_error(err: &std::io::Error, program: &OsStr) -> SshError {
    if err.kind() == std::io::ErrorKind::NotFound {
        SshError::SshMissing
    } else {
        SshError::Spawn(format!("{}: {err}", program.to_string_lossy()))
    }
}

/// Collect stdout and stderr on threads and wait, killing the child at the
/// deadline.
fn wait_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> Result<(Option<i32>, Output), SshError> {
    let stdout = child.stdout.take().map(read_to_string_thread);
    let stderr = child.stderr.take().map(read_to_string_thread);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(err) => return Err(SshError::Spawn(err.to_string())),
        }
    };
    let Some(status) = status else {
        // Not joined: a grandchild may still hold the pipes open, and the
        // reader threads end on their own when it goes.
        return Err(SshError::Timeout {
            secs: timeout.as_secs().max(1),
        });
    };
    let join = |handle: Option<std::thread::JoinHandle<String>>| {
        handle.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    let output = Output {
        stdout: join(stdout),
        stderr: join(stderr),
    };
    Ok((status.code(), output))
}

fn read_to_string_thread<R: Read + Send + 'static>(
    mut reader: R,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Test support: a fake `ssh` that records its arguments and stdin and plays
/// back scripted output. Shared by this crate's tests.
#[cfg(all(test, unix))]
pub(crate) mod fake {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    pub struct FakeSsh {
        pub dir: tempfile::TempDir,
    }

    impl FakeSsh {
        /// `ssh` in a temp dir: appends its argv (one per line, then `--`) to
        /// `calls`, its stdin to `stdin`, then prints `stdout`, prints `stderr`
        /// to stderr and exits with `code` (files in the same dir, optional).
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = r#"#!/bin/sh
here="$(dirname "$0")"
for arg in "$@"; do printf '%s\n' "$arg" >> "$here/calls"; done
printf -- '--\n' >> "$here/calls"
if [ -f "$here/interactive" ]; then
  # Sign-in mode: print the URL, then answer each stdin line.
  printf 'godspeed-login: url https://edge.example/auth?state=abc\n'
  while IFS= read -r line; do
    printf '%s\n' "$line" >> "$here/stdin"
    case "$line" in
      good.*) printf 'godspeed-login: signed-in me@example.com org_1\n'; exit 0 ;;
      *) printf 'godspeed-login: error invalid or expired sign-in code\n' ;;
    esac
  done
  exit 1
fi
cat >> "$here/stdin"
[ -f "$here/stdout" ] && cat "$here/stdout"
[ -f "$here/stderr" ] && cat "$here/stderr" >&2
[ -f "$here/sleep" ] && exec sleep "$(cat "$here/sleep")"
exit "$(cat "$here/code" 2>/dev/null || echo 0)"
"#;
            let path = dir.path().join("ssh");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir }
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        pub fn set(&self, name: &str, contents: &str) {
            std::fs::write(self.dir.path().join(name), contents).unwrap();
        }

        /// A runner that finds this fake as `ssh` on PATH.
        pub fn runner(&self) -> SshRunner {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs: Vec<PathBuf> = vec![self.dir.path().to_path_buf()];
            dirs.extend(std::env::split_paths(&path));
            SshRunner::new().with_env("PATH", std::env::join_paths(dirs).unwrap())
        }

        /// Every recorded invocation's argv.
        pub fn calls(&self) -> Vec<Vec<String>> {
            let text = std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default();
            let mut calls = Vec::new();
            let mut current = Vec::new();
            for line in text.lines() {
                if line == "--" {
                    calls.push(std::mem::take(&mut current));
                } else {
                    current.push(line.to_owned());
                }
            }
            calls
        }

        pub fn stdin(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("stdin")).unwrap_or_default()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::fake::FakeSsh;
    use super::*;

    fn target() -> SshTarget {
        SshTarget::parse("deploy@vps.example.com:2222").unwrap()
    }

    #[test]
    fn args_force_batch_mode_and_put_the_port_before_the_destination() {
        let args = SshRunner::new().args(&target(), "sh -s");
        assert_eq!(
            args,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-T",
                "-p",
                "2222",
                "deploy@vps.example.com",
                "sh -s",
            ]
        );
    }

    #[test]
    fn scripts_travel_on_stdin_and_stdout_comes_back() {
        let fake = FakeSsh::new();
        fake.set("stdout", "os=Linux\n");
        let out = fake
            .runner()
            .run_script(&target(), "echo hi\n", Duration::from_secs(10))
            .unwrap();
        assert_eq!(out.stdout, "os=Linux\n");
        assert_eq!(fake.stdin(), "echo hi\n");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].last().unwrap(), "sh -s");
        assert!(calls[0].contains(&"BatchMode=yes".to_owned()));
    }

    #[test]
    fn ssh_failures_are_classified() {
        let fake = FakeSsh::new();
        fake.set("code", "255");
        fake.set(
            "stderr",
            "deploy@vps.example.com: Permission denied (publickey).\n",
        );
        let err = fake
            .runner()
            .run_script(&target(), "true", Duration::from_secs(10))
            .unwrap_err();
        assert_eq!(err, SshError::AuthFailed);

        fake.set("code", "4");
        fake.set("stderr", "godspeed-install: error checksum-mismatch\n");
        let err = fake
            .runner()
            .run_script(&target(), "true", Duration::from_secs(10))
            .unwrap_err();
        assert!(matches!(err, SshError::Remote { code: Some(4), .. }));
    }

    #[test]
    fn a_hung_command_is_stopped_at_the_deadline() {
        let fake = FakeSsh::new();
        fake.set("sleep", "5");
        let started = Instant::now();
        let err = fake
            .runner()
            .run_script(&target(), "true", Duration::from_millis(300))
            .unwrap_err();
        assert!(matches!(err, SshError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_missing_ssh_says_so() {
        let err = SshRunner::new()
            .with_program("/nonexistent/godspeed-test-ssh")
            .run_script(&target(), "true", Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(err, SshError::SshMissing);
    }
}
