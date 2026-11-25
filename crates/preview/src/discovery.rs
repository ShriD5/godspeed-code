//! Observe only this user's listeners. HTTP probes run only after project
//! ownership has been established from the process's actual working directory
//! and commands performing authentication, and test runs with their fixture
//! servers, have been excluded.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Listener {
    pub pid: u32,
    pub parent: u32,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub started_at: u64,
    pub address: SocketAddr,
    pub zeron_owned: bool,
    /// An ancestor of its process (up to this engine's own children) runs a
    /// test runner ([`is_test_command`]): a fixture server the test spawned,
    /// never probed. Its own command is checked by [`Self::is_test_run`].
    pub test_run: bool,
}

impl Listener {
    pub fn belongs_to(&self, root: &Path) -> bool {
        self.cwd.starts_with(root)
    }

    /// A login CLI may open an HTTP listener in the project's cwd, but that
    /// listener is a browser callback, not a development server. Some (such as
    /// Infisical) abort the login on an unsolicited HEAD, so exclude explicit
    /// authentication commands before connecting at all. Match whole arguments,
    /// not paths, URLs or source strings that happen to mention authentication.
    pub(crate) fn is_authentication_command(&self) -> bool {
        self.args
            .iter()
            .skip(1)
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| {
                matches!(
                    arg.as_str(),
                    "login"
                        | "auth"
                        | "authenticate"
                        | "signin"
                        | "sign-in"
                        | "sso"
                        | "oauth"
                        | "oauth2"
                )
            })
    }

    /// A test suite's listener: its own command runs tests, or an ancestor's
    /// does ([`Self::test_run`]). Test suites start scripted fixture servers
    /// on loopback that answer a fixed number of connections; a probe would
    /// take one of them and fail the test, so these are never connected to.
    pub(crate) fn is_test_run(&self) -> bool {
        self.test_run || is_test_command(&self.args)
    }
}

/// Programs that are test runners whatever their arguments.
const TEST_RUNNERS: &[&str] = &[
    "vitest",
    "jest",
    "mocha",
    "_mocha",
    "pytest",
    "py.test",
    "cargo-nextest",
];
/// Test runners' packages, as a path's `node_modules/<package>` (a runner's
/// own entry point or a worker it forks, run by `node`).
const TEST_RUNNER_PACKAGES: &[&str] = &[
    "vitest",
    "@vitest",
    "jest",
    "jest-cli",
    "jest-worker",
    "@jest",
    "mocha",
];
/// Programs that run the next word as the command: `npx vitest`,
/// `python3 -m pytest`, `node /…/npm test`, `uv run pytest`.
const LAUNCHERS: &[&str] = &[
    "node", "nodejs", "npx", "bunx", "pnpx", "tsx", "ts-node", "env", "uv", "poetry", "pipenv",
    "exec", "x", "dlx", "run", "sh", "bash", "zsh",
];

/// A program word's name: its file name, lowercased, without a script or
/// executable extension (`/…/node_modules/.bin/vitest.cmd` → `vitest`,
/// `/…/npm-cli.js` → `npm-cli`).
fn program_name(word: &str) -> String {
    let name = Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(word)
        .to_lowercase();
    for extension in [".js", ".mjs", ".cjs", ".ts", ".mts", ".exe", ".cmd"] {
        if let Some(stem) = name.strip_suffix(extension) {
            return stem.to_owned();
        }
    }
    name
}

fn is_python(name: &str) -> bool {
    name.strip_prefix("python")
        .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// A Rust test binary: cargo builds tests (unit and integration alike) into
/// `<target>/[<triple>/]<profile>/deps/<crate>-<16 hex digits>`, and runs
/// them from there; binaries an app runs (`cargo run`) sit one level up.
fn is_rust_test_binary(program: &str) -> bool {
    let path = Path::new(program);
    if path.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("deps")) {
        return false;
    }
    let hashed = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".exe").unwrap_or(n))
        .and_then(|n| n.rsplit_once('-'))
        .is_some_and(|(_, hash)| hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()));
    hashed || path.components().any(|c| c.as_os_str() == "target")
}

