//! Chat import from other agent apps: T3 Code, plain Claude Code, plain Codex,
//! and the Cursor Agent CLI.
//!
//! Sources are only ever read (T3's live database through a private copy);
//! everything written goes through the paths native chats use:
//!
//!  - spaces via `WorkspaceHost::create_space`, reusing an existing space for
//!    the same folder on this device;
//!  - transcripts as session docs saved in the "born chat2" shape
//!    (`save_snapshot_with_cursor(id, bytes, 0, CHAT2_DOC_EPOCH)`, exactly as
//!    the local→synced import lands docs), entries built by folding the same
//!    `AgentEvent`s a live run emits (see [`model`]);
//!  - chat rows via `WorkspaceHost::import_chat_row`, stamped with the
//!    source's harness-native session id in `harness_session_id` /
//!    `harness_session_cwd` — the row `SessionsEngine::resume_for` reads on
//!    every dispatch, so the next message resumes the original CLI session;
//!  - attachments copied into this profile's uploads (`Uploads`).
//!
//! Idempotence is structural: a chat's id is derived from its source key
//! (`t3:<thread>`, `claude:<session>`, `codex:<thread>`), so a re-run finds
//! its own rows. A chat continued here is never touched again; one still
//! holding only imported entries gets the source's newer messages appended.
//! Cross-source duplicates are caught by native id: a Claude/Codex session a
//! T3 thread resumes is left to the T3 import, and any session some chat in
//! this workspace already resumes is skipped.

mod claude;
mod codex;
mod cursor;
mod model;
mod snapshot;
mod t3;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use zeron_doc::SessionDoc;
use zeron_proto::chat_import::{
    ChatImportPreview, ChatImportPreviewChat, ChatImportPreviewProject, ChatImportProgress,
    ChatImportRequest, ChatImportSource, ChatImportState, ChatImportSummary,
};
use zeron_proto::{Chat, ChatConfig, SandboxLevel, SidebarPinChange, Space};
use zeron_sync::DocsStore;

use crate::EngineError;
use crate::chat2_host::CHAT2_DOC_EPOCH;
use crate::doc_host::DocHost;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;
use model::{ENTRY_PREFIX, Role, SourceChat, SourceImage, SourceMessage};

/// Where each source lives on this device.
#[derive(Debug, Clone)]
pub struct ImportRoots {
    /// T3 Code's home (`~/.t3`, or `$T3CODE_HOME`).
    pub t3_home: PathBuf,
    /// Claude Code's projects dir (`~/.claude/projects`, or under `$CLAUDE_CONFIG_DIR`).
    pub claude_projects: PathBuf,
    /// Codex's home (`~/.codex`, or `$CODEX_HOME`).
    pub codex_home: PathBuf,
    /// The Cursor Agent CLI's chat stores (`~/.cursor/chats`, or under `$CURSOR_DATA_DIR`).
    pub cursor_chats: PathBuf,
}

impl ImportRoots {
    /// The default locations under `home`.
    pub fn under_home(home: &Path) -> Self {
        Self {
            t3_home: home.join(".t3"),
            claude_projects: home.join(".claude").join("projects"),
            codex_home: home.join(".codex"),
            cursor_chats: home.join(".cursor").join("chats"),
        }
    }

    /// The locations the apps themselves use, honoring their env overrides.
    pub fn detect() -> Self {
        let env_dir = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = env_dir("HOME")
            .or_else(|| env_dir("USERPROFILE"))
            .unwrap_or_default();
        let mut roots = Self::under_home(&home);
        if let Some(dir) = env_dir("T3CODE_HOME") {
            roots.t3_home = dir;
        }
        if let Some(dir) = env_dir("CLAUDE_CONFIG_DIR") {
            roots.claude_projects = dir.join("projects");
        }
        if let Some(dir) = env_dir("CODEX_HOME") {
            roots.codex_home = dir;
        }
        if let Some(dir) = env_dir("CURSOR_DATA_DIR") {
            roots.cursor_chats = dir.join("chats");
        }
        roots
    }
}

