//! The build environment Godspeed adds to everything it spawns: agent
//! processes and terminals (project Actions run in terminals).
//!
//! Two parts, both owned by the engine's `build_slots` service and published
//! here with [`install`] so the spawn sites can read them without a handle:
//!
//! - **Shared build slots.** One GNU-make-style jobserver (a named FIFO, make
//!   4.4's `fifo:` style) for every agent. `CARGO_MAKEFLAGS` points cargo at
//!   it, so three agents building at once share N compile slots instead of
//!   starting N rustc each. `MAKEFLAGS` is added only when the `make` on PATH
//!   is GNU make 4.4 or newer: older makes (macOS ships 3.81, many Linux
//!   distros 4.3) can't read a `fifo:` jobserver — 3.81 even treats the bare
//!   `-j` as "unlimited".
//! - **sccache.** `RUSTC_WRAPPER` for runs in a Rust project, so worktrees
//!   of one repo reuse compiled dependencies.
//!
//! The user's own choices win: nothing here is set when the run's env, the
//! engine's env, the login shell or cargo config already picks build
//! parallelism (or a rustc wrapper).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Variables that mean "I chose my own build parallelism". With a jobserver
/// inherited, cargo ignores `-j`/`CARGO_BUILD_JOBS`, so setting ours on top
/// of any of these would silently override the user.
pub const JOBS_OVERRIDES: &[&str] = &["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS", "CARGO_BUILD_JOBS"];

/// Variables that already pick a rustc wrapper.
pub const WRAPPER_OVERRIDES: &[&str] = &["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"];

/// Crates whose build scripts copy `CARGO_MAKEFLAGS` into `MAKEFLAGS` for a
/// `make` they run. A `make` older than 4.4 stops on a `fifo:` jobserver
/// ("invalid --jobserver-fds string"), so a lockfile that pulls one of these
/// in only gets the pool when `make` is new enough.
const MAKEFLAGS_FORWARDERS: &[&str] =
    &["cmake", "openssl-src", "tikv-jemalloc-sys", "jemalloc-sys"];

/// What the engine resolved: the running pool and the tools it found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildEnv {
    /// `fifo:<path>` of the shared jobserver while it runs.
    pub jobserver: Option<String>,
    /// The `make` on PATH is GNU make ≥ 4.4 and reads `fifo:` jobservers.
    pub make_reads_fifo: bool,
    /// sccache to use as `RUSTC_WRAPPER`, when the setting is on.
    pub sccache: Option<PathBuf>,
}

static CURRENT: RwLock<Option<Arc<BuildEnv>>> = RwLock::new(None);

/// Publish (or clear) the build environment new spawns get.
pub fn install(env: Option<BuildEnv>) {
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = env.map(Arc::new);
}

/// The published build environment, if any.
pub fn current() -> Option<Arc<BuildEnv>> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The build variables for a process starting in `cwd`. `explicit` says
/// whether the caller sets a variable on it itself (a run's worktree env, a
/// terminal's env): a build variable there counts as the user's choice, like
/// one in the engine's own environment or the login shell.
pub fn vars_for(cwd: Option<&Path>, explicit: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    let Some(env) = current() else {
        return Vec::new();
    };
    let cargo_home = cargo_home();
    vars(&env, cwd, cargo_home.as_deref(), |name| {
        explicit(name) || ambient_var(name).is_some()
    })
}

/// [`vars_for`] applied to a std command that also gets `explicit`.
pub fn apply(
    cmd: &mut std::process::Command,
    cwd: Option<&Path>,
    explicit: &BTreeMap<String, String>,
) {
    for (name, value) in vars_for(cwd, |name| explicit.contains_key(name)) {
        cmd.env(name, value);
    }
}

/// A variable the user set for every process: in the engine's environment
/// or their login shell's.
pub fn ambient_var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .or_else(|| crate::shell_env::login_shell_var(name))
}

