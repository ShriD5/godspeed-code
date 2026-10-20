//! Login-shell PATH snapshot.
//!
//! GUI/service launches (Dock, Finder, launchd, systemd) never run the user's
//! shell init, so the daemon's own PATH misses everything the shell shapes:
//! nvm's shell function, fnm multishells, asdf/mise shims, custom npm
//! prefixes, nix profiles, `~/.zshrc` exports. The hardcoded known-location
//! lists in the resolvers cover the common managers, but the only fix that
//! works for *any* setup is asking the user's actual shell: spawn it once as
//! an interactive login shell, have it print its environment between markers,
//! and keep the PATH it reports. If `codex`/`claude` runs in their terminal,
//! it resolves here too.
//!
//! The snapshot is captured once per process and cached, including a
//! definitive negative result (no usable shell, no PATH in its output). A
//! TIMEOUT is not an answer: heavy init (a slow corporate profile, a cold
//! disk after login) can outlast the first attempt, and caching that as
//! "no PATH" left every agent "not found" until restart. A timed-out
//! capture reads as unknown for now and is retried in the background with
//! a longer budget. It is defensive about hostile shell init:
//! - `-lic` first (interactive login — nvm and friends load in rc files),
//!   falling back to `-lc` if that produces nothing (some rc files hang or
//!   `exec` a multiplexer when interactive).
//! - Output is read on a side thread into a shared buffer; the poll loop
//!   returns as soon as the end marker appears, so init that blocks *after*
//!   printing (or grandchildren inheriting the pipe) can't wedge us.
//! - The shell runs in its own session, so an interactive shell can't take
//!   over the terminal Godspeed was launched from, and a hard per-attempt
//!   timeout kills it with everything it started.
//!
//! Set `GODSPEED_NO_LOGIN_SHELL=1` to disable the snapshot entirely.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

/// The settled answer: a PATH, or definitively none.
static CACHE: OnceLock<Option<OsString>> = OnceLock::new();

