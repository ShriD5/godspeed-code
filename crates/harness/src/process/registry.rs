//! A record of the agent processes this engine runs, so the next engine can
//! end the ones a crash left behind.
//!
//! Agent CLIs outlive an engine that dies abruptly (SIGKILL, a crash, a
//! power-user `kill -9`): nothing reaps them, they are reparented to launchd
//! or `systemd --user`, and they keep running tools — editing files, pushing,
//! filing issues — for a chat the restarted engine shows as stopped. Each run
//! records its agent's pid, process start time and process-group status in a
//! small file that its registration removes on drop. At start the engine
//! ends every recorded agent whose engine is gone, after checking the pid
//! still names the same process (pids recycle), and its descendants with it.
//!
//! Unix only: Windows agents run in job objects that end with the engine.

use std::path::PathBuf;
#[cfg(unix)]
use std::path::Path;

/// Removes its record when the agent's run ends normally.
#[derive(Debug)]
pub(crate) struct Registration(PathBuf);

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn records_dir() -> PathBuf {
    #[cfg(unix)]
    // SAFETY: getuid has no failure mode.
    let name = format!("godspeed-agent-pids-{}", unsafe { libc::getuid() });
    #[cfg(not(unix))]
    let name = "godspeed-agent-pids".to_owned();
    std::env::temp_dir().join(name)
}

/// A process's start time as `ps` reports it: with the pid, it names one
/// process for good.
#[cfg(unix)]
fn start_time(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !started.is_empty()).then_some(started)
}

#[cfg(unix)]
fn engine_start() -> Option<&'static str> {
    static START: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    START
        .get_or_init(|| start_time(std::process::id()))
        .as_deref()
}

/// Record a freshly spawned agent. `None` when the record can't be written:
/// the run goes ahead, only unprotected against an engine crash.
pub(crate) fn register(pid: Option<u32>, label: &str) -> Option<Registration> {
    #[cfg(unix)]
    {
        register_in(&records_dir(), pid?, label)
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, label);
        None
    }
}

#[cfg(unix)]
fn register_in(dir: &Path, pid: u32, label: &str) -> Option<Registration> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).ok()?;
    let record = serde_json::json!({
        "engine": std::process::id(),
        "engineStarted": engine_start()?,
        "pid": pid,
        "started": start_time(pid)?,
        "label": label,
    });
    let path = dir.join(format!("{}-{pid}.json", std::process::id()));
    std::fs::write(&path, record.to_string()).ok()?;
    Some(Registration(path))
}

/// End the agents a dead engine left running. Runs on its own thread: the
/// engine starts without waiting on `ps`.
pub fn reap_orphaned_agents() {
    #[cfg(unix)]
    {
        let _ = std::thread::Builder::new()
            .name("godspeed-reap-agents".into())
            .spawn(|| {
                let reaped = reap_in(&records_dir());
                if reaped > 0 {
                    tracing::warn!(reaped, "ended agent processes left by an engine that exited abruptly");
                }
            });
    }
}

/// Returns how many recorded agents were ended.
#[cfg(unix)]
fn reap_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let own = std::process::id();
    let mut reaped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(record) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let field = |key: &str| record.get(key).and_then(serde_json::Value::as_str);
        let number = |key: &str| {
            record
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        let (Some(engine), Some(pid)) = (number("engine"), number("pid")) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        // A live engine (this one, or another instance) owns its agents.
        if engine == own || start_time(engine).as_deref() == field("engineStarted") {
            continue;
        }
        // Gone, or the pid now names some other process: nothing to end.
        if start_time(pid).is_none() || start_time(pid).as_deref() != field("started") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        end_tree(pid);
        reaped += 1;
        let _ = std::fs::remove_file(&path);
    }
    reaped
}

/// SIGTERM the agent, its process group when it leads one, and every
/// descendant (shells start in their own groups); SIGKILL what is left
/// after a grace.
#[cfg(unix)]
fn end_tree(pid: u32) {
    let mut targets = descendants(pid);
    targets.push(pid as i32);
    let targets: Vec<i32> = targets
        .into_iter()
        // SAFETY: getpgid only inspects; a vanished pid returns -1.
        .map(|p| if unsafe { libc::getpgid(p) } == p { -p } else { p })
        .collect();
    let alive = |target: i32| unsafe { libc::kill(target, 0) } == 0;
    for &target in &targets {
        crate::send_signal(&target, crate::Signal::Term);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while targets.iter().any(|&t| alive(t)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for &target in &targets {
        if alive(target) {
            crate::send_signal(&target, crate::Signal::Kill);
        }
    }
}

#[cfg(unix)]
fn descendants(root: u32) -> Vec<i32> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let table: Vec<(i32, i32)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace().map(str::parse::<i32>);
            Some((cols.next()?.ok()?, cols.next()?.ok()?))
        })
        .collect();
    let mut found = Vec::new();
    let mut frontier = vec![root as i32];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in &table {
            if ppid == parent && pid > 1 && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// pingdotgg/t3code#15357 / #15542: an agent whose engine died keeps
    /// working unsupervised. Its record lets the next engine end it — and
    /// only it: a live engine's agents, a recycled pid and a normally ended
    /// run are left alone.
    #[test]
    fn orphaned_agents_are_ended_and_live_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let spawn = || {
            std::process::Command::new("sh")
                .args(["-c", "sleep 60 & wait"])
                .spawn()
                .unwrap()
        };
        let mut orphan = spawn();
        let mut kept = spawn();
        // Let the shells start their sleeps.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let grandchild = descendants(orphan.id());
        assert!(!grandchild.is_empty());

        // The orphan's engine is a process that has exited.
        let mut dead_engine = std::process::Command::new("true").spawn().unwrap();
        let dead = dead_engine.id();
        dead_engine.wait().unwrap();
        let write = |name: &str, engine: u32, engine_started: &str, pid: u32, started: &str| {
            std::fs::write(
                dir.path().join(name),
                serde_json::json!({
                    "engine": engine, "engineStarted": engine_started,
                    "pid": pid, "started": started, "label": "claude",
                })
                .to_string(),
            )
            .unwrap();
        };
        write("orphan.json", dead, "Mon Jan  1 00:00:00 2001", orphan.id(), &start_time(orphan.id()).unwrap());
        // The same pid but a different process start: recycled, not ours.
        write("recycled.json", dead, "x", kept.id(), "Mon Jan  1 00:00:00 2001");
        // This (live) engine's own agent.
        let registration = register_in(dir.path(), kept.id(), "codex").unwrap();
        std::fs::write(dir.path().join("torn.json"), "{").unwrap();

        assert_eq!(reap_in(dir.path()), 1);
        let status = orphan.wait().unwrap();
        assert!(!status.success(), "the orphan was ended");
        for pid in grandchild {
            let mut gone = false;
            for _ in 0..60 {
                if unsafe { libc::kill(pid, 0) } != 0 {
                    gone = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(gone, "its descendant {pid} too");
        }
        assert!(kept.try_wait().unwrap().is_none(), "a live engine's agent runs on");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 1, "only the live record stays: {left:?}");
        drop(registration);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let _ = kept.kill();
        let _ = kept.wait();
    }
}
