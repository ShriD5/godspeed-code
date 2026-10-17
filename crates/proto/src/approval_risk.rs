//! How risky an approval is, for the "Needs you" inbox: low-risk requests
//! can be approved in one batch, high-risk ones always show the exact
//! command or diff first.
//!
//! - **Low**: read-only commands (`ls`, `cat`, `rg`, `git status`/`diff`/`log`
//!   …) and file edits or reads inside the chat's workspace.
//! - **Medium**: anything else that stays in the workspace (builds, tests,
//!   commits, in-workspace deletes), and whatever the rules can't place.
//! - **High**: network access, `git push` / forced or hard resets, deletes or
//!   writes outside the workspace, `sudo`, package installs and publishing,
//!   secrets and env files, and MCP tools that may write externally.
//!
//! The rules are a reading of the command line, not a sandbox: they only
//! decide what may skip a second look, so anything unclear is never Low.

use serde::{Deserialize, Serialize};

use crate::{ApprovalKind, ApprovalRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalRisk {
    Low,
    Medium,
    High,
}

impl ApprovalRisk {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low risk",
            Self::Medium => "Medium risk",
            Self::High => "High risk",
        }
    }
}

/// The risk of `request` for a chat whose workspace is `workspace` (its
/// checkout path; `None` for a chat without one).
pub fn approval_risk(request: &ApprovalRequest, workspace: Option<&str>) -> ApprovalRisk {
    let scope = Scope::new(workspace, request.cwd.as_deref());
    match request.kind {
        ApprovalKind::Command => request
            .command
            .as_deref()
            .map_or(ApprovalRisk::Medium, |command| {
                command_risk(command, &scope)
            }),
        ApprovalKind::FileChange => paths_risk(&request.paths, &scope, Access::Write),
        ApprovalKind::FileRead => paths_risk(&request.paths, &scope, Access::Read),
        ApprovalKind::Network => ApprovalRisk::High,
        ApprovalKind::McpTool => mcp_risk(&request.title),
        ApprovalKind::Other => ApprovalRisk::Medium,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
    Delete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Location {
    Inside,
    Outside,
    Unknown,
}

/// The workspace root and the directory relative paths resolve against.
#[derive(Clone)]
struct Scope {
    root: Option<Vec<String>>,
    cwd: Option<Vec<String>>,
}

impl Scope {
    fn new(workspace: Option<&str>, cwd: Option<&str>) -> Self {
        let root = workspace.and_then(|w| absolute(w, None));
        let cwd = cwd
            .and_then(|c| absolute(c, root.as_deref()))
            .or_else(|| root.clone());
        Self { root, cwd }
    }

    fn locate(&self, path: &str) -> Location {
        let Some(root) = &self.root else {
            return Location::Unknown;
        };
        match absolute(path, self.cwd.as_deref()) {
            Some(path) if path.starts_with(root) => Location::Inside,
            Some(_) => Location::Outside,
            None => Location::Outside,
        }
    }

    /// Follow a `cd`: later commands on the line resolve against `dir`.
    fn change_dir(&mut self, dir: Option<&str>) {
        self.cwd = dir.and_then(|d| absolute(d, self.cwd.as_deref()));
    }

    fn cwd_inside(&self) -> bool {
        match (&self.root, &self.cwd) {
            (Some(root), Some(cwd)) => cwd.starts_with(root),
            _ => false,
        }
    }
}

/// Lexically normalized absolute components of `path`; `None` for a path
/// that can't be pinned down (`~`, variables, a relative path with no base).
fn absolute(path: &str, base: Option<&[String]>) -> Option<Vec<String>> {
    let path = path.trim();
    if path.is_empty() || path.starts_with('~') || path.contains('$') {
        return None;
    }
    let mut parts: Vec<String> = if path.starts_with('/') {
        Vec::new()
    } else {
        base?.to_vec()
    };
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            segment => parts.push(segment.to_string()),
        }
    }
    Some(parts)
}

