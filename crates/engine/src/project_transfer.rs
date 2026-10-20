//! Adding a project to another device ([`zeron_proto::project_transfer`]).
//!
//! On the source device: list the untracked files git ignores in a project
//! and read the ones the user picked, a chunk at a time. On the receiving
//! device: clone the repository into a fresh folder, register the project,
//! and write the relayed files into the new checkout.
//!
//! The viewport carries the files from one engine to the other over each
//! engine's own authenticated link; engines never talk to each other here.
//! Every path is relative to a registered project folder, checked component
//! by component: no `..`, no absolute paths, nothing under `.git`, and no
//! symlink is ever followed out of the project.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::AsyncReadExt as _;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use zeron_proto::project_transfer::{
    CloneProgress, CloneProjectRequest, IgnoredFile, IgnoredFileListing, MAX_LISTED_IGNORED_FILES,
    MAX_RELAY_FILE_BYTES, ProjectFileChunk, RELAY_CHUNK_BYTES, WriteProjectFileOutcome,
    WriteProjectFileRequest,
};

use crate::EngineError;
use crate::repos::{Repos, noninteractive_git};
use crate::workspace_host::WorkspaceHost;

/// A listing's git call, at most.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// A listing's git output, at most (a huge ignored tree is cut, not read).
const LIST_MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// A clone, at most. Longer is a wedged transfer; the user can retry.
const CLONE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Relative paths longer than this are refused before touching the disk.
const MAX_RELATIVE_PATH_BYTES: usize = 4096;
/// Tail of git's stderr kept for a failure message.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

fn failed(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// A client-supplied path below a project folder, as a relative `PathBuf`.
/// Either separator splits; every part must be a plain name, never `.`,
/// `..`, empty, a drive or stream (`:`) or `.git`.
pub fn relative_project_path(path: &str) -> Result<PathBuf, EngineError> {
    if path.is_empty() || path.len() > MAX_RELATIVE_PATH_BYTES || path.contains('\0') {
        return Err(failed("Invalid file path"));
    }
    let mut relative = PathBuf::new();
    for part in path.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            return Err(failed(format!("Invalid file path: {path}")));
        }
        if part.eq_ignore_ascii_case(".git") {
            return Err(failed("Files under .git are never copied"));
        }
        relative.push(part);
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(failed(format!("Invalid file path: {path}")));
    }
    Ok(relative)
}

fn canonical_root(root: &Path) -> Result<PathBuf, EngineError> {
    std::fs::canonicalize(root).map_err(|_| failed("The project folder is unavailable"))
}

/// `relative`'s parent folders below `root`, refusing any that is a symlink
/// or not a folder. With `create`, missing ones are made one at a time (never
/// `create_dir_all`, which would follow a planted symlink out of the root).
fn checked_parent(root: &Path, relative: &Path, create: bool) -> Result<PathBuf, EngineError> {
    let mut current = root.to_path_buf();
    let parts: Vec<_> = relative.components().collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(failed("A folder on the way is a symlink"));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(failed("A folder on the way is a file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                std::fs::create_dir(&current)?;
            }
            Err(_) => return Err(failed("The file's folder is missing")),
        }
    }
    Ok(current)
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

fn git_command(cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x08000000);
    }
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    noninteractive_git(&mut cmd);
    cmd
}

