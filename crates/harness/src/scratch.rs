//! A private temp root for one managed adapter run.
//!
//! Archive-installed adapters ship as PyInstaller one-file bundles. The bundle
//! bootloader unpacks its payload (the Antigravity adapter unpacks ~1 GB) into
//! a fresh `_MEI<random>` directory under the child's temp root on every
//! launch, and removes that directory only when the bootloader process exits
//! normally. Adapters are reaped through `TerminateJobObject`, which skips that
//! path entirely, so the unpack directory outlives the run and accumulates on
//! disk — one gigabyte per Antigravity session.
//!
//! Pointing the child's temp variables at a directory this process owns moves
//! the payload somewhere it can delete itself once the job is gone. Removal is
//! retried briefly: the guard drops after the job is terminated, but the killed
//! processes may still be closing handles on the way out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Temp variables the child reads to pick its unpack location. `TMPDIR` is the
/// Unix name PyInstaller honors; `TEMP`/`TMP` are the Windows pair.
const TEMP_VARS: [&str; 3] = ["TEMP", "TMP", "TMPDIR"];

/// How long to keep retrying a removal the child is still holding open.
const REMOVAL_ATTEMPTS: u32 = 10;
const REMOVAL_BACKOFF: Duration = Duration::from_millis(50);

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Owns one child's temp root and removes it on drop.
#[derive(Debug)]
pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Create a uniquely named temp root for a single child process. The label
    /// only aids debugging; the pid and a process-local counter keep concurrent
    /// runs of the same adapter apart.
    pub(crate) fn new(label: &str) -> std::io::Result<Self> {
        static SWEPT: std::sync::Once = std::sync::Once::new();
        SWEPT.call_once(|| sweep_stale(&std::env::temp_dir(), std::process::id()));
        let label: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        loop {
            let path = std::env::temp_dir().join(format!(
                "godspeed-{label}-{}-{}",
                std::process::id(),
                NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Route the child's temp files into this directory.
    pub(crate) fn apply(&self, command: &mut crate::process::Command) {
        for key in TEMP_VARS {
            command.env(key, self.path());
        }
    }
}

/// Scratch roots and unpack directories older than this, whose owner can't
/// be checked, are leftovers.
const STALE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Reclaim what an earlier engine left in `temp`: scratch roots of an engine
/// that died before its guards dropped (`godspeed-<label>-<pid>-<n>`, owner
/// gone), and Antigravity `_MEI*` unpack directories from builds before the
/// scratch root existed — a gigabyte each. Only directories carrying the
/// Antigravity bundle's own files count; other PyInstaller apps share the
/// `_MEI` prefix and are never touched.
fn sweep_stale(temp: &Path, own_pid: u32) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let stale = if let Some(rest) = name.strip_prefix("godspeed-") {
            // `<label>-<pid>-<n>`: the label may itself contain dashes.
            let mut parts = rest.rsplitn(3, '-');
            let (Some(counter), Some(pid), Some(_label)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(_), Ok(pid)) = (counter.parse::<u64>(), pid.parse::<u32>()) else {
                continue;
            };
            pid != own_pid && !owner_alive(pid, &path)
        } else if name.starts_with("_MEI") {
            ["agy_acp_licenses.txt", "localharness", "localharness.exe"]
                .iter()
                .any(|marker| path.join(marker).exists())
                && older_than(&path, STALE_AGE)
        } else {
            false
        };
        if stale && let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::debug!(path = %path.display(), %error, "could not reclaim stale adapter temp dir");
        }
    }
}

fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|elapsed| elapsed > age)
}