fn paths_risk(paths: &[String], scope: &Scope, access: Access) -> ApprovalRisk {
    if paths.iter().any(|p| is_secret_path(p)) {
        return ApprovalRisk::High;
    }
    if paths.is_empty() {
        return ApprovalRisk::Medium;
    }
    paths
        .iter()
        .map(|path| location_risk(scope.locate(path), access))
        .max()
        .unwrap_or(ApprovalRisk::Medium)
}

fn location_risk(location: Location, access: Access) -> ApprovalRisk {
    match (access, location) {
        (Access::Read, Location::Inside) => ApprovalRisk::Low,
        (Access::Read, _) => ApprovalRisk::Medium,
        (Access::Write, Location::Inside) => ApprovalRisk::Low,
        (Access::Write, Location::Unknown) => ApprovalRisk::Medium,
        (Access::Delete, Location::Inside) => ApprovalRisk::Medium,
        (Access::Write | Access::Delete, _) => ApprovalRisk::High,
    }
}

const SECRET_NAMES: &[&str] = &[
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    "credentials",
    "credentials.json",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];
const SECRET_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];
const SECRET_EXTENSIONS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore"];

fn is_secret_path(path: &str) -> bool {
    let path = path.trim().trim_matches(['"', '\'']);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some((name, dirs)) = segments.split_last() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let env_file = name == ".env" || (name.starts_with(".env.") && !is_env_template(&name));
    env_file
        || SECRET_NAMES.contains(&name.as_str())
        || SECRET_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        || dirs.iter().any(|dir| SECRET_DIRS.contains(dir))
}

/// `.env.example` and friends document variables; they hold no secrets.
fn is_env_template(name: &str) -> bool {
    [".example", ".sample", ".template", ".dist", ".defaults"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

const MCP_READ_VERBS: &[&str] = &[
    "get", "list", "read", "search", "find", "query", "fetch", "view", "show", "describe",
    "lookup", "status", "inspect",
];

/// An MCP tool's risk from its name: read-ish verbs are Medium (they still
/// reach another system), anything else may write externally.
fn mcp_risk(title: &str) -> ApprovalRisk {
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .flat_map(split_camel)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.iter().any(|w| MCP_READ_VERBS.contains(&w.as_str())) {
        ApprovalRisk::Medium
    } else {
        ApprovalRisk::High
    }
}

fn split_camel(word: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in word.chars() {
        if c.is_ascii_uppercase() && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push(c);
    }
    out.push(current);
    out
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The highest risk of any simple command in `command`.
fn command_risk(command: &str, scope: &Scope) -> ApprovalRisk {
    let floor = if scope.cwd_inside() {
        ApprovalRisk::Low
    } else {
        ApprovalRisk::Medium
    };
    let mut risk = floor;
    if command.contains("$(") || command.contains('`') {
        risk = risk.max(ApprovalRisk::Medium);
    }
    let mut scope = scope.clone();
    for segment in split_commands(command) {
        if segment.words.first().is_some_and(|w| w == "cd") {
            scope.change_dir(segment.words.get(1).map(String::as_str));
            if !scope.cwd_inside() {
                risk = risk.max(ApprovalRisk::Medium);
            }
            continue;
        }
        risk = risk.max(segment_risk(&segment, &scope));
        if risk == ApprovalRisk::High {
            break;
        }
    }
    risk
}

/// One simple command: its words plus where its output is redirected.
#[derive(Debug, Default)]
struct Segment {
    words: Vec<String>,
    redirects: Vec<String>,
}

/// Split a shell line into simple commands on `;`, `&&`, `||`, `|`, `&` and
/// newlines, honouring quotes, and pull out `>`/`>>` redirection targets.
fn split_commands(command: &str) -> Vec<Segment> {
    let mut segments = vec![Segment::default()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut redirect_next = false;
    let mut chars = command.chars().peekable();

    fn finish(word: &mut String, in_word: &mut bool, redirect: &mut bool, seg: &mut Segment) {
        if *in_word {
            let w = std::mem::take(word);
            if *redirect {
                seg.redirects.push(w);
                *redirect = false;
            } else {
                seg.words.push(w);
            }
            *in_word = false;
        }
    }

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                in_word = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    in_word = true;
                }
            }
            ' ' | '\t' => {
                let seg = segments.last_mut().expect("one segment");
                finish(&mut word, &mut in_word, &mut redirect_next, seg);
            }
            ';' | '|' | '&' | '\n' => {
                let seg = segments.last_mut().expect("one segment");
                finish(&mut word, &mut in_word, &mut redirect_next, seg);
                if matches!(chars.peek(), Some('|' | '&')) {
                    chars.next();
                }
                segments.push(Segment::default());
            }
            '>' => {
                let seg = segments.last_mut().expect("one segment");
                let fd_dup = word == "2" || word == "1";
                if fd_dup {
                    word.clear();
                    in_word = false;
                }
                finish(&mut word, &mut in_word, &mut redirect_next, seg);
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                if chars.peek() == Some(&'&') {
                    chars.next();
                    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next();
                    }
                    continue;
                }
                redirect_next = true;
            }
            '<' => {
                let seg = segments.last_mut().expect("one segment");
                finish(&mut word, &mut in_word, &mut redirect_next, seg);
            }
            _ => {
                word.push(c);
                in_word = true;
            }
        }
    }
    let seg = segments.last_mut().expect("one segment");
    finish(&mut word, &mut in_word, &mut redirect_next, seg);
    segments.retain(|s| !s.words.is_empty() || !s.redirects.is_empty());
    segments
}