/// Whether `args` (a process's command line) runs tests: a Rust test binary
/// or libtest's flags, `cargo test`/`cargo nextest`, `go test` and its
/// `*.test` binaries, `bun test`, `deno test`, `npm|pnpm|yarn|bun [run] test`,
/// vitest, jest, mocha or pytest. Whole words and path components only, never
/// substrings: `node -e "require('vitest')"` or a project folder named
/// `tests` is no test run. Arguments after `--` belong to the program a
/// launcher runs, not to it, and are not read.
pub fn is_test_command(args: &[String]) -> bool {
    let Some(program) = args.first() else {
        return false;
    };
    if is_rust_test_binary(program)
        || args
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--test-threads" || arg.starts_with("--test-threads="))
    {
        return true;
    }
    // Go's test binaries: `go test` builds `<package>.test` and runs it.
    if Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".test"))
        .is_some_and(|stem| !stem.is_empty())
    {
        return true;
    }
    // The command's words: its arguments up to `--`, flags (and cargo's
    // `+toolchain`) left out.
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|arg| *arg != "--")
        .filter(|arg| !arg.starts_with('-') && !arg.starts_with('+'))
        .collect();
    let names: Vec<String> = words.iter().map(|word| program_name(word)).collect();
    let script = |name: Option<&String>| {
        name.is_some_and(|name| name == "test" || name.starts_with("test:"))
    };
    for (i, name) in names.iter().enumerate() {
        // A word naming a program: the first, or one a launcher runs.
        let command =
            i == 0 || LAUNCHERS.contains(&names[i - 1].as_str()) || is_python(&names[i - 1]);
        if command && TEST_RUNNERS.contains(&name.as_str()) {
            return true;
        }
        let path = Path::new(words[i]);
        let mut components = path
            .components()
            .map(|c| c.as_os_str().to_str().unwrap_or(""));
        let mut previous = "";
        let in_runner_package = components.any(|component| {
            let found = previous == "node_modules" && TEST_RUNNER_PACKAGES.contains(&component);
            previous = component;
            found
        });
        if in_runner_package {
            return true;
        }
        let next = names.get(i + 1);
        let tested = match name.as_str() {
            "cargo" => next.is_some_and(|n| matches!(n.as_str(), "test" | "t" | "nextest")),
            "go" | "deno" => next.is_some_and(|n| n == "test"),
            "npm" | "npm-cli" | "pnpm" | "yarn" | "bun" => {
                script(next)
                    || (name.starts_with("npm") && next.is_some_and(|n| n == "t"))
                    || (next.is_some_and(|n| n == "run" || n == "run-script")
                        && script(names.get(i + 2)))
            }
            _ => false,
        };
        if tested {
            return true;
        }
    }
    false
}

/// Preserve a useful framework label without advertising process arguments,
/// which may contain credentials. Rank frontends above generic HTTP services.
pub fn framework(args: &[String]) -> (&'static str, u8) {
    for (needle, name) in [
        ("vite", "Vite"),
        ("next", "Next.js"),
        ("astro", "Astro"),
        ("miniflare", "Miniflare"),
    ] {
        if args.iter().any(|arg| {
            let arg = arg.to_lowercase();
            Path::new(&arg).file_stem().and_then(|s| s.to_str()) == Some(needle)
                || arg.contains(&format!("/{needle}/"))
                || arg.starts_with(&format!("{needle}-server"))
        }) {
            return (name, if needle == "miniflare" { 80 } else { 100 });
        }
    }
    if args.first().is_some_and(|s| {
        Path::new(s)
            .file_name()
            .is_some_and(|n| n == "node" || n == "bun" || n == "deno")
    }) {
        ("Node HTTP server", 50)
    } else {
        ("HTTP server", 40)
    }
}

/// Port flags are operational settings, never part of a service identity.
pub fn command_identity(args: &[String]) -> String {
    let mut skip = false;
    let python_http = args.iter().any(|arg| arg == "http.server");
    args.iter()
        .filter_map(|arg| {
            if skip {
                skip = false;
                return None;
            }
            if matches!(arg.as_str(), "--port" | "-p" | "--inspect-port") {
                skip = true;
                return None;
            }
            if arg.starts_with("--port=")
                || arg.starts_with("PORT=")
                || arg.starts_with("--inspect-port=")
            {
                return None;
            }
            if python_http && arg.parse::<u16>().is_ok() {
                return None;
            }
            Some(arg.as_str())
        })
        .collect::<Vec<_>>()
        .join("\0")
}