/// The first of `names` the user set for every process, for Settings to say
/// why the pool is paused.
pub fn first_ambient(names: &[&'static str]) -> Option<&'static str> {
    names
        .iter()
        .copied()
        .find(|name| ambient_var(name).is_some())
}

/// The decision itself, pure for tests. `user_set` answers whether the user
/// set a variable anywhere; `cargo_home` is where the global cargo config
/// lives.
pub fn vars(
    env: &BuildEnv,
    cwd: Option<&Path>,
    cargo_home: Option<&Path>,
    user_set: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(auth) = &env.jobserver
        && !JOBS_OVERRIDES.iter().any(|name| user_set(name))
        && !cargo_config_sets(cwd, cargo_home, "jobs")
        && (env.make_reads_fifo || cwd.is_some_and(lockfile_is_fifo_safe))
    {
        let flags = format!("-j --jobserver-auth={auth}");
        if env.make_reads_fifo {
            out.push(("MAKEFLAGS".to_owned(), flags.clone()));
        }
        out.push(("CARGO_MAKEFLAGS".to_owned(), flags));
    }
    if let Some(sccache) = &env.sccache
        && cwd.and_then(cargo_manifest_dir).is_some()
        && !WRAPPER_OVERRIDES.iter().any(|name| user_set(name))
        && !cargo_config_sets(cwd, cargo_home, "rustc-wrapper")
    {
        out.push((
            "RUSTC_WRAPPER".to_owned(),
            sccache.to_string_lossy().into_owned(),
        ));
    }
    out
}

/// The nearest directory at or above `cwd` with a `Cargo.toml`.
pub fn cargo_manifest_dir(cwd: &Path) -> Option<&Path> {
    cwd.ancestors().find(|dir| dir.join("Cargo.toml").is_file())
}

/// A Rust project whose `Cargo.lock` has none of [`MAKEFLAGS_FORWARDERS`].
/// No lockfile is not safe: there is no telling what the first build pulls.
fn lockfile_is_fifo_safe(cwd: &Path) -> bool {
    let Some(lock) = cwd
        .ancestors()
        .map(|dir| dir.join("Cargo.lock"))
        .find(|path| path.is_file())
    else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(lock) else {
        return false;
    };
    !text.lines().any(|line| {
        line.strip_prefix("name = \"")
            .and_then(|rest| rest.strip_suffix('"'))
            .is_some_and(|name| MAKEFLAGS_FORWARDERS.contains(&name))
    })
}

/// `$CARGO_HOME`, else `~/.cargo`.
fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

/// Whether any cargo config cargo would read from `cwd` sets `build.<key>`:
/// `.cargo/config.toml` (or legacy `config`) in `cwd` and every ancestor,
/// then `$CARGO_HOME`.
fn cargo_config_sets(cwd: Option<&Path>, cargo_home: Option<&Path>, key: &str) -> bool {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in cwd.into_iter().flat_map(Path::ancestors) {
        files.push(dir.join(".cargo/config.toml"));
        files.push(dir.join(".cargo/config"));
    }
    if let Some(home) = cargo_home {
        files.push(home.join("config.toml"));
        files.push(home.join("config"));
    }
    files.iter().any(|file| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| table.get("build")?.as_table()?.get(key).cloned())
            .is_some()
    })
}

/// The directories a child of ours searches: our PATH, then the login
/// shell's.
fn search_path() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(shell_path) = crate::shell_env::login_shell_path() {
        dirs.extend(std::env::split_paths(shell_path));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| !dir.as_os_str().is_empty() && seen.insert(dir.clone()));
    dirs
}

/// `name` on the search path, as an absolute path.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    search_path()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Whether the `make` children will find is GNU make 4.4 or newer (the
/// first with `fifo:` jobservers). Runs `make --version`; blocking.
pub fn make_reads_fifo() -> bool {
    let Some(make) = find_tool("make") else {
        return false;
    };
    std::process::Command::new(make)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .is_some_and(|out| gnu_make_reads_fifo(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `make --version`: "GNU Make 4.4.1" → true, "GNU Make 3.81" → false.
pub fn gnu_make_reads_fifo(version: &str) -> bool {
    let Some(rest) = version
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("GNU Make "))
    else {
        return false;
    };
    let mut parts = rest
        .split(|c: char| !c.is_ascii_digit())
        .map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= (4, 4),
        _ => false,
    }
}

#[cfg(test)]
#[path = "build_env_tests.rs"]
mod tests;