/// The chat id an imported conversation gets: stable per source key, so a
/// re-run (or another device's run) lands on the same row.
pub fn chat_id_for(source_key: &str) -> String {
    let digest = Sha256::digest(format!("godspeed-chat-import\0{source_key}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_custom_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// One scanned source: its chats plus what's needed to load transcripts.
struct Scan {
    found: bool,
    location: String,
    chats: Vec<SourceChat>,
    warnings: Vec<String>,
    reader: Reader,
}

enum Reader {
    None,
    T3(t3::T3Snapshot),
    Files(HashMap<String, PathBuf>),
}

impl Scan {
    fn transcript(
        &self,
        source: ChatImportSource,
        chat: &SourceChat,
    ) -> Result<Vec<SourceMessage>, EngineError> {
        match &self.reader {
            Reader::None => Ok(Vec::new()),
            Reader::T3(db) => db.transcript(chat.key.trim_start_matches("t3:")),
            Reader::Files(files) => {
                let path = files
                    .get(&chat.key)
                    .ok_or_else(|| EngineError::Other(format!("{} vanished", chat.key)))?;
                match source {
                    ChatImportSource::Claude => claude::transcript(path),
                    ChatImportSource::Cursor => cursor::transcript(path),
                    _ => codex::transcript(path),
                }
            }
        }
    }
}

/// Read a source's chat list (no transcripts).
fn scan(roots: &ImportRoots, source: ChatImportSource) -> Result<Scan, EngineError> {
    let mut warnings = Vec::new();
    match source {
        ChatImportSource::T3 => {
            let Some(db) = t3::database_path(&roots.t3_home) else {
                return Ok(Scan {
                    found: false,
                    location: roots.t3_home.display().to_string(),
                    chats: Vec::new(),
                    warnings,
                    reader: Reader::None,
                });
            };
            let snapshot =
                t3::T3Snapshot::open(&db, roots.t3_home.join("userdata").join("attachments"))?;
            let chats = snapshot.scan(&mut warnings)?;
            Ok(Scan {
                found: true,
                location: db.display().to_string(),
                chats,
                warnings,
                reader: Reader::T3(snapshot),
            })
        }
        ChatImportSource::Claude | ChatImportSource::Codex | ChatImportSource::Cursor => {
            let unarchived =
                |files: Vec<PathBuf>| files.into_iter().map(|path| (path, false)).collect();
            let (location, files): (PathBuf, Vec<(PathBuf, bool)>) = match source {
                ChatImportSource::Claude => (
                    roots.claude_projects.clone(),
                    unarchived(claude::session_files(&roots.claude_projects)),
                ),
                ChatImportSource::Cursor => (
                    roots.cursor_chats.clone(),
                    unarchived(cursor::store_files(&roots.cursor_chats)),
                ),
                _ => (
                    roots.codex_home.join("sessions"),
                    codex::rollout_files(&roots.codex_home),
                ),
            };
            let titles = if source == ChatImportSource::Codex {
                codex::thread_titles(&roots.codex_home)
            } else {
                HashMap::new()
            };
            let scan_one = move |path: &Path, archived: bool| match source {
                ChatImportSource::Claude => claude::scan_file(path),
                ChatImportSource::Cursor => cursor::scan_file(path),
                _ => codex::scan_file(path, archived, &titles),
            };
            let mut chats: Vec<SourceChat> = Vec::new();
            let mut paths = HashMap::new();
            for (path, scanned) in scan_bounded(files, SCAN_FILE_TIMEOUT, scan_one) {
                match scanned {
                    // A thread resumed under several rollouts keeps its newest.
                    Ok(Some(chat)) => {
                        if let Some(existing) = chats.iter_mut().find(|c| c.key == chat.key) {
                            if chat.updated_at > existing.updated_at {
                                paths.insert(chat.key.clone(), path);
                                *existing = chat;
                            }
                        } else {
                            paths.insert(chat.key.clone(), path);
                            chats.push(chat);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => warnings.push(format!("{}: {err}", path.display())),
                }
            }
            Ok(Scan {
                found: location.is_dir(),
                location: location.display().to_string(),
                chats,
                warnings,
                reader: Reader::Files(paths),
            })
        }
    }
}

/// How long one source file may take to read before the scan skips it.
const SCAN_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Scan `files` in order on a worker thread, giving each `timeout`. A read
/// that never returns (a FIFO, a network mount, a privacy prompt nobody can
/// answer) costs that one file: its thread is abandoned, a fresh worker takes
/// the rest, and the file reports an error.
fn scan_bounded<T, F>(
    files: Vec<(PathBuf, bool)>,
    timeout: std::time::Duration,
    scan: F,
) -> Vec<(PathBuf, Result<Option<T>, EngineError>)>
where
    T: Send + 'static,
    F: Fn(&Path, bool) -> Result<Option<T>, EngineError> + Send + Sync + 'static,
{
    type Job = (PathBuf, bool);
    type Done<T> = Result<Option<T>, EngineError>;
    let scan = Arc::new(scan);
    let spawn = || {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Done<T>>();
        let scan = scan.clone();
        let spawned = std::thread::Builder::new()
            .name("chat-import-scan".into())
            .spawn(move || {
                for (path, archived) in job_rx {
                    if done_tx.send(scan(&path, archived)).is_err() {
                        return;
                    }
                }
            });
        spawned.ok().map(|_| (job_tx, done_rx))
    };
    let mut worker = None;
    let mut out = Vec::with_capacity(files.len());
    for (path, archived) in files {
        if worker.is_none() {
            worker = spawn();
        }
        let Some((jobs, done)) = &worker else {
            out.push((path, Err(EngineError::Other("scan thread unavailable".into()))));
            continue;
        };
        let result = match jobs.send((path.clone(), archived)) {
            Ok(()) => done.recv_timeout(timeout),
            Err(_) => Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        match result {
            Ok(scanned) => out.push((path, scanned)),
            Err(err) => {
                // Dropping the channels lets a stuck worker exit whenever its
                // read finally returns.
                worker = None;
                let reason = match err {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        format!("no answer after {}s; skipped", timeout.as_secs())
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => "scan failed".into(),
                };
                out.push((path, Err(EngineError::Other(reason))));
            }
        }
    }
    out
}

/// macOS privacy-protected (TCC) locations. Touching them from a process
/// without consent blocks on a prompt, so the import never probes inside
/// them; the space's git stamp is filled in later by `SpacesSync`.
fn privacy_protected(path: &Path) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    if path.starts_with("/Volumes") {
        return true;
    }
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return false;
    };
    let home = Path::new(&home);
    [
        "Desktop",
        "Documents",
        "Downloads",
        "Pictures",
        "Movies",
        "Music",
        "Library/Mobile Documents",
    ]
    .iter()
    .any(|dir| path.starts_with(home.join(dir)))
}

/// Native session ids T3 threads resume (empty when T3 Code isn't here).
fn t3_owned_ids(roots: &ImportRoots, warnings: &mut Vec<String>) -> HashSet<String> {
    let Some(db) = t3::database_path(&roots.t3_home) else {
        return HashSet::new();
    };
    match t3::T3Snapshot::open(&db, PathBuf::new()).and_then(|s| s.owned_native_ids()) {
        Ok(ids) => ids.into_iter().collect(),
        Err(err) => {
            warnings.push(format!(
                "T3 Code sessions not checked for duplicates: {err}"
            ));
            HashSet::new()
        }
    }
}

/// What this workspace already holds, for classification.
#[derive(Default)]
struct Existing {
    chats: HashMap<String, Chat>,
    /// Native session id → the chat resuming it.
    sessions: HashMap<String, String>,
    spaces: Vec<Space>,
}

impl Existing {
    fn space_for(&self, device_id: &str, path: &str) -> Option<&Space> {
        self.spaces
            .iter()
            .find(|s| s.device_id == device_id && s.path.trim_end_matches('/') == path)
    }
}

/// Group classified chats into preview projects (most recent first).
fn preview_projects(
    chats: &[(SourceChat, ChatImportState)],
    has_space: impl Fn(&str) -> bool,
) -> Vec<ChatImportPreviewProject> {
    let mut grouped: BTreeMap<&str, Vec<&(SourceChat, ChatImportState)>> = BTreeMap::new();
    for item in chats {
        grouped
            .entry(item.0.project_path.as_str())
            .or_default()
            .push(item);
    }
    let mut projects: Vec<ChatImportPreviewProject> = grouped
        .into_iter()
        .map(|(path, items)| ChatImportPreviewProject {
            path: path.to_string(),
            title: items
                .iter()
                .find_map(|(c, _)| c.project_title.clone())
                .unwrap_or_else(|| model::basename(path)),
            has_space: has_space(path),
            chats: items
                .into_iter()
                .map(|(c, state)| ChatImportPreviewChat {
                    source_key: c.key.clone(),
                    title: c.title.clone(),
                    created_at: c.created_at,
                    updated_at: c.updated_at,
                    harness: c.harness,
                    resumable: c.resumable(),
                    state: *state,
                })
                .collect(),
        })
        .collect();
    projects.sort_by_key(|p| std::cmp::Reverse(p.date_range().map(|(_, last)| last)));
    projects
}

/// A source preview without any workspace to compare against (the CLI's
/// offline dry run): every chat is new unless a T3 thread owns it.
pub fn preview_source_only(roots: &ImportRoots, source: ChatImportSource) -> ChatImportPreview {
    let mut warnings = Vec::new();
    let owned = if source == ChatImportSource::T3 {
        HashSet::new()
    } else {
        t3_owned_ids(roots, &mut warnings)
    };
    match scan(roots, source) {
        Ok(scan) => {
            warnings.extend(scan.warnings.iter().cloned());
            let classified: Vec<_> = scan
                .chats
                .iter()
                .map(|c| {
                    let state = if c.native_ids.iter().any(|id| owned.contains(id)) {
                        ChatImportState::Elsewhere
                    } else {
                        ChatImportState::New
                    };
                    (c.clone(), state)
                })
                .collect();
            ChatImportPreview {
                source,
                found: scan.found,
                location: scan.location,
                projects: preview_projects(&classified, |_| false),
                warnings,
            }
        }
        Err(err) => failed_preview(roots, source, err, warnings),
    }
}

fn failed_preview(
    roots: &ImportRoots,
    source: ChatImportSource,
    err: EngineError,
    mut warnings: Vec<String>,
) -> ChatImportPreview {
    warnings.push(err.to_string());
    ChatImportPreview {
        source,
        found: true,
        location: match source {
            ChatImportSource::T3 => roots.t3_home.display().to_string(),
            ChatImportSource::Claude => roots.claude_projects.display().to_string(),
            ChatImportSource::Codex => roots.codex_home.display().to_string(),
            ChatImportSource::Cursor => roots.cursor_chats.display().to_string(),
        },
        projects: Vec::new(),
        warnings,
    }
}

/// The import service an engine runtime carries.
#[derive(Clone)]
pub struct ChatImporter {
    inner: Arc<Inner>,
}

struct Inner {
    device_id: String,
    store: Arc<DocsStore>,
    workspace: WorkspaceHost,
    doc_host: DocHost,
    uploads: Uploads,
    roots: ImportRoots,
}

impl ChatImporter {
    pub fn new(
        device_id: &str,
        store: Arc<DocsStore>,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        uploads: Uploads,
        roots: ImportRoots,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                device_id: device_id.to_string(),
                store,
                workspace,
                doc_host,
                uploads,
                roots,
            }),
        }
    }

    pub fn roots(&self) -> &ImportRoots {
        &self.inner.roots
    }

    /// The same importer reading sources from other locations.
    pub fn with_roots(&self, roots: ImportRoots) -> Self {
        let inner = &self.inner;
        Self::new(
            &inner.device_id,
            inner.store.clone(),
            inner.workspace.clone(),
            inner.doc_host.clone(),
            inner.uploads.clone(),
            roots,
        )
    }

    fn existing(&self) -> Result<Existing, EngineError> {
        let mut existing = Existing::default();
        for chat in self.inner.workspace.read_chats()? {
            if let Some(id) = chat
                .harness_session_id
                .as_deref()
                .filter(|id| !id.is_empty())
            {
                existing.sessions.insert(id.to_string(), chat.id.clone());
            }
            existing.chats.insert(chat.id.clone(), chat);
        }
        existing.spaces = self.inner.workspace.read_spaces()?;
        Ok(existing)
    }

    fn classify(
        &self,
        chat: &SourceChat,
        existing: &Existing,
        owned: &HashSet<String>,
    ) -> Result<ChatImportState, EngineError> {
        let chat_id = chat_id_for(&chat.key);
        if let Some(row) = existing.chats.get(&chat_id) {
            let newer = match (chat.updated_at, row.last_message_at) {
                (Some(source), Some(here)) => source > here + chrono::Duration::seconds(1),
                _ => false,
            };
            return Ok(if newer {
                ChatImportState::Update
            } else {
                ChatImportState::Imported
            });
        }
        // Imported once, then deleted here: the doc outlives the row. Don't
        // resurrect what the user removed.
        if self.inner.store.has_snapshot(&chat_id)? {
            return Ok(ChatImportState::Imported);
        }
        let elsewhere = chat.native_ids.iter().any(|id| {
            owned.contains(id)
                || existing
                    .sessions
                    .get(id)
                    .is_some_and(|other| other != &chat_id)
        });
        Ok(if elsewhere {
            ChatImportState::Elsewhere
        } else {
            ChatImportState::New
        })
    }

    fn classified(
        &self,
        source: ChatImportSource,
    ) -> Result<(Scan, Existing, Vec<(SourceChat, ChatImportState)>), EngineError> {
        let mut warnings = Vec::new();
        let owned = if source == ChatImportSource::T3 {
            HashSet::new()
        } else {
            t3_owned_ids(&self.inner.roots, &mut warnings)
        };
        let mut scan = scan(&self.inner.roots, source)?;
        scan.warnings.extend(warnings);
        let existing = self.existing()?;
        let mut classified = Vec::with_capacity(scan.chats.len());
        for chat in &scan.chats {
            classified.push((chat.clone(), self.classify(chat, &existing, &owned)?));
        }
        Ok((scan, existing, classified))
    }

    /// Scan `source` and compare it against this workspace. Blocking.
    pub fn preview(&self, source: ChatImportSource) -> ChatImportPreview {
        match self.classified(source) {
            Ok((scan, existing, classified)) => ChatImportPreview {
                source,
                found: scan.found,
                location: scan.location.clone(),
                projects: preview_projects(&classified, |path| {
                    existing.space_for(&self.inner.device_id, path).is_some()
                }),
                warnings: scan.warnings,
            },
            Err(err) => failed_preview(&self.inner.roots, source, err, Vec::new()),
        }
    }

    /// Import the request's projects, emitting progress (the last item is
    /// always the summary, which is also returned). Blocking.
    pub fn run(
        &self,
        request: &ChatImportRequest,
        mut emit: impl FnMut(ChatImportProgress),
    ) -> ChatImportSummary {
        let mut summary = ChatImportSummary::default();
        if let Err(err) = self.run_inner(request, &mut emit, &mut summary) {
            summary.errors.push(err.to_string());
        }
        // New chats were saved straight to the store, past any doc handle.
        if let Some(index) = self.inner.doc_host.message_index() {
            index.request_reconcile();
        }
        emit(ChatImportProgress::Summary {
            summary: summary.clone(),
        });
        summary
    }

    fn run_inner(
        &self,
        request: &ChatImportRequest,
        emit: &mut impl FnMut(ChatImportProgress),
        summary: &mut ChatImportSummary,
    ) -> Result<(), EngineError> {
        let source = request.source;
        let (scan, mut existing, classified) = self.classified(source)?;
        let wanted: Option<HashSet<String>> = request.projects.as_ref().map(|paths| {
            paths
                .iter()
                .map(|p| p.trim_end_matches('/').to_string())
                .collect()
        });
        let selected: Vec<&(SourceChat, ChatImportState)> = classified
            .iter()
            .filter(|(chat, _)| {
                wanted
                    .as_ref()
                    .is_none_or(|w| w.contains(&chat.project_path))
            })
            .collect();
        let work: Vec<&(SourceChat, ChatImportState)> = selected
            .iter()
            .copied()
            .filter(|(_, state)| matches!(state, ChatImportState::New | ChatImportState::Update))
            .collect();
        summary.skipped = selected.len() - work.len();
        let total = work.len();
        emit(ChatImportProgress::Start { total });
        // Oldest first, so the sidebar's recency order matches the source's.
        let mut work = work;
        work.sort_by_key(|(chat, _)| chat.created_at);
        for (index, (chat, state)) in work.into_iter().enumerate() {
            emit(ChatImportProgress::Chat {
                index,
                total,
                title: chat.title.clone(),
            });
            let result = match state {
                ChatImportState::New => {
                    self.import_new(source, &scan, chat, &mut existing, summary)
                }
                _ => self.import_update(source, &scan, chat, summary),
            };
            if let Err(err) = result {
                summary.errors.push(format!("{}: {err}", chat.title));
            }
        }
        Ok(())
    }

    /// The space for a chat's project folder on this device, created when
    /// missing.
    fn ensure_space(
        &self,
        chat: &SourceChat,
        existing: &mut Existing,
        summary: &mut ChatImportSummary,
    ) -> Result<String, EngineError> {
        let device_id = &self.inner.device_id;
        if let Some(space) = existing.space_for(device_id, &chat.project_path) {
            return Ok(space.id.clone());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let title = chat.project_title.clone();
        let name = title.filter(|t| t != &model::basename(&chat.project_path));
        let project = Path::new(&chat.project_path);
        let git = !privacy_protected(project) && project.join(".git").exists();
        self.inner
            .workspace
            .create_space(&id, device_id, &chat.project_path, name.clone(), git)?;
        // `create_space` is a no-op for a live duplicate; read back the winner.
        let spaces = self.inner.workspace.read_spaces()?;
        let space = spaces
            .iter()
            .find(|s| {
                s.device_id == *device_id && s.path.trim_end_matches('/') == chat.project_path
            })
            .cloned()
            .ok_or_else(|| {
                EngineError::Other(format!("space for {} not created", chat.project_path))
            })?;
        if space.id == id {
            summary.spaces_created += 1;
        }
        existing.spaces = spaces;
        Ok(space.id)
    }

    /// Doc entries for a transcript, attachments copied into uploads.
    fn entries(
        &self,
        chat: &SourceChat,
        messages: &[SourceMessage],
        summary: &mut ChatImportSummary,
    ) -> Vec<zeron_doc::SessionMessageEntry> {
        let mut entries = Vec::new();
        for message in messages {
            let text = if message.role == Role::User && !message.images.is_empty() {
                let paths = self.copy_images(chat, message, summary);
                crate::doc_host::queued_message_prompt(&message.text, &paths)
            } else {
                message.text.clone()
            };
            entries.extend(model::message_entries(
                message,
                &text,
                &self.inner.device_id,
            ));
        }
        entries
    }

    fn copy_images(
        &self,
        chat: &SourceChat,
        message: &SourceMessage,
        summary: &mut ChatImportSummary,
    ) -> Vec<String> {
        let mut paths = Vec::new();
        for (index, image) in message.images.iter().enumerate() {
            let key = format!("{}:{}:{index}", chat.key, message.id);
            let copied = match image {
                SourceImage::File { path, root } => self
                    .inner
                    .uploads
                    .import_generated_image(path, root, &key)
                    .map(|image| image.path),
                SourceImage::Inline { base64, media_type } => {
                    let upload_id = format!("{:x}", Sha256::digest(key.as_bytes()));
                    let upload_id = &upload_id[..32];
                    let ext = match media_type.as_str() {
                        "image/jpeg" => "jpg",
                        "image/webp" => "webp",
                        "image/gif" => "gif",
                        _ => "png",
                    };
                    self.inner
                        .uploads
                        .append(upload_id, base64, Some(0))
                        .and_then(|()| {
                            self.inner
                                .uploads
                                .commit(upload_id, &format!("image-{}.{ext}", index + 1))
                        })
                }
            };
            match copied {
                Ok(path) => {
                    summary.attachments += 1;
                    paths.push(path);
                }
                // A missing or unreadable image costs only the image.
                Err(err) => {
                    tracing::warn!(chat = %chat.key, error = %err, "import: image not copied")
                }
            }
        }
        paths
    }

    fn import_new(
        &self,
        source: ChatImportSource,
        scan: &Scan,
        chat: &SourceChat,
        existing: &mut Existing,
        summary: &mut ChatImportSummary,
    ) -> Result<(), EngineError> {
        let messages = scan.transcript(source, chat)?;
        if messages.is_empty() {
            summary.skipped += 1;
            return Ok(());
        }
        let chat_id = chat_id_for(&chat.key);
        let space_id = self.ensure_space(chat, existing, summary)?;
        let entries = self.entries(chat, &messages, summary);
        // Doc first, row last: once the row appears the chat is clickable,
        // and by then its transcript is already durable.
        let doc = SessionDoc::init(&chat_id)?;
        for entry in &entries {
            doc.push_message(entry)?;
        }
        let bytes = doc.export_snapshot()?;
        self.inner
            .store
            .save_snapshot_with_cursor(&chat_id, &bytes, 0, CHAT2_DOC_EPOCH)?;
        let row = self.chat_row(chat, &chat_id, &space_id, &messages);
        self.inner.workspace.import_chat_row(&row)?;
        if let Some(id) = &chat.resume {
            existing.sessions.insert(id.clone(), chat_id.clone());
        }
        existing.chats.insert(chat_id.clone(), row);
        if chat.pinned
            && let Err(err) = self
                .inner
                .workspace
                .change_sidebar_pin(&SidebarPinChange::Pin {
                    session_id: chat_id,
                    after: None,
                    before: None,
                })
        {
            tracing::warn!(chat = %chat.key, error = %err, "import: pin not applied");
        }
        summary.imported += 1;
        summary.messages += messages.len();
        Ok(())
    }

    /// Append the source's newer messages to a chat that holds only imported
    /// entries. A chat continued here is left alone: its transcript and the
    /// source have diverged.
    fn import_update(
        &self,
        source: ChatImportSource,
        scan: &Scan,
        chat: &SourceChat,
        summary: &mut ChatImportSummary,
    ) -> Result<(), EngineError> {
        let chat_id = chat_id_for(&chat.key);
        let handle = self.inner.doc_host.open_local(&chat_id)?;
        let have = handle.doc().read_entries()?;
        if have.iter().any(|e| !e.id.starts_with(ENTRY_PREFIX)) {
            summary.skipped += 1;
            return Ok(());
        }
        let messages = scan.transcript(source, chat)?;
        let ids: HashSet<&str> = have.iter().map(|e| e.id.as_str()).collect();
        let fresh: Vec<SourceMessage> = messages
            .iter()
            .filter(|m| !ids.contains(format!("{ENTRY_PREFIX}{}", m.id).as_str()))
            .cloned()
            .collect();
        for entry in self.entries(chat, &fresh, summary) {
            handle.doc().push_message(&entry)?;
        }
        self.inner.doc_host.persist_fork(&handle)?;
        if let Some(mut row) = self.inner.workspace.chat(&chat_id)? {
            row.last_message_preview = model::last_preview(&messages);
            row.last_message_at = chat.updated_at.or(row.last_message_at);
            row.last_seen_at = row.last_message_at;
            if let Some(resume) = &chat.resume {
                row.harness_session_id = Some(resume.clone());
                row.harness_session_cwd = Some(chat.cwd.clone());
            }
            self.inner.workspace.import_chat_row(&row)?;
        }
        summary.updated += 1;
        summary.messages += fresh.len();
        Ok(())
    }

    fn chat_row(
        &self,
        chat: &SourceChat,
        chat_id: &str,
        space_id: &str,
        messages: &[SourceMessage],
    ) -> Chat {
        let first = messages.first().map(|m| m.at);
        let last = messages.last().map(|m| m.at);
        let at = |ms: Option<i64>| {
            ms.filter(|ms| *ms > 0)
                .and_then(DateTime::<Utc>::from_timestamp_millis)
        };
        let created_at = chat.created_at.or(at(first)).unwrap_or_else(Utc::now);
        let updated_at = chat.updated_at.or(at(last)).unwrap_or(created_at);
        Chat {
            id: chat_id.to_string(),
            device_id: self.inner.device_id.clone(),
            title: Some(chat.title.clone()),
            archived: chat.archived,
            cwd: Some(chat.cwd.clone()),
            branch: chat.branch.clone(),
            checkout_id: None,
            source_context: None,
            // A chat whose source harness this app lacks keeps no config and
            // no resume id: it runs on the default harness, freshly.
            config: chat.harness.map(|harness| ChatConfig {
                harness,
                model: chat.model.clone(),
                reasoning: chat.reasoning,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
                access_mode: chat.access_mode,
            }),
            last_message_preview: model::last_preview(messages),
            last_message_at: Some(updated_at),
            created_at,
            harness_session_id: chat.resume.clone(),
            harness_session_cwd: chat.resume.as_ref().map(|_| chat.cwd.clone()),
            space_id: Some(space_id.to_string()),
            // Seen as of import: history isn't news.
            last_seen_at: Some(updated_at),
            room_gen: Some(CHAT2_DOC_EPOCH),
            parent_chat_id: None,
            limit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_are_stable_per_source_key() {
        assert_eq!(chat_id_for("t3:a"), chat_id_for("t3:a"));
        assert_ne!(chat_id_for("t3:a"), chat_id_for("claude:a"));
        assert_eq!(chat_id_for("t3:a").len(), 36);
    }

    #[test]
    fn a_file_that_never_answers_costs_only_itself() {
        let files: Vec<(PathBuf, bool)> = ["a", "hang", "b", "hang", "c"]
            .iter()
            .map(|name| (PathBuf::from(name), false))
            .collect();
        let started = std::time::Instant::now();
        let scanned = scan_bounded(
            files,
            std::time::Duration::from_millis(100),
            |path: &Path, _| {
                if path == Path::new("hang") {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                }
                Ok(Some(path.display().to_string()))
            },
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let names: Vec<_> = scanned
            .iter()
            .map(|(path, result)| match result {
                Ok(Some(name)) => name.clone(),
                Ok(None) => format!("{}: none", path.display()),
                Err(err) => format!("{}: {err}", path.display()),
            })
            .collect();
        assert_eq!(names[0], "a");
        assert!(names[1].starts_with("hang: ") && names[1].contains("skipped"));
        assert_eq!(names[2], "b");
        assert!(names[3].contains("skipped"));
        assert_eq!(names[4], "c");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn privacy_protected_folders_are_never_probed() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert!(privacy_protected(&home.join("Documents/app")));
        assert!(privacy_protected(&home.join("Desktop")));
        assert!(privacy_protected(Path::new("/Volumes/USB/repo")));
        assert!(!privacy_protected(&home.join("Developer/app")));
        assert!(!privacy_protected(&home.join("Documentsx/app")));
    }
}