/// Commands that only read, with no flag that makes them write.
const READ_ONLY: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "rg",
    "grep",
    "egrep",
    "fgrep",
    "ag",
    "fd",
    "tree",
    "pwd",
    "echo",
    "printf",
    "which",
    "whereis",
    "type",
    "file",
    "stat",
    "du",
    "df",
    "date",
    "whoami",
    "uname",
    "diff",
    "cmp",
    "sort",
    "uniq",
    "cut",
    "tr",
    "jq",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "true",
    "false",
    "test",
    "[",
    "cd",
    "nl",
    "column",
    "od",
    "hexdump",
    "xxd",
    "sha256sum",
    "shasum",
    "md5sum",
    "md5",
    "id",
    "hostname",
    "uptime",
    "ps",
    "lsof",
    "bat",
    "exa",
    "eza",
    "comm",
    "paste",
    "fold",
    "rev",
    "seq",
    "sleep",
    "tput",
];
const NETWORK: &[&str] = &[
    "curl",
    "wget",
    "ssh",
    "scp",
    "sftp",
    "ftp",
    "telnet",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "http",
    "https",
    "xh",
    "aria2c",
    "rsync",
    "gh",
    "aws",
    "gcloud",
    "az",
    "kubectl",
    "docker",
    "flyctl",
    "vercel",
    "wrangler",
    "heroku",
    "terraform",
];
const PRIVILEGED: &[&str] = &["sudo", "doas", "su", "pkexec", "launchctl", "systemctl"];
const DELETES: &[&str] = &["rm", "rmdir", "unlink", "shred", "trash", "srm"];
const WRITES_LAST: &[&str] = &["cp", "mv", "ln", "install"];
const WRITES_ALL: &[&str] = &[
    "touch", "mkdir", "chmod", "chown", "chgrp", "tee", "truncate",
];
const WRAPPERS: &[&str] = &["env", "time", "nice", "nohup", "command", "builtin", "exec"];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish"];