/// The PATH the user's login shell reports, captured once and cached for the
/// life of the process. `None` when disabled, non-unix, no usable shell, the
/// shell never produced a parseable snapshot, or — for now — the capture
/// timed out and a background retry is pending.
pub fn login_shell_path() -> Option<&'static OsStr> {
    #[cfg(unix)]
    {
        if let Some(settled) = CACHE.get() {
            return settled.as_deref();
        }
        unix::resolve(&CACHE)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Kick off the snapshot on a background thread so the first harness resolve
/// doesn't pay the shell-startup latency inline. Call at daemon startup.
pub fn prewarm() {
    #[cfg(unix)]
    {
        let _ = std::thread::Builder::new()
            .name("godspeed-shell-env".into())
            .spawn(|| {
                let _ = login_shell_path();
            });
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::{OsStr, OsString};
    use std::io::Read;
    use std::sync::OnceLock;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const BEGIN_MARKER: &str = "__GODSPEED_SHELL_ENV_BEGIN__";
    const END_MARKER: &str = "__GODSPEED_SHELL_ENV_END__";
    /// Enough for any sane environment; a runaway rc file can't OOM us.
    const MAX_OUTPUT: usize = 2 * 1024 * 1024;
    const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
    /// Background retries after a timeout get a longer budget.
    const RETRY_TIMEOUT: Duration = Duration::from_secs(20);
    const RETRY_FIRST_DELAY: Duration = Duration::from_secs(30);
    const RETRY_MAX_DELAY: Duration = Duration::from_secs(600);
    /// After the shell exits, wait this long for the pipe to flush.
    const EXIT_FLUSH_GRACE: Duration = Duration::from_millis(250);

    /// What one capture learned.
    #[derive(Debug, PartialEq)]
    pub(super) enum Snapshot {
        Path(OsString),
        /// The shell answered, or can't be asked: there is no PATH to add.
        NoPath,
        /// Every attempt ran out of time: unknown, ask again later.
        TimedOut,
    }

    #[derive(Default)]
    struct Retry {
        /// The first (synchronous) capture has run.
        started: bool,
        attempts: u32,
        retry_at: Option<Instant>,
        running: bool,
    }

    static RETRY: Mutex<Retry> = Mutex::new(Retry {
        started: false,
        attempts: 0,
        retry_at: None,
        running: false,
    });

    fn retry_delay(attempts: u32) -> Duration {
        RETRY_FIRST_DELAY
            .saturating_mul(1 << attempts.saturating_sub(1).min(5))
            .min(RETRY_MAX_DELAY)
    }

    /// The first caller captures inline (concurrent first callers wait for
    /// it, as before); a timeout schedules background retries, and callers
    /// meanwhile get `None` at once instead of paying the shell again.
    pub(super) fn resolve(cache: &'static OnceLock<Option<OsString>>) -> Option<&'static OsStr> {
        let mut retry = RETRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(settled) = cache.get() {
            return settled.as_deref();
        }
        if !retry.started {
            retry.started = true;
            match capture(ATTEMPT_TIMEOUT) {
                Snapshot::TimedOut => {
                    retry.attempts = 1;
                    retry.retry_at = Some(Instant::now() + retry_delay(1));
                    return None;
                }
                settled => {
                    let _ = cache.set(match settled {
                        Snapshot::Path(path) => Some(path),
                        _ => None,
                    });
                    return cache.get().and_then(|s| s.as_deref());
                }
            }
        }
        if !retry.running && retry.retry_at.is_some_and(|at| Instant::now() >= at) {
            retry.running = true;
            let spawned = std::thread::Builder::new()
                .name("godspeed-shell-env-retry".into())
                .spawn(move || {
                    let outcome = capture(RETRY_TIMEOUT);
                    let mut retry = RETRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    retry.running = false;
                    match outcome {
                        Snapshot::TimedOut => {
                            retry.attempts += 1;
                            retry.retry_at = Some(Instant::now() + retry_delay(retry.attempts));
                            tracing::warn!(
                                attempts = retry.attempts,
                                "login shell PATH capture timed out again; retrying later"
                            );
                        }
                        Snapshot::Path(path) => {
                            let _ = cache.set(Some(path));
                        }
                        Snapshot::NoPath => {
                            let _ = cache.set(None);
                        }
                    }
                });
            if spawned.is_err() {
                retry.running = false;
            }
        }
        None
    }

    fn capture(timeout: Duration) -> Snapshot {
        if std::env::var_os("GODSPEED_NO_LOGIN_SHELL").is_some_and(|v| !v.is_empty()) {
            return Snapshot::NoPath;
        }
        let Some(shell) = user_shell() else {
            return Snapshot::NoPath;
        };
        snapshot(&shell, timeout)
    }

    /// The user's shell: `$SHELL`, then the passwd entry, then well-known
    /// defaults. Non-executables and nologin shells are skipped.
    fn user_shell() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            candidates.push(PathBuf::from(s));
        }
        // systemd/launchd services often start without SHELL — passwd has it.
        if let Some(p) = passwd_shell() {
            candidates.push(p);
        }
        candidates.push(PathBuf::from("/bin/zsh"));
        candidates.push(PathBuf::from("/bin/bash"));
        candidates.push(PathBuf::from("/bin/sh"));
        candidates.into_iter().find(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string());
            let blocked = matches!(name.as_deref(), Some("nologin" | "false") | None);
            !blocked && is_executable(p)
        })
    }

    fn passwd_shell() -> Option<PathBuf> {
        // SAFETY: getpwuid's static buffer is only read here, and callers are
        // serialized through the OnceLock init above.
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() || (*pw).pw_shell.is_null() {
                return None;
            }
            let shell = std::ffi::CStr::from_ptr((*pw).pw_shell);
            (!shell.to_bytes().is_empty())
                .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(shell.to_bytes())))
        }
    }

    fn is_executable(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    fn shell_name(shell: &Path) -> String {
        shell
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// Flag sets to try, most-loaded first. csh/tcsh reject `-l` combined with
    /// `-c`; fish runs config.fish for every invocation, so `-l` alone loads
    /// everything without interactive-mode side effects; nushell's `-l` loads
    /// its env and config files for a `-c` command too.
    fn attempt_flag_sets(shell: &Path) -> Vec<Vec<&'static str>> {
        match shell_name(shell).as_str() {
            "csh" | "tcsh" => vec![vec!["-c"]],
            "fish" | "nu" => vec![vec!["-l", "-c"], vec!["-c"]],
            _ => vec![vec!["-l", "-i", "-c"], vec!["-l", "-c"]],
        }
    }

    /// The probe command in the shell's own language. Nushell prints only a
    /// pipeline's final value and keeps PATH as a list, so it prints the
    /// markers explicitly and lets an external `env` render the environment
    /// the way any child process sees it.
    fn probe_script(shell: &Path) -> String {
        match shell_name(shell).as_str() {
            "nu" => format!("print '{BEGIN_MARKER}'; print (^env); print '{END_MARKER}'"),
            _ => format!("echo {BEGIN_MARKER}; env; echo {END_MARKER}"),
        }
    }

    /// Run the probe per flag set until one yields a parseable PATH.
    pub(super) fn snapshot(shell: &Path, timeout: Duration) -> Snapshot {
        let script = probe_script(shell);
        let mut timed_out = false;
        for flags in attempt_flag_sets(shell) {
            let (output, expired) = run_and_capture(shell, &flags, &script, timeout);
            if let Some(path) = parse_snapshot_path(&output) {
                return Snapshot::Path(path);
            }
            timed_out |= expired;
        }
        if timed_out {
            Snapshot::TimedOut
        } else {
            Snapshot::NoPath
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot_path(shell: &Path, timeout: Duration) -> Option<OsString> {
        match snapshot(shell, timeout) {
            Snapshot::Path(path) => Some(path),
            _ => None,
        }
    }

    /// Spawn the shell and collect stdout until the end marker appears, the
    /// child exits (plus a flush grace), or the timeout kills it. The reader
    /// lives on its own thread appending into a shared buffer, so a shell that
    /// blocks after printing — or a grandchild that inherits the pipe and
    /// never closes it — can't hang us on EOF.
    /// Also reports whether the attempt was cut off by the timeout.
    fn run_and_capture(
        shell: &Path,
        flags: &[&str],
        script: &str,
        timeout: Duration,
    ) -> (Vec<u8>, bool) {
        let mut cmd = std::process::Command::new(shell);
        cmd.args(flags)
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Let rc files detect (and skip work for) this probe, mirroring
            // VSCODE_RESOLVING_ENVIRONMENT; TERM=dumb quiets fancy prompts.
            .env("GODSPEED_RESOLVING_ENVIRONMENT", "1")
            .env("TERM", "dumb");
        // An interactive zsh opens /dev/tty when stdin isn't one and takes the
        // foreground from whatever terminal launched us, then stops on SIGTTOU
        // or leaves that terminal suspended. Its own session has no
        // controlling terminal, and its process group dies with it.
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let Ok(mut child) = cmd.spawn() else {
            return (Vec::new(), false);
        };
        let group = child.id() as libc::pid_t;
        let kill = |child: &mut std::process::Child| {
            // SAFETY: the group is the shell's own session, created above.
            unsafe { libc::kill(-group, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
        };
        let mut timed_out = false;
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        if let Some(mut stdout) = child.stdout.take() {
            let buf = Arc::clone(&buf);
            let _ = std::thread::Builder::new()
                .name("godspeed-shell-env-read".into())
                .spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match stdout.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut b = buf
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                if b.len() >= MAX_OUTPUT {
                                    break;
                                }
                                b.extend_from_slice(&chunk[..n]);
                            }
                        }
                    }
                });
        }
        let deadline = Instant::now() + timeout;
        let mut exited_at: Option<Instant> = None;
        let mut scanned = 0usize;
        loop {
            {
                let b = buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Only scan the unscanned tail (minus marker-length overlap).
                let from = scanned.saturating_sub(END_MARKER.len());
                if find_subslice(&b[from..], END_MARKER.as_bytes()).is_some() {
                    break;
                }
                scanned = b.len();
            }
            match exited_at {
                Some(at) if at.elapsed() >= EXIT_FLUSH_GRACE => break,
                Some(_) => {}
                None => match child.try_wait() {
                    Ok(Some(_)) => exited_at = Some(Instant::now()),
                    Ok(None) if Instant::now() >= deadline => {
                        kill(&mut child);
                        timed_out = true;
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                },
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if exited_at.is_none() {
            kill(&mut child);
        }
        let b = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (b.clone(), timed_out)
    }

    /// Extract PATH from the `env` dump between the LAST begin marker and the
    /// first end marker after it (rc noise printed before our command — or a
    /// marker echoed by init itself — lands before the real one).
    fn parse_snapshot_path(output: &[u8]) -> Option<OsString> {
        let begin = rfind_subslice(output, BEGIN_MARKER.as_bytes())?;
        let after = &output[begin + BEGIN_MARKER.len()..];
        let end = find_subslice(after, END_MARKER.as_bytes())?;
        for line in after[..end].split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(value) = line.strip_prefix(b"PATH=")
                && !value.is_empty()
            {
                return Some(OsString::from_vec(value.to_vec()));
            }
        }
        None
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .rposition(|window| window == needle)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn fake_shell(dir: &Path, body: &str) -> PathBuf {
            let path = dir.join("fake-shell");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// A fake $SHELL skeleton: consume flags, exec the `-c` payload.
        const RUN_PAYLOAD: &str = r#"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-c" ]; then shift; exec /bin/sh -c "$1"; fi
  shift
done
exit 1
"#;

        #[test]
        fn parses_path_between_markers() {
            let output = format!(
                "rc noise\n{BEGIN_MARKER}\nHOME=/home/u\nPATH=/custom/bin:/usr/bin\nX=y\n{END_MARKER}\ntrailing"
            );
            let path = parse_snapshot_path(output.as_bytes()).unwrap();
            assert_eq!(path, OsString::from("/custom/bin:/usr/bin"));
        }

        #[test]
        fn ignores_marker_echoed_by_init() {
            // rc noise that happens to contain the begin marker but no PATH
            // after it must not shadow the real snapshot.
            let output =
                format!("{BEGIN_MARKER}\ngarbage\n{BEGIN_MARKER}\nPATH=/real/bin\n{END_MARKER}\n");
            let path = parse_snapshot_path(output.as_bytes()).unwrap();
            assert_eq!(path, OsString::from("/real/bin"));
        }

        #[test]
        fn snapshots_path_from_fake_shell() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh\nPATH=\"/godspeed-test/custom/bin:/usr/bin:/bin\"; export PATH\n{RUN_PAYLOAD}"
                ),
            );
            let path = snapshot_path(&shell, Duration::from_secs(10)).unwrap();
            let path = path.to_string_lossy();
            assert!(path.starts_with("/godspeed-test/custom/bin:"), "got: {path}");
        }

        #[test]
        fn falls_back_when_interactive_attempt_hangs() {
            let dir = tempfile::tempdir().unwrap();
            // Simulates rc files that wedge only in interactive mode (`exec
            // tmux` and friends): sleep forever when -i is present.
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh\ncase \" $* \" in *\" -i \"*) sleep 60;; esac\nPATH=\"/godspeed-test/fallback/bin:/usr/bin:/bin\"; export PATH\n{RUN_PAYLOAD}"
                ),
            );
            let start = Instant::now();
            let path = snapshot_path(&shell, Duration::from_millis(400)).unwrap();
            assert!(
                path.to_string_lossy()
                    .starts_with("/godspeed-test/fallback/bin"),
                "got: {}",
                path.to_string_lossy()
            );
            // First attempt burned ~400ms then was killed; the whole resolve
            // must not have waited out the sleep.
            assert!(start.elapsed() < Duration::from_secs(5));
        }

        #[test]
        fn gives_up_on_a_shell_that_never_answers() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(dir.path(), "#!/bin/sh\nsleep 60\n");
            let start = Instant::now();
            // Unknown, not "no PATH": the caller retries later (T3 Code's
            // #10534 cached this as success and lost PATH until restart).
            assert_eq!(snapshot(&shell, Duration::from_millis(300)), Snapshot::TimedOut);
            assert!(start.elapsed() < Duration::from_secs(5));
            // A shell that answers without a PATH is a real answer.
            let quiet = fake_shell(dir.path(), "#!/bin/sh\necho hello\n");
            assert_eq!(snapshot(&quiet, Duration::from_secs(5)), Snapshot::NoPath);
        }

        /// pingdotgg/t3code@7f63b10275: an interactive shell must not see the
        /// terminal Godspeed was launched from. Leading its own process group
        /// means it leads its own session, with no controlling terminal.
        #[test]
        fn probe_runs_in_its_own_session() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh
