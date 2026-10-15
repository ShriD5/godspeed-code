//! Message index: local full-text search over every chat transcript in this
//! engine's store (`SearchMessages`), imported history included.
//!
//! A pure-Rust inverted index ([`index`]) held in memory and persisted to
//! `message-index.bin` beside the docs store ([`persist`]). One background
//! thread owns all writes:
//!
//! - **startup**: load the file (a missing, old or corrupt one starts empty),
//!   then reconcile against the store's snapshot versions — only chats whose
//!   `(saved_at, length)` differ from the indexed marker are re-read;
//! - **live**: every change of an open chat doc notes the chat
//!   ([`MessageIndex::note_changed`]); after [`LIVE_DEBOUNCE`] of quiet (at
//!   most [`LIVE_MAX_WAIT`] while a reply streams) the doc's transcript is
//!   diffed into the index entry by entry, so a streamed reply re-indexes
//!   one message, never the chat;
//! - **catch-all**: imports and other writers that bypass doc handles call
//!   [`MessageIndex::request_reconcile`]; a periodic pass covers the rest;
//! - **persistence**: rewritten after [`PERSIST_QUIET`] without changes, at
//!   most every [`PERSIST_EVERY`], and on [`MessageIndex::flush`]. Markers
//!   are only ever written with the content they describe, so a crash costs
//!   a re-read of recently changed chats, never a stale index.
//!
//! Searches read under a shared lock and never wait for the worker.

mod index;
mod persist;
mod tokens;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};

use zeron_doc::{MessagePart, MessageRole, SessionDoc, SessionMessageEntry};
use zeron_proto::{MessageSearchHit, MessageSearchRole, ToolCall};
use zeron_sync::DocsStore;

use index::{Index, Marker, MessageText, SearchOptions};

/// File name of the persisted index, beside the profile's docs store.
pub const INDEX_FILE: &str = "message-index.bin";
/// Quiet time after a doc change before its transcript is re-indexed.
pub const LIVE_DEBOUNCE: Duration = Duration::from_millis(300);
/// A doc changing continuously (a streaming reply) re-indexes this often.
pub const LIVE_MAX_WAIT: Duration = Duration::from_millis(1500);
/// The index file is rewritten once changes pause this long...
const PERSIST_QUIET: Duration = Duration::from_secs(5);
/// ...but never more often than this.
const PERSIST_EVERY: Duration = Duration::from_secs(60);
/// Periodic reconcile for snapshots written without a doc handle.
const RECONCILE_EVERY: Duration = Duration::from_secs(5 * 60);

/// Which store documents are chats worth indexing.
pub type ChatFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Index counters (logs, tests, the bench).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIndexStats {
    pub chats: usize,
    pub messages: usize,
    pub terms: usize,
}

#[derive(Clone)]
pub struct MessageIndex {
    inner: Arc<Inner>,
}