fn segment_risk(segment: &Segment, scope: &Scope) -> ApprovalRisk {
    let mut risk = ApprovalRisk::Low;
    for target in &segment.redirects {
        if target == "/dev/null" || target.starts_with("/dev/std") {
            continue;
        }
        risk = risk.max(target_risk(target, scope, Access::Write).max(ApprovalRisk::Medium));
    }
    let words = &segment.words;
    let mut at = 0;
    while at < words.len() && is_assignment(&words[at]) {
        at += 1;
    }
    // A bare `env` or `printenv` dumps the environment, secrets included.
    if words.len() == at && at > 0 {
        return risk.max(ApprovalRisk::Medium);
    }
    let Some(program) = words.get(at) else {
        return risk;
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    let args = &words[at + 1..];
    if WRAPPERS.contains(&program) {
        if args.is_empty() && program == "env" {
            return ApprovalRisk::High;
        }
        let rest = Segment {
            words: args
                .iter()
                .skip_while(|a| a.starts_with('-') || is_assignment(a))
                .cloned()
                .collect(),
            redirects: Vec::new(),
        };
        return risk.max(segment_risk(&rest, scope));
    }
    if args.iter().any(|a| is_secret_path(a)) || program == "printenv" || program == "security" {
        return ApprovalRisk::High;
    }
    risk.max(program_risk(program, args, scope))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn program_risk(program: &str, args: &[String], scope: &Scope) -> ApprovalRisk {
    if PRIVILEGED.contains(&program) || NETWORK.contains(&program) {
        return ApprovalRisk::High;
    }
    if SHELLS.contains(&program) {
        return match args.iter().position(|a| a == "-c" || a == "-lc") {
            Some(i) => args
                .get(i + 1)
                .map_or(ApprovalRisk::Medium, |inner| command_risk(inner, scope)),
            None => ApprovalRisk::Medium,
        };
    }
    if DELETES.contains(&program) {
        return path_args(args)
            .map(|p| target_risk(p, scope, Access::Delete))
            .max()
            .unwrap_or(ApprovalRisk::Medium)
            .max(ApprovalRisk::Medium);
    }
    if WRITES_LAST.contains(&program) {
        return path_args(args)
            .last()
            .map_or(ApprovalRisk::Medium, |p| {
                target_risk(p, scope, Access::Write)
            })
            .max(ApprovalRisk::Medium);
    }
    if WRITES_ALL.contains(&program) {
        return path_args(args)
            .map(|p| target_risk(p, scope, Access::Write))
            .max()
            .unwrap_or(ApprovalRisk::Medium)
            .max(ApprovalRisk::Medium);
    }
    match program {
        "git" => git_risk(args),
        "find" => find_risk(args, scope),
        "sed" | "perl" if args.iter().any(|a| a.starts_with("-i")) => ApprovalRisk::Medium,
        "dd" => ApprovalRisk::High,
        "npm" | "pnpm" | "yarn" | "bun" => js_package_risk(program, args),
        "npx" | "pnpx" | "bunx" | "dlx" => ApprovalRisk::High,
        "pip" | "pip3" | "pipx" | "gem" | "brew" | "apt" | "apt-get" | "yum" | "dnf" | "pacman"
        | "port" | "conda" => package_manager_risk(args),
        "cargo" => subcommand_risk(args, &["install", "publish", "login", "owner", "yank"]),
        "go" => subcommand_risk(args, &["install", "get"]),
        "uv" => subcommand_risk(args, &["add", "pip", "tool", "publish"]),
        "poetry" => subcommand_risk(args, &["add", "publish", "install"]),
        program if READ_ONLY.contains(&program) => ApprovalRisk::Low,
        _ => ApprovalRisk::Medium,
    }
}

/// Non-flag arguments: the paths a file command acts on.
fn path_args(args: &[String]) -> impl DoubleEndedIterator<Item = &str> {
    args.iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
}

fn target_risk(path: &str, scope: &Scope, access: Access) -> ApprovalRisk {
    if is_secret_path(path) {
        return ApprovalRisk::High;
    }
    location_risk(scope.locate(path), access)
}

const GIT_READ_ONLY: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "describe",
    "shortlog",
    "grep",
    "show-ref",
    "merge-base",
    "cat-file",
    "rev-list",
    "whatchanged",
    "name-rev",
    "for-each-ref",
    "check-ignore",
    "count-objects",
    "help",
    "version",
];

fn git_risk(args: &[String]) -> ApprovalRisk {
    let mut rest = args.iter().map(String::as_str);
    let mut sub = None;
    while let Some(arg) = rest.next() {
        match arg {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                rest.next();
            }
            flag if flag.starts_with('-') => {}
            arg => {
                sub = Some(arg);
                break;
            }
        }
    }
    let Some(sub) = sub else {
        return ApprovalRisk::Low;
    };
    let rest: Vec<&str> = rest.collect();
    let has = |flags: &[&str]| rest.iter().any(|a| flags.contains(a));
    let forced = has(&["-f", "--force", "--force-with-lease"])
        || rest
            .iter()
            .any(|a| a.starts_with("--force") || a.starts_with('+'));
    match sub {
        "push" | "send-email" | "request-pull" | "filter-branch" | "filter-repo" => {
            ApprovalRisk::High
        }
        "reset" if has(&["--hard", "--merge", "--keep"]) => ApprovalRisk::High,
        "clean"
            if rest
                .iter()
                .any(|a| is_short_flag_with(a, 'f') || *a == "--force") =>
        {
            ApprovalRisk::High
        }
        "checkout" | "switch" if forced => ApprovalRisk::High,
        "branch" if has(&["-D"]) || forced => ApprovalRisk::High,
        "update-ref" if has(&["-d"]) => ApprovalRisk::High,
        "clone" | "submodule" => ApprovalRisk::High,
        "remote" if rest.is_empty() || has(&["-v", "--verbose", "show", "get-url"]) => {
            ApprovalRisk::Low
        }
        "branch" if rest.is_empty() || rest.iter().all(|a| is_branch_listing(a)) => {
            ApprovalRisk::Low
        }
        "tag" if rest.is_empty() || has(&["-l", "--list"]) => ApprovalRisk::Low,
        "stash" if rest.first().is_some_and(|a| *a == "list" || *a == "show") => ApprovalRisk::Low,
        "config" if has(&["--get", "--list", "-l", "--get-all"]) => ApprovalRisk::Low,
        "reflog" if rest.is_empty() || rest.first() == Some(&"show") => ApprovalRisk::Low,
        sub if GIT_READ_ONLY.contains(&sub) => ApprovalRisk::Low,
        _ => ApprovalRisk::Medium,
    }
}