/// Whether the engine that made a scratch root may still be using it.
fn owner_alive(pid: u32, path: &Path) -> bool {
    #[cfg(unix)]
    {
        let _ = path;
        // SAFETY: signal 0 only probes for existence. EPERM still means alive.
        let probed = unsafe { libc::kill(pid as i32, 0) };
        probed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        !older_than(path, STALE_AGE)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        for attempt in 1..=REMOVAL_ATTEMPTS {
            match std::fs::remove_dir_all(&self.path) {
                Ok(()) => return,
                // Already gone: nothing to clean up and nothing to report.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => {
                    if attempt == REMOVAL_ATTEMPTS {
                        tracing::warn!(
                            path = %self.path.display(),
                            %error,
                            "could not remove adapter scratch directory"
                        );
                    } else {
                        std::thread::sleep(REMOVAL_BACKOFF);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard's real job: a terminated adapter leaves its `_MEI*` unpack
    /// directory behind, and the whole tree must go when the guard drops.
    #[test]
    fn drop_removes_the_unpack_tree_a_killed_child_leaves_behind() {
        let root = {
            let dir = ScratchDir::new("unit").unwrap();
            let unpack = dir.path().join("_MEI00001234");
            std::fs::create_dir_all(&unpack).unwrap();
            std::fs::write(unpack.join("payload.bin"), [0u8; 4096]).unwrap();
            assert!(unpack.join("payload.bin").is_file());
            dir.path().to_path_buf()
        };
        assert!(!root.exists(), "scratch directory survived the guard");
    }

    /// pingdotgg/t3code#12239: an engine killed before its guards drop
    /// leaves its scratch roots, and builds before the scratch root left
    /// `_MEI*` unpacks in the shared temp dir (56 GB on one machine). Both
    /// are reclaimed — only those, never a live engine's or another
    /// PyInstaller app's.
    #[cfg(unix)]
    #[test]
    fn stale_scratch_roots_and_antigravity_unpacks_are_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let dead = exited.id();
        exited.wait().unwrap();
        let own = std::process::id();
        let dir = |name: &str| {
            let path = temp.path().join(name);
            std::fs::create_dir_all(path.join("_MEI0001")).unwrap();
            path
        };
        let orphan = dir(&format!("godspeed-agy-acp-server-exe-{dead}-0"));
        let mine = dir(&format!("godspeed-agy-acp-server-exe-{own}-3"));
        let other_live = dir("godspeed-pi-mcp-1-0");
        let unrelated = dir("godspeed-probe-501");
        let old_agy = dir("_MEIold");
        std::fs::write(old_agy.join("agy_acp_licenses.txt"), "").unwrap();
        let fresh_agy = dir("_MEIfresh");
        std::fs::write(fresh_agy.join("agy_acp_licenses.txt"), "").unwrap();
        let other_app = dir("_MEIother");
        let past = std::time::SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        for path in [&old_agy, &other_app] {
            std::fs::File::open(path).unwrap().set_modified(past).unwrap();
        }
        sweep_stale(temp.path(), own);
        assert!(!orphan.exists(), "a dead engine's scratch root");
        assert!(!old_agy.exists(), "an old Antigravity unpack");
        for kept in [&mine, &other_live, &unrelated, &fresh_agy, &other_app] {
            assert!(kept.exists(), "{}", kept.display());
        }
    }

    #[test]
    fn concurrent_scratch_dirs_do_not_collide() {
        let first = ScratchDir::new("unit").unwrap();
        let second = ScratchDir::new("unit").unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir() && second.path().is_dir());
    }

    /// `apply` is the half that decides *where* the child unpacks. Read the
    /// environment the command would hand the child instead of trusting it.
    #[test]
    fn apply_points_every_temp_variable_at_the_scratch_dir() {
        let dir = ScratchDir::new("unit").unwrap();
        let mut command = crate::process::Command::new("probe");
        dir.apply(&mut command);
        let envs: Vec<(String, Option<std::ffi::OsString>)> = command
            .as_std_mut()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().to_ascii_uppercase(),
                    value.map(|v| v.to_os_string()),
                )
            })
            .collect();
        for key in TEMP_VARS {
            let found = envs
                .iter()
                .find(|(name, _)| name == key)
                .and_then(|(_, value)| value.as_deref());
            assert_eq!(
                found,
                Some(dir.path().as_os_str()),
                "{key} should point at the scratch dir"
            );
        }
    }
}

#[cfg(all(test, unix))]
#[test]
fn scratch_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = ScratchDir::new("private").unwrap();
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