/// Trimmed stdout of a git command that succeeded, else `None`.
async fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(LIST_TIMEOUT, git_command(cwd).args(args).output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git ls-files` of ignored, untracked entries, NUL-separated. `--directory`
/// reports a folder holding nothing but ignored files as one `name/` entry,
/// which is how `node_modules` and build output drop out. `None` outside a
/// repository; a cut-off output keeps only its complete entries.
async fn ignored_entries(root: &Path) -> Result<Option<Vec<String>>, EngineError> {
    let mut child = git_command(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
            "-z",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| failed(format!("git failed to start: {error}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let run = async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(LIST_MAX_OUTPUT)
            .read_to_end(&mut bytes)
            .await?;
        let cut = bytes.len() as u64 >= LIST_MAX_OUTPUT;
        if cut {
            child.start_kill().ok();
        }
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((bytes, cut, status))
    };
    let (bytes, cut, status) = match tokio::time::timeout(LIST_TIMEOUT, run).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => return Err(failed(format!("Listing ignored files failed: {error}"))),
        Err(_) => return Err(failed("Listing ignored files timed out")),
    };
    if !cut && !status.success() {
        return Ok(None);
    }
    let mut entries: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    if cut || bytes.last() != Some(&0) {
        entries.pop(); // the last entry may be incomplete
    }
    Ok(Some(
        entries
            .into_iter()
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| std::str::from_utf8(entry).ok())
            .map(str::to_string)
            .collect(),
    ))
}

/// The files a clone elsewhere is missing: ignored, untracked, regular files
/// (no symlinks, nothing under `.git`, no wholly ignored folders), sorted,
/// at most [`MAX_LISTED_IGNORED_FILES`], with what the clone itself needs.
pub async fn list_ignored_files(root: &Path) -> Result<IgnoredFileListing, EngineError> {
    let root = canonical_root(root)?;
    let Some(mut entries) = ignored_entries(&root).await? else {
        return Ok(IgnoredFileListing::default());
    };
    entries.retain(|entry| !entry.ends_with('/'));
    entries.sort();
    entries.dedup();
    let scan_root = root.clone();
    let (files, truncated) = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        for entry in entries {
            let Ok(relative) = relative_project_path(&entry) else {
                continue;
            };
            let full = scan_root.join(&relative);
            // lstat: a symlink points wherever it likes, so it is never offered.
            let Ok(meta) = std::fs::symlink_metadata(&full) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            if files.len() == MAX_LISTED_IGNORED_FILES {
                return (files, true);
            }
            let contained = std::fs::canonicalize(&full)
                .is_ok_and(|canonical| canonical.starts_with(&scan_root));
            if contained {
                files.push(IgnoredFile {
                    path: entry.replace('\\', "/"),
                    size: meta.len(),
                });
            }
        }
        (files, false)
    })
    .await
    .map_err(|error| failed(error.to_string()))?;

    let remote_url = match git_text(&root, &["remote", "get-url", "origin"]).await {
        Some(url) if !url.is_empty() => Some(url),
        _ => match git_text(&root, &["remote"]).await {
            Some(names) => match names.lines().next().map(str::trim) {
                Some(name) if !name.is_empty() => {
                    git_text(&root, &["remote", "get-url", name]).await
                }
                _ => None,
            },
            None => None,
        },
    };
    let subdirectory = git_text(&root, &["rev-parse", "--show-prefix"])
        .await
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    let repository_name = git_text(&root, &["rev-parse", "--show-toplevel"])
        .await
        .and_then(|top| {
            Path::new(&top)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    Ok(IgnoredFileListing {
        files,
        truncated,
        remote_url,
        subdirectory,
        repository_name,
    })
}

// ---------------------------------------------------------------------------
// Reading and writing relayed files
// ---------------------------------------------------------------------------

/// One chunk of a file below `root`, at most [`RELAY_CHUNK_BYTES`]. Refuses
/// symlinks, anything resolving outside the project and files over
/// [`MAX_RELAY_FILE_BYTES`].
pub async fn read_file_chunk(
    root: &Path,
    path: &str,
    offset: u64,
) -> Result<ProjectFileChunk, EngineError> {
    let relative = relative_project_path(path)?;
    let root = canonical_root(root)?;
    tokio::task::spawn_blocking(move || {
        let full = root.join(&relative);
        let meta = std::fs::symlink_metadata(&full).map_err(|_| failed("No such file"))?;
        if !meta.file_type().is_file() {
            return Err(failed("Only regular files are copied"));
        }
        let canonical = std::fs::canonicalize(&full).map_err(|_| failed("No such file"))?;
        if !canonical.starts_with(&root) {
            return Err(failed("The file is outside the project"));
        }
        let size = meta.len();
        if size > MAX_RELAY_FILE_BYTES {
            return Err(failed("Larger than 1 MiB"));
        }
        if offset > size {
            return Err(failed("The file changed while it was read"));
        }
        let mut file = std::fs::File::open(&canonical)?;
        file.seek(SeekFrom::Start(offset))?;
        let want = (size - offset).min(RELAY_CHUNK_BYTES as u64) as usize;
        let mut buffer = vec![0; want];
        file.read_exact(&mut buffer)
            .map_err(|_| failed("The file changed while it was read"))?;
        let next_offset = offset + want as u64;
        Ok(ProjectFileChunk {
            data: BASE64.encode(&buffer),
            size,
            next_offset,
            done: next_offset >= size,
        })
    })
    .await
    .map_err(|error| failed(error.to_string()))?
}

/// Is `relative` tracked in the repository at `root`?
async fn is_tracked(root: &Path, relative: &Path) -> Result<bool, EngineError> {
    let path = relative.to_string_lossy().replace('\\', "/");
    let output = tokio::time::timeout(
        LIST_TIMEOUT,
        git_command(root)
            .args(["ls-files", "-z", "--", &path])
            .output(),
    )
    .await
    .map_err(|_| failed("Checking the file against git timed out"))?
    .map_err(|error| failed(format!("git failed to start: {error}")))?;
    if !output.status.success() {
        return Err(failed("Couldn't check the file against git"));
    }
    Ok(!output.stdout.is_empty())
}

fn partial_name(relative: &Path) -> String {
    let name = relative
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(".{name}.godspeed-relay")
}

/// Write one chunk of a relayed file below `root`. Offset 0 starts a staging
/// file beside the destination, later chunks must continue it exactly, and
/// the `done` chunk renames it into place. Tracked files are never replaced;
/// an untracked file there already (an earlier attempt) is.
pub async fn write_file_chunk(
    root: &Path,
    request: WriteProjectFileRequest,
) -> Result<WriteProjectFileOutcome, EngineError> {
    let relative = relative_project_path(&request.path)?;
    let data = BASE64
        .decode(request.data.as_bytes())
        .map_err(|_| failed("The relayed chunk is not valid base64"))?;
    if data.len() > RELAY_CHUNK_BYTES {
        return Err(failed("The relayed chunk is too large"));
    }
    let written = request.offset + data.len() as u64;
    if written > MAX_RELAY_FILE_BYTES {
        return Err(failed("Larger than 1 MiB"));
    }
    let root = canonical_root(root)?;
    if request.offset == 0 && is_tracked(&root, &relative).await? {
        return Err(failed("Git tracks this file in the new checkout"));
    }
    let offset = request.offset;
    let done = request.done;
    tokio::task::spawn_blocking(move || {
        let parent = checked_parent(&root, &relative, offset == 0)?;
        let canonical_parent =
            std::fs::canonicalize(&parent).map_err(|_| failed("The file's folder is missing"))?;
        if !canonical_parent.starts_with(&root) {
            return Err(failed("The file is outside the project"));
        }
        let destination = root.join(&relative);
        match std::fs::symlink_metadata(&destination) {
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(failed("Something other than a file is in the way"));
            }
            _ => {}
        }
        let partial = parent.join(partial_name(&relative));
        let mut file = if offset == 0 {
            match std::fs::symlink_metadata(&partial) {
                Ok(meta) if meta.is_dir() => {
                    return Err(failed("Something other than a file is in the way"));
                }
                Ok(_) => std::fs::remove_file(&partial)?,
                Err(_) => {}
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)?
        } else {
            let meta = std::fs::symlink_metadata(&partial)
                .map_err(|_| failed("The relayed file was not started"))?;
            if !meta.file_type().is_file() || meta.len() != offset {
                return Err(failed("The relayed chunks arrived out of order"));
            }
            std::fs::OpenOptions::new().append(true).open(&partial)?
        };
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        if done {
            std::fs::rename(&partial, &destination)?;
        }
        Ok(WriteProjectFileOutcome { written, done })
    })
    .await
    .map_err(|error| failed(error.to_string()))?
}

// ---------------------------------------------------------------------------
// Cloning
// ---------------------------------------------------------------------------

struct CloneEntry {
    cancel: CancellationToken,
}

/// Clones in flight on this engine, by client-minted id.
#[derive(Clone, Default)]
pub struct CloneRegistry {
    inner: Arc<Mutex<HashMap<String, CloneEntry>>>,
}

impl CloneRegistry {
    fn insert(&self, clone_id: &str, cancel: CancellationToken) -> Result<(), EngineError> {
        let mut clones = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if clones.contains_key(clone_id) {
            return Err(failed("That clone is already running"));
        }
        clones.insert(clone_id.to_string(), CloneEntry { cancel });
        Ok(())
    }

    fn remove(&self, clone_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(clone_id);
    }

    /// Cancel a running clone; `false` when none has that id.
    pub fn cancel(&self, clone_id: &str) -> bool {
        match self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(clone_id)
        {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/// `Receiving objects:  45% (450/1000)` → ("Receiving objects", 45).
fn parse_progress(line: &str) -> Option<(String, Option<u8>)> {
    let line = line.trim().trim_start_matches("remote:").trim();
    let (stage, rest) = line.split_once(':')?;
    let stage = stage.trim();
    if stage.is_empty() || stage.len() > 40 || !stage.chars().all(|c| c.is_alphanumeric() || c == ' ')
    {
        return None;
    }
    let percent = rest
        .trim()
        .split_once('%')
        .and_then(|(number, _)| number.trim().parse::<u8>().ok())
        .map(|value| value.min(100));
    percent.map(|percent| (stage.to_string(), Some(percent)))
}

/// The message to show for a failed clone: git's `fatal:` line, else its
/// last line.
fn clone_failure(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty() && parse_progress(line).is_none())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .or(lines.last())
        .copied()
        .unwrap_or("git clone failed");
    line.trim_start_matches("fatal:")
        .trim_start_matches("error:")
        .trim()
        .to_string()
}

/// The folder a clone lands in, checked: absolute after `~` expansion, and
/// missing or an empty folder.
fn clone_destination(destination: &str) -> Result<PathBuf, EngineError> {
    let destination = destination.trim();
    if destination.is_empty() {
        return Err(failed("Choose a folder on the other device"));
    }
    let expanded = crate::repos::expand_home(destination).map_err(failed)?;
    let path = PathBuf::from(&expanded);
    if !path.is_absolute() {
        return Err(failed("Use a full path, or one starting with ~/"));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(failed("The folder can't contain .."));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {
            let mut entries = std::fs::read_dir(&path)?;
            if entries.next().is_some() {
                return Err(failed(format!("{} is not empty", path.display())));
            }
        }
        Ok(_) => return Err(failed(format!("{} already exists", path.display()))),
        Err(_) => {}
    }
    Ok(path)
}

/// What to remove when a clone fails: the topmost folder the clone created,
/// or the destination's contents when it was an existing empty folder.
#[derive(Debug, Clone)]
enum Cleanup {
    Remove(PathBuf),
    Empty(PathBuf),
}

impl Cleanup {
    fn for_destination(destination: &Path) -> Self {
        if destination.exists() {
            return Self::Empty(destination.to_path_buf());
        }
        let mut topmost = destination.to_path_buf();
        while let Some(parent) = topmost.parent() {
            if parent.as_os_str().is_empty() || parent.exists() {
                break;
            }
            topmost = parent.to_path_buf();
        }
        Self::Remove(topmost)
    }

    fn run(&self) {
        match self {
            Self::Remove(path) => {
                if let Err(error) = std::fs::remove_dir_all(path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %path.display(), %error, "half-cloned folder left behind");
                }
            }
            Self::Empty(path) => {
                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.flatten() {
                        let entry = entry.path();
                        let removed = match std::fs::symlink_metadata(&entry) {
                            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&entry),
                            _ => std::fs::remove_file(&entry),
                        };
                        if let Err(error) = removed {
                            tracing::warn!(path = %entry.display(), %error, "half-cloned file left behind");
                        }
                    }
                }
            }
        }
    }
}

/// Start `git clone` for `request` and report through the returned
/// receiver. The clone runs on its own task: a caller dropping its stream
/// doesn't stop it, only [`CloneRegistry::cancel`] does.
pub fn start_clone(
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    request: CloneProjectRequest,
) -> Result<watch::Receiver<CloneProgress>, EngineError> {
    let clone_id = request.clone_id.trim().to_string();
    if clone_id.is_empty() || request.space_id.trim().is_empty() {
        return Err(failed("Missing clone or project id"));
    }
    let url = request.url.trim().to_string();
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return Err(failed("Invalid repository URL"));
    }
    let subdirectory = match request.subdirectory.trim_matches(['/', '\\']) {
        "" => None,
        sub => Some(relative_project_path(sub)?),
    };
    let destination = clone_destination(&request.destination)?;
    let registry = repos.project_clones().clone();
    let cancel = CancellationToken::new();
    registry.insert(&clone_id, cancel.clone())?;
    let (tx, rx) = watch::channel(CloneProgress::Starting);
    tokio::spawn(async move {
        let outcome = run_clone(
            &repos,
            &workspace,
            &device_id,
            &url,
            &destination,
            subdirectory.as_deref(),
            &request.space_id,
            request.name.clone(),
            &cancel,
            &tx,
        )
        .await;
        registry.remove(&clone_id);
        tx.send_replace(outcome);
    });
    Ok(rx)
}

#[allow(clippy::too_many_arguments)] // one call site, all of it the request
async fn run_clone(
    repos: &Repos,
    workspace: &WorkspaceHost,
    device_id: &str,
    url: &str,
    destination: &Path,
    subdirectory: Option<&Path>,
    space_id: &str,
    name: Option<String>,
    cancel: &CancellationToken,
    progress: &watch::Sender<CloneProgress>,
) -> CloneProgress {
    let cleanup = Cleanup::for_destination(destination);
    let parent = destination.parent().unwrap_or(destination);
    if let Err(error) = std::fs::create_dir_all(parent) {
        cleanup.run();
        return CloneProgress::Failed {
            message: format!("Couldn't create {}: {error}", parent.display()),
        };
    }
    let spawned = git_command(parent)
        .args(["clone", "--progress", "--", url])
        .arg(destination)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            cleanup.run();
            return CloneProgress::Failed {
                message: format!("git failed to start: {error}"),
            };
        }
    };
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut tail = String::new();
    let mut pending = String::new();
    let mut buffer = [0u8; 4096];
    let read_loop = async {
        loop {
            let read = match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let text = String::from_utf8_lossy(&buffer[..read]);
            tail.push_str(&text);
            if tail.len() > STDERR_TAIL_BYTES {
                let cut = tail.len() - STDERR_TAIL_BYTES;
                let cut = (cut..tail.len())
                    .find(|ix| tail.is_char_boundary(*ix))
                    .unwrap_or(tail.len());
                tail.drain(..cut);
            }
            pending.push_str(&text);
            while let Some(ix) = pending.find(['\r', '\n']) {
                let line: String = pending.drain(..=ix).collect();
                if let Some((stage, percent)) = parse_progress(&line) {
                    progress.send_replace(CloneProgress::Cloning { stage, percent });
                }
            }
        }
        child.wait().await
    };
    enum Ended {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }
    let ended = tokio::select! {
        status = read_loop => Ended::Exited(status),
        () = cancel.cancelled() => Ended::Cancelled,
        () = tokio::time::sleep(CLONE_TIMEOUT) => Ended::TimedOut,
    };
    let status = match ended {
        Ended::Exited(Ok(status)) => status,
        Ended::Exited(Err(error)) => {
            cleanup.run();
            return CloneProgress::Failed {
                message: format!("git clone failed: {error}"),
            };
        }
        Ended::Cancelled | Ended::TimedOut => {
            let cancelled = cancel.is_cancelled();
            child.start_kill().ok();
            let _ = child.wait().await;
            cleanup.run();
            return if cancelled {
                CloneProgress::Cancelled
            } else {
                CloneProgress::Failed {
                    message: "The clone took too long".into(),
                }
            };
        }
    };
    if !status.success() {
        cleanup.run();
        return CloneProgress::Failed {
            message: clone_failure(&tail),
        };
    }
    if cancel.is_cancelled() {
        cleanup.run();
        return CloneProgress::Cancelled;
    }

    progress.send_replace(CloneProgress::Registering);
    let checkout_root = match std::fs::canonicalize(destination) {
        Ok(root) => root,
        Err(error) => {
            cleanup.run();
            return CloneProgress::Failed {
                message: format!("The clone vanished: {error}"),
            };
        }
    };
    let project_path = match subdirectory {
        None => checkout_root.clone(),
        Some(sub) => {
            let candidate = checkout_root.join(sub);
            match std::fs::canonicalize(&candidate) {
                Ok(path) if path.starts_with(&checkout_root) && path.is_dir() => path,
                _ => {
                    cleanup.run();
                    return CloneProgress::Failed {
                        message: format!(
                            "{} is not in the cloned repository's default branch",
                            sub.to_string_lossy().replace('\\', "/")
                        ),
                    };
                }
            }
        }
    };
    if let Err(error) = repos.add(&checkout_root.to_string_lossy()).await {
        tracing::debug!(%error, "remembering the cloned repository failed");
    }
    let path = project_path.to_string_lossy().into_owned();
    if let Err(error) = workspace.create_space(space_id, device_id, &path, name, true) {
        return CloneProgress::Failed {
            message: format!("The project was cloned but couldn't be added: {error}"),
        };
    }
    // An existing project for the same folder keeps its own id.
    let spaces = workspace.read_spaces().unwrap_or_default();
    let space_id = spaces
        .iter()
        .find(|space| space.id == space_id)
        .or_else(|| {
            spaces
                .iter()
                .find(|space| space.device_id == device_id && space.path == path)
        })
        .map(|space| space.id.clone())
        .unwrap_or_else(|| space_id.to_string());
    CloneProgress::Done {
        checkout_root: checkout_root.to_string_lossy().into_owned(),
        project_path: path,
        space_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_refuse_escapes_and_git() {
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "a/../../b",
            "a//b",
            "./a",
            ".git/config",
            "sub/.GIT/HEAD",
            "C:\\x",
            "a\\..\\b",
            "nul\0byte",
        ] {
            assert!(relative_project_path(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(
            relative_project_path("apps/web/.env.local").unwrap(),
            PathBuf::from("apps").join("web").join(".env.local")
        );
        assert_eq!(
            relative_project_path("a\\b").unwrap(),
            PathBuf::from("a").join("b")
        );
        assert!(relative_project_path(".gitignore").is_ok());
    }

    #[test]
    fn git_progress_lines_parse() {
        assert_eq!(
            parse_progress("Receiving objects:  45% (450/1000), 1.2 MiB | 3 MiB/s"),
            Some(("Receiving objects".into(), Some(45)))
        );
        assert_eq!(
            parse_progress("remote: Counting objects: 100% (12/12), done."),
            Some(("Counting objects".into(), Some(100)))
        );
        assert_eq!(parse_progress("Cloning into '/tmp/x'..."), None);
        assert_eq!(parse_progress("fatal: repository not found"), None);
        assert_eq!(
            clone_failure(
                "Cloning into 'x'...\nReceiving objects: 10% (1/10)\rfatal: repository 'y' not found\n"
            ),
            "repository 'y' not found"
        );
        assert_eq!(clone_failure(""), "git clone failed");
    }

    #[test]
    fn destination_must_be_absolute_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(clone_destination("relative/path").is_err());
        assert!(clone_destination("").is_err());
        assert!(clone_destination(&format!("{}/a/../b", dir.path().display())).is_err());
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(clone_destination(&empty.to_string_lossy()).unwrap(), empty);
        std::fs::write(empty.join("x"), "x").unwrap();
        assert!(clone_destination(&empty.to_string_lossy()).is_err());
        assert!(clone_destination(&empty.join("x").to_string_lossy()).is_err());
        let fresh = dir.path().join("new/deeper");
        assert_eq!(clone_destination(&fresh.to_string_lossy()).unwrap(), fresh);
        match Cleanup::for_destination(&fresh) {
            Cleanup::Remove(path) => assert_eq!(path, dir.path().join("new")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