fn is_short_flag_with(arg: &str, flag: char) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flag)
}

fn is_branch_listing(arg: &str) -> bool {
    matches!(
        arg,
        "-a" | "-r" | "-v" | "-vv" | "--all" | "--remotes" | "--list" | "-l" | "--show-current"
    ) || arg.starts_with("--contains")
        || arg.starts_with("--merged")
        || arg.starts_with("--no-merged")
        || arg.starts_with("--sort")
        || arg.starts_with("--format")
}

fn find_risk(args: &[String], scope: &Scope) -> ApprovalRisk {
    if args.iter().any(|a| a == "-delete") {
        return path_args(args)
            .take_while(|a| !a.starts_with('('))
            .next()
            .map_or(ApprovalRisk::High, |root| {
                target_risk(root, scope, Access::Delete)
            })
            .max(ApprovalRisk::Medium);
    }
    if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint"
        )
    }) {
        return ApprovalRisk::Medium;
    }
    ApprovalRisk::Low
}

fn first_subcommand(args: &[String]) -> Option<&str> {
    args.iter()
        .map(String::as_str)
        .find(|a| !a.starts_with('-'))
}

fn js_package_risk(program: &str, args: &[String]) -> ApprovalRisk {
    let Some(sub) = first_subcommand(args) else {
        return if program == "yarn" || program == "bun" {
            ApprovalRisk::High
        } else {
            ApprovalRisk::Medium
        };
    };
    match sub {
        "install" | "i" | "ci" | "add" | "publish" | "unpublish" | "link" | "update"
        | "upgrade" | "up" | "login" | "adduser" | "exec" | "dlx" | "x" | "create" | "init"
        | "deprecate" | "dist-tag" | "owner" | "token" | "global" => ApprovalRisk::High,
        "ls" | "list" | "outdated" | "view" | "info" | "why" | "explain" | "--version" => {
            ApprovalRisk::Low
        }
        _ => ApprovalRisk::Medium,
    }
}

fn package_manager_risk(args: &[String]) -> ApprovalRisk {
    match first_subcommand(args) {
        Some("list" | "show" | "info" | "search" | "freeze" | "check" | "outdated") => {
            ApprovalRisk::Low
        }
        _ => ApprovalRisk::High,
    }
}

