//! The git binary the engine spawns.
//!
//! On macOS `/usr/bin/git` is an xcrun shim that looks up the active developer
//! directory on every run, roughly doubling the cost of each spawn. When that
//! shim is the git on PATH, the Xcode git behind it runs directly. A git placed
//! earlier on PATH (Homebrew, Nix) still wins, and any lookup failure keeps
//! plain `git`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The program to pass to `Command::new` for git. The lookup runs on its own
/// thread so no caller waits on xcrun; until it lands, git runs via the shim.
#[cfg(target_os = "macos")]
pub(crate) fn git_program() -> OsString {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Key = (Option<OsString>, Option<OsString>);
    static RESOLVED: Mutex<Option<(Key, Option<PathBuf>)>> = Mutex::new(None);
    static RESOLVING: AtomicBool = AtomicBool::new(false);

    let key = (std::env::var_os("PATH"), std::env::var_os("DEVELOPER_DIR"));
    {
        let resolved = RESOLVED.lock().unwrap_or_else(|e| e.into_inner());
        // An Xcode update or removal deletes the binary; look again instead of failing the spawn.
        if let Some((cached, git)) = resolved.as_ref()
            && *cached == key
            && git.as_deref().is_none_or(is_executable)
        {
            return git
                .clone()
                .map_or_else(|| "git".into(), PathBuf::into_os_string);
        }
    }
    if !RESOLVING.swap(true, Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("git-program".into())
            .spawn(move || {
                let git = shim_target(key.0.as_deref(), Path::new(SHIM), locate_with_xcrun);
                *RESOLVED.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, git));
                RESOLVING.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            RESOLVING.store(false, Ordering::Release);
        }
    }
    "git".into()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn git_program() -> OsString {
    "git".into()
}

#[cfg(target_os = "macos")]
const SHIM: &str = "/usr/bin/git";

/// The real git behind `shim` when `shim` is the first git on `path`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn shim_target(
    path: Option<&OsStr>,
    shim: &Path,
    locate: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let first = std::env::split_paths(path?)
        .map(|dir| dir.join("git"))
        .find(|candidate| is_executable(candidate))?;
    if first != shim {
        return None;
    }
    locate().filter(|git| git.is_absolute() && git != shim && is_executable(git))
}

/// `xcrun --find git`, which honours DEVELOPER_DIR and `xcode-select` as the shim does.
#[cfg(target_os = "macos")]
fn locate_with_xcrun() -> Option<PathBuf> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("/usr/bin/xcrun")
        .args(["--find", "git"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut stdout).ok()?;
    Some(PathBuf::from(stdout.trim()))
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
#[allow(dead_code)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_git(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let git = dir.join("git");
        std::fs::write(&git, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        git
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn the_shim_first_on_path_runs_the_git_behind_it() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = fake_git(&tmp.path().join("usr/bin"));
        let real = fake_git(&tmp.path().join("Xcode/usr/bin"));
        let path = path_of(&[&tmp.path().join("empty"), shim.parent().unwrap()]);

        let resolved = shim_target(Some(&path), &shim, || Some(real.clone()));

        assert_eq!(resolved, Some(real));
    }

    #[test]
    fn a_git_earlier_on_path_wins_without_asking_xcrun() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = fake_git(&tmp.path().join("usr/bin"));
        let brew = fake_git(&tmp.path().join("homebrew/bin"));
        let path = path_of(&[brew.parent().unwrap(), shim.parent().unwrap()]);

        let resolved = shim_target(Some(&path), &shim, || panic!("xcrun consulted"));

        assert_eq!(resolved, None);
    }

    #[test]
    fn a_failed_or_missing_lookup_keeps_the_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = fake_git(&tmp.path().join("usr/bin"));
        let path = path_of(&[shim.parent().unwrap()]);

        assert_eq!(shim_target(Some(&path), &shim, || None), None);
        assert_eq!(
            shim_target(Some(&path), &shim, || Some(tmp.path().join("gone/git"))),
            None
        );
        assert_eq!(shim_target(Some(&path), &shim, || Some("git".into())), None);
    }
}