pub async fn is_http(address: SocketAddr) -> bool {
    tokio::time::timeout(Duration::from_millis(800), async move {
        let mut socket = tokio::net::TcpStream::connect(address).await.ok()?;
        socket
            .write_all(
                format!(
                    "HEAD / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .ok()?;
        let mut prefix = [0; 12];
        socket.read_exact(&mut prefix).await.ok()?;
        let version = &prefix[..9];
        let status = std::str::from_utf8(&prefix[9..])
            .ok()?
            .parse::<u16>()
            .ok()?;
        ((version == b"HTTP/1.1 " || version == b"HTTP/1.0 ") && (100..600).contains(&status))
            .then_some(())
    })
    .await
    .ok()
    .flatten()
    .is_some()
}

fn mark_descendants(listeners: &mut [Listener], parents: &HashMap<u32, u32>, owner: u32) {
    for listener in listeners {
        let mut pid = listener.pid;
        for _ in 0..128 {
            if pid == owner {
                listener.zeron_owned = true;
                break;
            }
            let Some(&parent) = parents.get(&pid) else {
                break;
            };
            if parent == pid || parent == 0 {
                break;
            }
            pid = parent;
        }
    }
}

/// Flags each listener whose process has an ancestor running tests
/// ([`is_test_command`]): a fixture server is often a plain python/node child
/// of the test. The walk stops at this engine's own children (an agent, a
/// terminal's shell): an agent may carry its prompt in its arguments ("run
/// npm test, then start the dev server"), and everything it starts would be
/// hidden for its lifetime.
fn mark_test_runs(
    listeners: &mut [Listener],
    parents: &HashMap<u32, u32>,
    commands: &HashMap<u32, Vec<String>>,
) {
    let engine = std::process::id();
    let mut verdicts: HashMap<u32, bool> = HashMap::new();
    for listener in listeners {
        let Some(&first) = parents.get(&listener.pid) else {
            continue;
        };
        let mut pid = first;
        for _ in 0..64 {
            if pid <= 1 || pid == engine {
                break;
            }
            let Some(&parent) = parents.get(&pid) else {
                break;
            };
            if parent == engine {
                break;
            }
            let test = *verdicts
                .entry(pid)
                .or_insert_with(|| commands.get(&pid).is_some_and(|args| is_test_command(args)));
            if test {
                listener.test_run = true;
                break;
            }
            if parent == pid {
                break;
            }
            pid = parent;
        }
    }
}

/// Process start in ms since the epoch, derived the same way for the scanner
/// and for [`same_process`] so the two agree exactly.
#[cfg(target_os = "linux")]
fn linux_clock() -> (u64, u64) {
    let boot = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("btime ")
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .unwrap_or(0);
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    (boot, ticks)
}
#[cfg(target_os = "linux")]
fn linux_started_at(stat_fields: &[&str], boot: u64, ticks: u64) -> u64 {
    boot * 1000
        + stat_fields
            .get(19)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1000
            / ticks
}

/// Whether `pid` is still the process the scanner observed with `started_at`.
/// Cheap enough to run per proxied connection; a full [`listeners`] pass
/// walks every process (and spawns lsof/ps on macOS), which the proxy used
/// to do for every asset a remote page requested.
#[cfg(target_os = "linux")]
pub fn same_process(pid: u32, started_at: u64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, tail)) = stat.rsplit_once(") ") else {
        return false;
    };
    let fields: Vec<_> = tail.split_whitespace().collect();
    let (boot, ticks) = linux_clock();
    linux_started_at(&fields, boot, ticks) == started_at
}