echo {BEGIN_MARKER}
echo PATH=$(ps -o pgid= -p $$ | tr -d ' ')/$$
echo {END_MARKER}
"
                ),
            );
            let Snapshot::Path(path) = snapshot(&shell, Duration::from_secs(10)) else {
                panic!("no snapshot");
            };
            let path = path.to_string_lossy();
            let (group, pid) = path.split_once('/').unwrap();
            assert_eq!(group, pid);
        }

        #[test]
        fn timeout_kills_what_the_shell_started() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("sleeper.pid");
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh
sleep 60 >/dev/null &
echo $! > {}
wait
",
                    pid_file.display()
                ),
            );
            assert_eq!(snapshot(&shell, Duration::from_millis(300)), Snapshot::TimedOut);
            let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            // SAFETY: signal 0 only probes whether the pid still exists.
            while unsafe { libc::kill(pid, 0) } == 0 {
                assert!(Instant::now() < deadline, "sleep {pid} outlived the probe");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn retries_back_off_and_cap() {
            assert_eq!(retry_delay(1), Duration::from_secs(30));
            assert_eq!(retry_delay(2), Duration::from_secs(60));
            assert_eq!(retry_delay(9), RETRY_MAX_DELAY);
        }

        /// pingdotgg/t3code#7846: nushell rejects the POSIX probe and keeps
        /// PATH as a list. It gets `nu -l -c` and a probe in its own syntax
        /// whose external `env` prints PATH the way children see it.
        #[test]
        fn nushell_gets_its_own_probe() {
            let dir = tempfile::tempdir().unwrap();
            let shell = dir.path().join("nu");
            std::fs::write(
                &shell,
                format!(
                    "#!/bin/sh\n[ \"$1\" = -l ] && [ \"$2\" = -c ] || exit 1\n\
                     [ \"$3\" = \"print '{BEGIN_MARKER}'; print (^env); print '{END_MARKER}'\" ] || exit 1\n\
                     echo {BEGIN_MARKER}; echo PATH=/nu/bin:/usr/bin; echo {END_MARKER}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                snapshot(&shell, Duration::from_secs(5)),
                Snapshot::Path("/nu/bin:/usr/bin".into())
            );
        }
    }
}