fn subcommand_risk(args: &[String], high: &[&str]) -> ApprovalRisk {
    match first_subcommand(args) {
        Some(sub) if high.contains(&sub) => ApprovalRisk::High,
        _ => ApprovalRisk::Medium,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/Users/me/code/app";

    fn command(cmd: &str) -> ApprovalRisk {
        let mut request = ApprovalRequest::new(ApprovalKind::Command, cmd);
        request.command = Some(cmd.into());
        request.cwd = Some(WS.into());
        approval_risk(&request, Some(WS))
    }

    fn files(kind: ApprovalKind, paths: &[&str]) -> ApprovalRisk {
        let mut request = ApprovalRequest::new(kind, "files");
        request.paths = paths.iter().map(|p| p.to_string()).collect();
        approval_risk(&request, Some(WS))
    }

    #[test]
    fn command_table() {
        use ApprovalRisk::*;
        let table: &[(&str, ApprovalRisk)] = &[
            // Read-only.
            ("ls -la", Low),
            ("cat src/main.rs", Low),
            ("rg -n 'fn main' src", Low),
            ("git status", Low),
            ("git diff --stat HEAD~1", Low),
            ("git log --oneline -20", Low),
            ("git -C sub status", Low),
            ("git branch -a", Low),
            ("git remote -v", Low),
            ("head -50 README.md | grep foo", Low),
            ("find . -name '*.rs'", Low),
            ("wc -l src/*.rs && ls", Low),
            ("cat file 2>/dev/null", Low),
            ("cd src && ls", Low),
            ("ls 2>&1 | head", Low),
            // In-workspace work.
            ("cargo test -p engine", Medium),
            ("cargo build", Medium),
            ("npm test", Medium),
            ("npm run build", Medium),
            ("git commit -m 'feat: x'", Medium),
            ("git add -A", Medium),
            ("git checkout -b feature", Medium),
            ("git fetch origin", Medium),
            ("git stash", Medium),
            ("rm -rf target", Medium),
            ("rm src/old.rs", Medium),
            ("mkdir -p build/out", Medium),
            ("echo hi > notes.txt", Medium),
            ("sed -i 's/a/b/' src/lib.rs", Medium),
            ("find . -name '*.tmp' -exec rm {} \\;", Medium),
            ("python3 scripts/gen.py", Medium),
            ("make", Medium),
            ("echo $(date)", Medium),
            ("bash -c 'cargo test'", Medium),
            ("bash -c 'ls'", Low),
            ("FOO=1 cargo test", Medium),
            // Network.
            ("curl https://example.com", High),
            ("wget http://x/y.tar.gz", High),
            ("ls && curl -X POST https://x", High),
            ("ssh host uptime", High),
            ("gh pr create", High),
            // Git destruction and publishing.
            ("git push", High),
            ("git push origin main --force", High),
            ("git reset --hard HEAD~3", High),
            ("git clean -fdx", High),
            ("git branch -D feature", High),
            ("git checkout -f main", High),
            ("git clone https://github.com/x/y", High),
            // Deletes and writes outside the workspace.
            ("rm -rf /", High),
            ("rm -rf ~/Documents", High),
            ("rm ../other-repo/file", High),
            ("rm -rf $HOME/x", High),
            ("cd /tmp && rm -rf *", High),
            ("cp build/app /usr/local/bin/app", High),
            ("echo x > /etc/hosts", High),
            ("echo x >> ~/.zshrc", High),
            ("touch /tmp/marker", High),
            ("find / -name core -delete", High),
            ("dd if=/dev/zero of=disk.img", High),
            // Privilege and packages.
            ("sudo rm -rf build", High),
            ("sudo ls", High),
            ("npm install left-pad", High),
            ("npm i", High),
            ("pnpm add zod", High),
            ("yarn", High),
            ("npm publish", High),
            ("npx create-react-app x", High),
            ("pip install requests", High),
            ("cargo install ripgrep", High),
            ("cargo publish", High),
            ("brew install jq", High),
            ("go install golang.org/x/tools/gopls@latest", High),
            // Secrets and env.
            ("cat .env", High),
            ("cat .env.local", High),
            ("cat config/.env.production", High),
            ("cat ~/.ssh/id_rsa", High),
            ("cat ~/.aws/credentials", High),
            ("cat certs/server.key", High),
            ("printenv", High),
            ("env", High),
            ("security find-generic-password -s x", High),
            // Templates are documentation.
            ("cat .env.example", Low),
        ];
        let failures: Vec<String> = table
            .iter()
            .filter(|(cmd, expected)| command(cmd) != *expected)
            .map(|(cmd, expected)| {
                format!("{cmd:?}: expected {expected:?}, got {:?}", command(cmd))
            })
            .collect();
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    #[test]
    fn file_requests_follow_the_workspace() {
        use ApprovalRisk::*;
        assert_eq!(files(ApprovalKind::FileChange, &["src/lib.rs"]), Low);
        assert_eq!(
            files(ApprovalKind::FileChange, &[&format!("{WS}/src/lib.rs")]),
            Low
        );
        assert_eq!(files(ApprovalKind::FileChange, &["/etc/hosts"]), High);
        assert_eq!(files(ApprovalKind::FileChange, &["../sibling/x.rs"]), High);
        assert_eq!(
            files(ApprovalKind::FileChange, &["src/a.rs", "/tmp/b"]),
            High
        );
        assert_eq!(files(ApprovalKind::FileChange, &[".env"]), High);
        assert_eq!(files(ApprovalKind::FileChange, &[]), Medium);
        assert_eq!(files(ApprovalKind::FileRead, &["src/lib.rs"]), Low);
        assert_eq!(
            files(ApprovalKind::FileRead, &["/var/log/system.log"]),
            Medium
        );
        assert_eq!(
            files(ApprovalKind::FileRead, &["/Users/me/.ssh/config"]),
            High
        );
    }

    #[test]
    fn other_kinds() {
        use ApprovalRisk::*;
        let network = ApprovalRequest::new(ApprovalKind::Network, "Fetch example.com");
        assert_eq!(approval_risk(&network, Some(WS)), High);
        let read_tool = ApprovalRequest::new(ApprovalKind::McpTool, "github.list_issues");
        assert_eq!(approval_risk(&read_tool, Some(WS)), Medium);
        let camel = ApprovalRequest::new(ApprovalKind::McpTool, "linear: getIssue");
        assert_eq!(approval_risk(&camel, Some(WS)), Medium);
        let write_tool = ApprovalRequest::new(ApprovalKind::McpTool, "slack.post_message");
        assert_eq!(approval_risk(&write_tool, Some(WS)), High);
        let other = ApprovalRequest::new(ApprovalKind::Other, "Something");
        assert_eq!(approval_risk(&other, Some(WS)), Medium);
        let no_command = ApprovalRequest::new(ApprovalKind::Command, "Run something");
        assert_eq!(approval_risk(&no_command, Some(WS)), Medium);
    }

    #[test]
    fn without_a_workspace_nothing_is_low_and_deletes_are_high() {
        use ApprovalRisk::*;
        let mut edit = ApprovalRequest::new(ApprovalKind::FileChange, "Edit");
        edit.paths = vec!["notes.md".into()];
        assert_eq!(approval_risk(&edit, None), Medium);
        let mut ls = ApprovalRequest::new(ApprovalKind::Command, "ls");
        ls.command = Some("ls".into());
        assert_eq!(approval_risk(&ls, None), Medium);
        ls.command = Some("rm notes.md".into());
        assert_eq!(approval_risk(&ls, None), High);
    }

    #[test]
    fn a_command_run_outside_the_workspace_is_never_low() {
        let mut request = ApprovalRequest::new(ApprovalKind::Command, "ls");
        request.command = Some("ls".into());
        request.cwd = Some("/Users/me".into());
        assert_eq!(approval_risk(&request, Some(WS)), ApprovalRisk::Medium);
        request.command = Some("rm notes.md".into());
        assert_eq!(approval_risk(&request, Some(WS)), ApprovalRisk::High);
    }
}