#[cfg(target_os = "linux")]
pub fn listeners() -> Vec<Listener> {
    use std::{fs, os::unix::fs::MetadataExt};
    let mut sockets = HashMap::new();
    for (file, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(text) = fs::read_to_string(file) {
            for line in text.lines().skip(1) {
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() <= 9 || parts[3] != "0A" {
                    continue;
                }
                if let Some(address) = proc_address(parts[1], ipv6) {
                    sockets.insert(parts[9].to_string(), address);
                }
            }
        }
    }
    let (boot, ticks) = linux_clock();
    let uid = unsafe { libc::geteuid() };
    let mut result = Vec::new();
    let mut parents = HashMap::new();
    let mut commands = HashMap::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return result;
    };
    for process in processes.flatten() {
        let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = process.path();
        if !fs::metadata(&path).is_ok_and(|m| m.uid() == uid) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(path.join("stat")) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = tail.split_whitespace().collect();
        let Some(parent) = fields.get(1).and_then(|v| v.parse().ok()) else {
            continue;
        };
        parents.insert(pid, parent);
        let args = fs::read(path.join("cmdline"))
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>();
        commands.insert(pid, args.clone());
        let Ok(cwd) = fs::read_link(path.join("cwd")) else {
            continue;
        };
        let started_at = linux_started_at(&fields, boot, ticks);
        let Ok(fds) = fs::read_dir(path.join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            let link = link.to_string_lossy();
            let Some(inode) = link
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
            else {
                continue;
            };
            if let Some(&address) = sockets.get(inode) {
                result.push(Listener {
                    pid,
                    parent,
                    cwd: cwd.clone(),
                    args: args.clone(),
                    started_at,
                    address,
                    zeron_owned: false,
                    test_run: false,
                });
            }
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    mark_test_runs(&mut result, &parents, &commands);
    result.sort_by_key(|l| (l.address, l.pid));
    result.dedup_by_key(|l| (l.address, l.pid));
    result
}

#[cfg(target_os = "linux")]
fn proc_address(value: &str, ipv6: bool) -> Option<SocketAddr> {
    let (host, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = if ipv6 {
        if host.len() != 32 {
            return None;
        }
        let mut bytes = [0; 16];
        for (i, chunk) in host.as_bytes().chunks_exact(8).enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(
                &u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16)
                    .ok()?
                    .to_le_bytes(),
            );
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        IpAddr::V4(Ipv4Addr::from(
            u32::from_str_radix(host, 16).ok()?.to_le_bytes(),
        ))
    };
    if !ip.is_loopback() && !ip.is_unspecified() {
        return None;
    }
    Some(SocketAddr::new(
        if ip.is_unspecified() {
            if ipv6 {
                Ipv6Addr::LOCALHOST.into()
            } else {
                Ipv4Addr::LOCALHOST.into()
            }
        } else {
            ip
        },
        port,
    ))
}

/// `ps lstart=` tokens ("Thu Sep 11 16:14:42 2026") to ms since the epoch.
#[cfg(target_os = "macos")]
fn parse_lstart(parts: &[&str]) -> u64 {
    use chrono::TimeZone;
    chrono::NaiveDateTime::parse_from_str(&parts.join(" "), "%a %b %e %T %Y")
        .ok()
        .and_then(|d| chrono::Local.from_local_datetime(&d).earliest())
        .map(|d| d.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

/// See the Linux variant. One `ps -p` for a single pid instead of two lsof
/// passes and a full `ps -ax` per proxied connection.
#[cfg(target_os = "macos")]
pub fn same_process(pid: u32, started_at: u64) -> bool {
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<_> = text.split_whitespace().collect();
    parts.len() >= 5 && parse_lstart(&parts[..5]) == started_at
}

#[cfg(target_os = "macos")]
pub fn listeners() -> Vec<Listener> {
    use std::process::Command;
    let uid = unsafe { libc::geteuid() }.to_string();
    let fields = |arguments: &[&str]| -> Vec<(u32, String)> {
        let Ok(output) = Command::new("/usr/sbin/lsof").args(arguments).output() else {
            return Vec::new();
        };
        let mut pid = 0;
        let mut values = Vec::new();
        for field in output.stdout.split(|b| *b == 0) {
            let field = String::from_utf8_lossy(field);
            let field = field.trim_start_matches('\n');
            if let Some(value) = field.strip_prefix('p') {
                pid = value.parse().unwrap_or(0);
            }
            if let Some(value) = field.strip_prefix('n') {
                values.push((pid, value.to_owned()));
            }
        }
        values
    };
    let cwds: HashMap<_, _> = fields(&["-nP", "-a", "-u", &uid, "-d", "cwd", "-F0pn"])
        .into_iter()
        .collect();
    let mut parents = HashMap::new();
    let mut processes = HashMap::new();
    if let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,lstart=,command="])
        .env("LC_ALL", "C")
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 8 {
                continue;
            }
            let (Ok(pid), Ok(parent)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else {
                continue;
            };
            parents.insert(pid, parent);
            let started_at = parse_lstart(&parts[2..7]);
            processes.insert(
                pid,
                (
                    parent,
                    started_at,
                    parts[7..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                ),
            );
        }
    }
    let mut result = Vec::new();
    for (pid, socket) in fields(&["-nP", "-a", "-u", &uid, "-iTCP", "-sTCP:LISTEN", "-F0pn"]) {
        let Some(cwd) = cwds.get(&pid).and_then(|s| std::fs::canonicalize(s).ok()) else {
            continue;
        };
        let Some((parent, started_at, args)) = processes.get(&pid) else {
            continue;
        };
        let addresses = if let Some(port) = socket
            .strip_prefix("*:")
            .and_then(|p| p.parse::<u16>().ok())
        {
            vec![
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
            ]
        } else {
            socket
                .parse::<SocketAddr>()
                .ok()
                .filter(|a| a.ip().is_loopback())
                .into_iter()
                .collect()
        };
        for address in addresses {
            result.push(Listener {
                pid,
                parent: *parent,
                cwd: cwd.clone(),
                args: args.clone(),
                started_at: *started_at,
                address,
                zeron_owned: false,
                test_run: false,
            });
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    let commands: HashMap<u32, Vec<String>> = processes
        .into_iter()
        .map(|(pid, (_, _, args))| (pid, args))
        .collect();
    mark_test_runs(&mut result, &parents, &commands);
    result
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn listeners() -> Vec<Listener> {
    Vec::new()
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn same_process(_pid: u32, _started_at: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listener_of(pid: u32) -> Listener {
        Listener {
            pid,
            parent: 0,
            cwd: "/project".into(),
            args: Vec::new(),
            started_at: 0,
            address: "127.0.0.1:3000".parse().unwrap(),
            zeron_owned: false,
            test_run: false,
        }
    }
    /// A fixture's own command says nothing; the test runner above it does.
    /// The walk stops at this engine's own children, whose arguments (an
    /// agent's prompt) are never judged.
    #[test]
    fn a_listener_under_a_test_runner_is_a_test_run() {
        let engine = std::process::id();
        let args = |words: &[&str]| words.iter().map(|w| (*w).to_string()).collect::<Vec<_>>();
        // engine -> agent (prompt mentions `npm test`) -> runner -> fixture
        // engine -> agent -> shell -> dev server
        let parents = HashMap::from([(10, engine), (11, 10), (12, 11), (20, 10), (21, 20)]);
        let commands = HashMap::from([
            (10, args(&["claude", "-p", "run", "npm", "test"])),
            (11, args(&["npx", "vitest", "run"])),
            (12, args(&["python3", "fixture.py"])),
            (20, args(&["bash"])),
            (21, args(&["vite"])),
        ]);
        let mut listeners = vec![
            Listener {
                parent: 11,
                ..listener_of(12)
            },
            Listener {
                parent: 20,
                ..listener_of(21)
            },
            Listener {
                parent: engine,
                ..listener_of(10)
            },
        ];
        mark_test_runs(&mut listeners, &parents, &commands);
        let verdicts: Vec<_> = listeners.iter().map(|l| (l.pid, l.test_run)).collect();
        assert_eq!(verdicts, vec![(12, true), (21, false), (10, false)]);
    }
    #[test]
    fn test_runs_are_not_preview_candidates() {
        let listener = |args: &[&str]| Listener {
            args: args.iter().map(|arg| (*arg).into()).collect(),
            ..listener_of(1)
        };
        for args in [
            // Rust: a test binary (unit or integration), libtest's flags,
            // cargo and nextest themselves.
            vec!["/work/app/target/debug/deps/app-0123456789abcdef"],
            vec![
                "/work/app/target/debug/deps/api_tests-89ab01cd23ef4567",
                "--nocapture",
            ],
            vec!["/tmp/cargo-target/aarch64-apple-darwin/release/deps/app-0123456789abcdef"],
            vec!["/work/app/target/debug/deps/fixtures"],
            vec!["/work/app/target/debug/app", "--test-threads=1"],
            vec!["./runner", "--test-threads", "4"],
            vec!["cargo", "test"],
            vec![
                "/Users/me/.cargo/bin/cargo",
                "+nightly",
                "test",
                "-p",
                "api",
            ],
            vec!["cargo", "t"],
            vec!["cargo", "nextest", "run"],
            vec!["/Users/me/.cargo/bin/cargo-nextest", "nextest", "run"],
            // JavaScript.
            vec!["bun", "test"],
            vec!["/Users/me/.bun/bin/bun", "test", "--watch"],
            vec!["bun", "run", "test"],
            vec!["npm", "test"],
            vec!["npm", "t"],
            vec!["node", "/usr/local/bin/npm", "run", "test:unit"],
            vec![
                "node",
                "/usr/local/lib/node_modules/npm/bin/npm-cli.js",
                "test",
            ],
            vec!["pnpm", "test"],
            vec!["pnpm", "run", "test"],
            vec!["yarn", "test"],
            vec!["yarn", "run", "test:e2e"],
            vec!["vitest"],
            vec!["vitest", "run"],
            vec!["node", "/work/app/node_modules/.bin/vitest", "run"],
            vec!["node", "/work/app/node_modules/vitest/vitest.mjs", "run"],
            vec![
                "node",
                "/work/app/node_modules/vitest/dist/workers/forks.js",
            ],
            vec!["npx", "vitest"],
            vec!["bunx", "jest"],
            vec!["jest", "--runInBand"],
            vec![
                "node",
                "--expose-gc",
                "/work/app/node_modules/jest/bin/jest.js",
            ],
            vec![
                "/usr/local/bin/node",
                "/work/app/node_modules/jest-worker/build/workers/processChild.js",
            ],
            vec!["mocha", "test/**/*.spec.js"],
            vec!["node", "/work/app/node_modules/mocha/bin/_mocha"],
            vec!["deno", "test", "--allow-net"],
            // Python and Go.
            vec!["pytest", "-x", "tests/"],
            vec!["/work/app/.venv/bin/pytest"],
            vec!["python3", "-m", "pytest", "-k", "fixture"],
            vec!["/usr/bin/python3.12", "-m", "pytest"],
            vec!["uv", "run", "pytest"],
            vec!["py.test"],
            vec!["go", "test", "./..."],
            vec!["/tmp/go-build2214/b001/server.test", "-test.v"],
        ] {
            assert!(listener(&args).is_test_run(), "{args:?}");
        }
        for args in [
            vec![],
            // Dev servers stay eligible.
            vec!["vite"],
            vec![
                "node",
                "/work/app/node_modules/vite/bin/vite.js",
                "--port",
                "5173",
            ],
            vec!["node", "/work/app/node_modules/.bin/next", "dev"],
            vec!["next", "dev"],
            vec!["next-server"],
            vec!["bun", "run", "dev"],
            vec!["bun", "--hot", "server.ts"],
            vec!["npm", "run", "dev"],
            vec!["npm", "start"],
            vec!["pnpm", "dev"],
            vec!["yarn", "start"],
            vec!["python", "-m", "http.server", "8000"],
            vec!["python3", "manage.py", "runserver"],
            vec!["deno", "run", "-A", "main.ts"],
            vec!["go", "run", "./cmd/server"],
            vec!["/work/app/target/debug/server"],
            vec!["cargo", "run", "--bin", "server"],
            vec!["cargo", "watch", "-x", "run"],
            // Whole words and path components, not substrings.
            vec!["node", "-e", "require('vitest')"],
            vec!["node", "/work/tests/server.js"],
            vec!["node", "/work/app/test/fixtures/server.js"],
            vec!["node", "/work/vitest-demo/server.js"],
            vec!["node", "/work/jest/server.js"],
            vec!["python3", "/work/pytest_plugins/serve.py"],
            vec!["node", "server.js", "--", "--test-threads", "1"],
            vec!["./serve", "--mode", "test"],
            vec!["/work/app/target/debug/deps"],
            vec!["/work/app/deps/server"],
            vec!["/work/.test"],
        ] {
            assert!(!listener(&args).is_test_run(), "{args:?}");
        }
        // An ancestor found running tests counts too.
        let fixture = Listener {
            args: vec!["python3".into(), "fixture.py".into()],
            test_run: true,
            ..listener_of(1)
        };
        assert!(fixture.is_test_run());
    }
    #[test]
    fn authentication_commands_are_not_preview_candidates() {
        let listener = |args: &[&str]| Listener {
            pid: 1,
            parent: 0,
            cwd: "/project".into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            started_at: 1,
            address: "127.0.0.1:3000".parse().unwrap(),
            zeron_owned: true,
            test_run: false,
        };
        for args in [
            vec!["/usr/bin/infisical", "login"],
            vec![
                "infisical",
                "--silent",
                "login",
                "--domain",
                "https://app.infisical.com",
            ],
            vec!["gh", "auth", "login"],
            vec!["aws", "sso", "login"],
            vec!["gcloud", "auth", "application-default", "login"],
            vec!["codex", "login"],
            vec!["node", "/tools/firebase.js", "login"],
            vec!["cli", "authenticate"],
            vec!["cli", "signin"],
            vec!["cli", "sign-in"],
            vec!["cli", "oauth"],
            vec!["cli", "oauth2"],
        ] {
            assert!(listener(&args).is_authentication_command(), "{args:?}");
        }
        for args in [
            vec![],
            vec!["node", "/project/auth/server.js"],
            vec!["node", "/project/login.js"],
            vec!["node", "/project/node_modules/vite/bin/vite.js"],
            vec!["next", "dev"],
            vec!["python3", "-m", "http.server", "3000"],
            vec!["python3", "-c", "import auth; auth.serve()"],
            vec!["server", "--auth", "--login-url=https://example.com/login"],
            vec!["infisical", "run", "--", "node", "server.js", "login"],
        ] {
            assert!(!listener(&args).is_authentication_command(), "{args:?}");
        }
    }
    #[test]
    fn service_identity_ignores_port_configuration() {
        let args = |port: &str| vec!["node".into(), "api.js".into(), "--port".into(), port.into()];
        assert_eq!(
            command_identity(&args("3000")),
            command_identity(&args("3001"))
        );
        assert_ne!(
            command_identity(&args("3000")),
            command_identity(&["node".into(), "docs.js".into()])
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn accepts_only_loopback_reachable_listeners() {
        assert_eq!(
            proc_address("0100007F:1435", false).unwrap(),
            "127.0.0.1:5173".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            proc_address("00000000000000000000000001000000:1435", true).unwrap(),
            "[::1]:5173".parse::<SocketAddr>().unwrap()
        );
        assert!(proc_address("0101A8C0:1435", false).is_none());
        assert!(proc_address("malformed:00", true).is_none());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn same_process_matches_the_scanner_start_time() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _guard = Kill(child);
        // `listeners()` only reports sockets; derive the scanner's start time
        // through the same platform path it uses.
        #[cfg(target_os = "linux")]
        let started_at = {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let (_, tail) = stat.rsplit_once(") ").unwrap();
            let fields: Vec<_> = tail.split_whitespace().collect();
            let (boot, ticks) = linux_clock();
            linux_started_at(&fields, boot, ticks)
        };
        #[cfg(target_os = "macos")]
        let started_at = {
            let output = std::process::Command::new("/bin/ps")
                .args(["-axo", "pid=,ppid=,lstart=,command="])
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
                .find(|parts| parts.first().and_then(|p| p.parse::<u32>().ok()) == Some(pid))
                .map(|parts| parse_lstart(&parts[2..7]))
                .unwrap()
        };
        assert!(started_at > 0);
        assert!(same_process(pid, started_at));
        assert!(!same_process(pid, started_at + 1000));
        assert!(!same_process(pid, 0));
        drop(_guard);
        assert!(
            !same_process(pid, started_at),
            "a reaped pid no longer matches"
        );
    }
    #[tokio::test]
    async fn requires_an_http_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for response in [
                b"SSH-2.0-test\r\n".as_slice(),
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
            ] {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut input = [0; 512];
                let _ = client.read(&mut input).await;
                client.write_all(response).await.unwrap();
            }
        });
        assert!(!is_http(address).await);
        assert!(is_http(address).await);
        task.await.unwrap();
    }
}