struct Inner {
    index: RwLock<Index>,
    /// Initial load and reconcile finished.
    ready: AtomicBool,
    store: Arc<DocsStore>,
    path: Option<PathBuf>,
    filter: Option<ChatFilter>,
    work: Mutex<Work>,
    wake: Condvar,
    /// Persisted generation equals the in-memory one.
    idle: Condvar,
    #[cfg(test)]
    snapshot_reads: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct Work {
    /// Chats changed through an open doc: the doc, first and last change.
    live: HashMap<String, Pending>,
    /// Docs noted since the worker last saw them; reconcile leaves their
    /// chats to the live path while the doc is alive.
    open: HashMap<String, Weak<SessionDoc>>,
    removed: Vec<String>,
    reconcile: bool,
    flush: bool,
    shutdown: bool,
    /// Bumped per index change; `persisted` trails it.
    generation: u64,
    persisted: u64,
    last_change: Option<Instant>,
    /// Outstanding work the worker has taken but not finished.
    busy: bool,
}

struct Pending {
    doc: Weak<SessionDoc>,
    first: Instant,
    last: Instant,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MessageIndex {
    /// Open the index for `store`, persisted at `dir/message-index.bin`
    /// (`None` keeps it in memory only), and start its worker. `filter`
    /// names the store documents that are chats (the workspace's rows);
    /// `None` indexes every document.
    pub fn open(store: Arc<DocsStore>, dir: Option<&Path>, filter: Option<ChatFilter>) -> Self {
        let inner = Arc::new(Inner {
            index: RwLock::new(Index::default()),
            ready: AtomicBool::new(false),
            store,
            path: dir.map(|dir| dir.join(INDEX_FILE)),
            filter,
            work: Mutex::new(Work::default()),
            wake: Condvar::new(),
            idle: Condvar::new(),
            #[cfg(test)]
            snapshot_reads: Default::default(),
        });
        let weak = Arc::downgrade(&inner);
        let spawned = std::thread::Builder::new()
            .name("message-index".into())
            .spawn(move || worker(weak));
        if let Err(error) = spawned {
            tracing::error!(%error, "message index worker failed to start");
        }
        Self { inner }
    }

    /// Whether the initial load + reconcile finished.
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> MessageIndexStats {
        let index = self.inner.read();
        MessageIndexStats {
            chats: index.chat_count(),
            messages: index.message_count(),
            terms: index.term_count(),
        }
    }

    /// An open chat doc changed (cheap; called on every doc commit).
    pub fn note_changed(&self, chat_id: &str, doc: &Arc<SessionDoc>) {
        let now = Instant::now();
        let mut work = lock(&self.inner.work);
        match work.live.get_mut(chat_id) {
            Some(pending) => pending.last = now,
            None => {
                work.live.insert(
                    chat_id.to_owned(),
                    Pending {
                        doc: Arc::downgrade(doc),
                        first: now,
                        last: now,
                    },
                );
                self.inner.wake.notify_one();
            }
        }
    }

    /// A chat was deleted.
    pub fn remove_chat(&self, chat_id: &str) {
        let mut work = lock(&self.inner.work);
        work.live.remove(chat_id);
        work.open.remove(chat_id);
        work.removed.push(chat_id.to_owned());
        self.inner.wake.notify_one();
    }

    /// Snapshots were written without a doc handle (imports): re-check every
    /// chat's stored version soon.
    pub fn request_reconcile(&self) {
        lock(&self.inner.work).reconcile = true;
        self.inner.wake.notify_one();
    }

    /// Ranked hits for `query` (see `index::Index::search`).
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        per_chat: Option<usize>,
        chat_filter: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<MessageSearchHit> {
        let options = SearchOptions {
            limit,
            per_chat,
            chat_filter,
            now_ms: crate::now_ms(),
        };
        self.inner.read().search(query, &options)
    }

    /// Wait until every noted change is indexed and the file is written (if
    /// changed). Shutdown and tests; bounded by `timeout`.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut work = lock(&self.inner.work);
        work.flush = true;
        self.inner.wake.notify_one();
        loop {
            let settled = self.inner.ready.load(Ordering::Acquire)
                && work.live.is_empty()
                && work.removed.is_empty()
                && !work.reconcile
                && !work.busy
                && (self.inner.path.is_none() || work.persisted == work.generation);
            if settled {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            work = self
                .inner
                .idle
                .wait_timeout(work, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Stop the worker (it persists pending changes first).
    pub fn shutdown(&self) {
        lock(&self.inner.work).shutdown = true;
        self.inner.wake.notify_one();
    }
}

impl Inner {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Index> {
        self.index.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn accepts(&self, chat_id: &str) -> bool {
        self.filter.as_ref().is_none_or(|accept| accept(chat_id))
    }

    fn changed(&self) {
        let mut work = lock(&self.work);
        work.generation += 1;
        work.last_change = Some(Instant::now());
    }

    fn load(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let started = Instant::now();
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(%error, "message index unreadable; rebuilding");
                return;
            }
        };
        match persist::decode(&bytes) {
            Ok(index) => {
                tracing::info!(
                    chats = index.chat_count(),
                    messages = index.message_count(),
                    bytes = bytes.len(),
                    ms = started.elapsed().as_millis() as u64,
                    "message index loaded"
                );
                *self.write() = index;
            }
            Err(error) => {
                tracing::warn!(%error, "message index rejected; rebuilding");
            }
        }
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let generation = lock(&self.work).generation;
        let bytes = persist::encode(&self.read());
        let tmp = path.with_extension("bin.tmp");
        let written = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, path));
        match written {
            Ok(()) => lock(&self.work).persisted = generation,
            Err(error) => {
                tracing::warn!(%error, "message index save failed");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// Index one chat's transcript from an open doc.
    fn index_live(&self, chat_id: &str, doc: &SessionDoc) {
        if !self.accepts(chat_id) {
            return;
        }
        match doc.read_entries() {
            Ok(entries) => {
                let messages = message_texts(entries);
                if self.write().update_chat(chat_id, &messages) {
                    self.changed();
                }
            }
            Err(error) => tracing::debug!(chat = %chat_id, %error, "message index: transcript read failed"),
        }
    }

    /// Bring every stored chat up to its snapshot version.
    fn reconcile(&self) {
        let started = Instant::now();
        let versions = match self.store.snapshot_versions() {
            Ok(versions) => versions,
            Err(error) => {
                tracing::warn!(%error, "message index: snapshot listing failed");
                return;
            }
        };
        let open: HashMap<String, Weak<SessionDoc>> = lock(&self.work).open.clone();
        let mut keep: HashSet<String> = HashSet::new();
        let mut read = 0usize;
        for (chat_id, saved_at, len) in versions {
            if !self.accepts(&chat_id) {
                continue;
            }
            keep.insert(chat_id.clone());
            let marker: Marker = (saved_at, len);
            if self.read().marker(&chat_id) == Some(marker) {
                continue;
            }
            if let Some(doc) = open.get(&chat_id).and_then(Weak::upgrade) {
                // The live path owns an open doc; it is at least as new as
                // its snapshot.
                self.index_live(&chat_id, &doc);
                self.mark(&chat_id, marker);
                continue;
            }
            match self.read_snapshot(&chat_id) {
                Ok(messages) => {
                    // The marker moved even when the content did not.
                    let mut index = self.write();
                    index.update_chat(&chat_id, &messages);
                    index.set_marker(&chat_id, Some(marker));
                    drop(index);
                    self.changed();
                    read += 1;
                }
                Err(error) => {
                    tracing::debug!(chat = %chat_id, %error, "message index: snapshot unreadable");
                }
            }
        }
        // Chats whose snapshot or registry row is gone, unless a live doc
        // (not yet saved) still holds them.
        let stale: Vec<String> = self
            .read()
            .chat_ids()
            .into_iter()
            .filter(|id| !keep.contains(id))
            .filter(|id| {
                open.get(id).and_then(Weak::upgrade).is_none() || !self.accepts(id)
            })
            .collect();
        if !stale.is_empty() {
            let mut index = self.write();
            for id in &stale {
                index.remove_chat(id);
            }
            drop(index);
            self.changed();
        }
        // Forget docs that closed.
        lock(&self.work)
            .open
            .retain(|_, doc| doc.strong_count() > 0);
        if read > 0 || !stale.is_empty() {
            tracing::info!(
                read,
                removed = stale.len(),
                ms = started.elapsed().as_millis() as u64,
                "message index reconciled"
            );
        }
    }

    fn mark(&self, chat_id: &str, marker: Marker) {
        let mut index = self.write();
        if index.marker(chat_id) != Some(marker) {
            index.set_marker(chat_id, Some(marker));
            drop(index);
            self.changed();
        }
    }

    fn read_snapshot(&self, chat_id: &str) -> Result<Vec<MessageText>, String> {
        #[cfg(test)]
        self.snapshot_reads.fetch_add(1, Ordering::Relaxed);
        let bytes = self
            .store
            .load_snapshot(chat_id)
            .map_err(|e| e.to_string())?
            .ok_or("snapshot vanished")?;
        let raw = loro::LoroDoc::new();
        raw.import(&bytes).map_err(|e| e.to_string())?;
        let entries = SessionDoc::from_doc(raw)
            .read_entries()
            .map_err(|e| e.to_string())?;
        Ok(message_texts(entries))
    }
}

fn worker(weak: Weak<Inner>) {
    {
        let Some(inner) = weak.upgrade() else { return };
        inner.load();
        inner.reconcile();
        inner.ready.store(true, Ordering::Release);
        if lock(&inner.work).generation > 0 {
            inner.persist();
        }
        inner.idle.notify_all();
    }
    let mut last_reconcile = Instant::now();
    let mut last_persist = Instant::now();
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let mut work = lock(&inner.work);
        let now = Instant::now();
        let due_live: Vec<String> = work
            .live
            .iter()
            .filter(|(_, p)| {
                now.duration_since(p.last) >= LIVE_DEBOUNCE
                    || now.duration_since(p.first) >= LIVE_MAX_WAIT
                    || work.flush
                    || work.shutdown
            })
            .map(|(id, _)| id.clone())
            .collect();
        let dirty = work.generation != work.persisted && inner.path.is_some();
        let persist_due = dirty
            && (work.flush
                || work.shutdown
                || (work
                    .last_change
                    .is_some_and(|at| now.duration_since(at) >= PERSIST_QUIET)
                    && now.duration_since(last_persist) >= PERSIST_EVERY));
        let reconcile_due = work.reconcile || now.duration_since(last_reconcile) >= RECONCILE_EVERY;
        let removed = std::mem::take(&mut work.removed);
        if due_live.is_empty() && removed.is_empty() && !reconcile_due && !persist_due {
            if work.shutdown {
                return;
            }
            if work.flush {
                work.flush = false;
                inner.idle.notify_all();
            }
            // Sleep until the next deadline (or a wake).
            let mut next = RECONCILE_EVERY.saturating_sub(now.duration_since(last_reconcile));
            for pending in work.live.values() {
                let at = (pending.last + LIVE_DEBOUNCE).min(pending.first + LIVE_MAX_WAIT);
                next = next.min(at.saturating_duration_since(now));
            }
            if dirty && let Some(at) = work.last_change {
                let quiet = (at + PERSIST_QUIET).saturating_duration_since(now);
                let spaced = (last_persist + PERSIST_EVERY).saturating_duration_since(now);
                next = next.min(quiet.max(spaced));
            }
            drop(inner.wake.wait_timeout(work, next.max(Duration::from_millis(5))));
            continue;
        }
        let docs: Vec<(String, Weak<SessionDoc>)> = due_live
            .iter()
            .filter_map(|id| {
                let pending = work.live.remove(id)?;
                Some((id.clone(), pending.doc))
            })
            .collect();
        for (id, doc) in &docs {
            work.open.insert(id.clone(), doc.clone());
        }
        work.reconcile = false;
        work.busy = true;
        drop(work);

        for (id, doc) in docs {
            if let Some(doc) = doc.upgrade() {
                inner.index_live(&id, &doc);
            }
        }
        if !removed.is_empty() {
            let mut index = inner.write();
            let changed = removed.iter().fold(false, |any, id| index.remove_chat(id) | any);
            drop(index);
            if changed {
                inner.changed();
            }
        }
        if reconcile_due {
            inner.reconcile();
            last_reconcile = Instant::now();
        }
        if persist_due {
            inner.persist();
            last_persist = Instant::now();
        }
        lock(&inner.work).busy = false;
        inner.idle.notify_all();
    }
}

/// The searchable text of a joined transcript: user and assistant prose,
/// plus tool titles/commands; reasoning, system rows and tool output are
/// not indexed.
fn message_texts(entries: Vec<SessionMessageEntry>) -> Vec<MessageText> {
    zeron_doc::join_continuation_entries(entries)
        .into_iter()
        .filter_map(|entry| {
            let role = match entry.role {
                MessageRole::User => MessageSearchRole::User,
                MessageRole::Assistant => MessageSearchRole::Assistant,
                MessageRole::System => return None,
            };
            let mut text = String::new();
            let mut tools = String::new();
            for part in &entry.parts {
                match part {
                    MessagePart::Text { text: body, .. } => {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(body);
                    }
                    MessagePart::Tool { call, .. } => {
                        let line = tool_text(call);
                        if !line.is_empty() {
                            if !tools.is_empty() {
                                tools.push('\n');
                            }
                            tools.push_str(&line);
                        }
                    }
                    _ => {}
                }
            }
            (!text.trim().is_empty() || !tools.is_empty()).then(|| MessageText {
                id: entry.id,
                role,
                created_at: entry.created_at,
                text,
                tools,
            })
        })
        .collect()
}

/// A tool call's searchable title: its command, path, pattern or name.
fn tool_text(call: &ToolCall) -> String {
    const MAX: usize = 512;
    let text = match call {
        ToolCall::Exec { command } => command.clone(),
        ToolCall::ReadFile { path }
        | ToolCall::WriteFile { path, .. }
        | ToolCall::EditFile { path, .. } => path.clone(),
        ToolCall::ApplyPatch { path } => path.clone().unwrap_or_default(),
        ToolCall::Search { pattern, path } => match path {
            Some(path) => format!("{pattern} {path}"),
            None => pattern.clone(),
        },
        ToolCall::Glob { pattern } => pattern.clone(),
        ToolCall::WebFetch { url, .. } => url.clone(),
        ToolCall::WebSearch { query } => query.clone(),
        ToolCall::Todo { items } => items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        ToolCall::Mcp { server, tool, .. } => format!("{server} {tool}"),
        ToolCall::Unknown { name, .. } => name.clone(),
    };
    if text.len() <= MAX {
        return text;
    }
    text[..text.floor_char_boundary(MAX)].to_owned()
}

#[cfg(test)]
mod tests;
