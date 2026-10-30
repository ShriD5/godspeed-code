//! Message index: local full-text search over every chat transcript in this
//! engine's store (`SearchMessages`), imported history included.
//!
//! The index lives on disk in SQLite ([`db`]: an FTS5 table plus the text
//! snippets are cut from), beside the docs store as [`INDEX_FILE`], so its
//! postings and message text are not held in memory. One background thread
//! owns all writes:
//!
//! - **startup**: open the file, then reconcile against the store's snapshot
//!   versions: only chats whose `(saved_at, length)` differ from the marker
//!   recorded with their rows are re-read, a batch of chats per transaction;
//! - **live**: every change of an open chat doc notes the chat
//!   ([`MessageIndex::note_changed`]). After [`LIVE_DEBOUNCE`] of quiet the
//!   worker looks at the doc's last entry. A finished turn is diffed into the
//!   index entry by entry (one full transcript read per turn). While a reply
//!   streams, only the turn in flight is read, into a small in-memory overlay
//!   that searches see at most [`LIVE_MAX_WAIT`] behind; nothing is written
//!   until the turn ends;
//! - **catch-all**: imports and other writers that bypass doc handles call
//!   [`MessageIndex::request_reconcile`]; a periodic pass covers the rest;
//! - **migration**: the first launch after the in-memory index was replaced
//!   builds the file from the store in the background. Until it finishes,
//!   searches answer from the old `message-index.bin` (loaded once), which is
//!   deleted when the build completes.
//!
//! Searches read through their own connection and never wait for the worker.

mod db;
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

use db::Db;
use index::{Doc, Index, Marker, MessageText, SearchOptions};

/// File name of the index database, beside the profile's docs store.
pub const INDEX_FILE: &str = "message-index.sqlite";
/// The in-memory index's file from earlier versions: read once while the
/// database is first built, then deleted.
pub const LEGACY_INDEX_FILE: &str = "message-index.bin";
/// Quiet time after a doc change before the worker looks at it.
pub const LIVE_DEBOUNCE: Duration = Duration::from_millis(300);
/// A reply streaming continuously refreshes its overlay this often.
pub const LIVE_MAX_WAIT: Duration = Duration::from_millis(1500);
/// Periodic reconcile for snapshots written without a doc handle.
const RECONCILE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Chats re-read per transaction while reconciling.
const RECONCILE_BATCH: usize = 32;
/// Entries read from the end of a doc for the turn in flight.
const STREAMING_TAIL_ENTRIES: usize = 256;

/// Which store documents are chats worth indexing.
pub type ChatFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Index counters (logs, tests, the bench).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    /// The worker's connection; also serves searches for an in-memory index.
    db: Mutex<Option<Db>>,
    /// Searches' own read connection (on-disk index only).
    reader: Mutex<Option<Db>>,
    /// The old in-memory index, answering searches while a first build runs.
    legacy: RwLock<Option<Index>>,
    /// Turns in flight, by chat: searched alongside the database and
    /// shadowing the stored copies of the same messages.
    overlay: RwLock<HashMap<String, Vec<Doc>>>,
    /// Initial build / reconcile finished.
    ready: AtomicBool,
    store: Arc<DocsStore>,
    dir: Option<PathBuf>,
    filter: Option<ChatFilter>,
    work: Mutex<Work>,
    wake: Condvar,
    /// Outstanding work drained.
    idle: Condvar,
    #[cfg(test)]
    snapshot_reads: std::sync::atomic::AtomicUsize,
    /// Whole-transcript reads of open docs.
    #[cfg(test)]
    full_reads: std::sync::atomic::AtomicUsize,
    /// Holds a first build after the legacy index loads (migration tests).
    #[cfg(test)]
    hold_build: AtomicBool,
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
    /// A search found the database damaged.
    rebuild: bool,
    flush: bool,
    shutdown: bool,
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

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

impl MessageIndex {
    /// Open the index for `store`, stored at `dir/message-index.sqlite`
    /// (`None` keeps it in memory), and start its worker. `filter` names the
    /// store documents that are chats (the workspace's rows); `None` indexes
    /// every document.
    pub fn open(store: Arc<DocsStore>, dir: Option<&Path>, filter: Option<ChatFilter>) -> Self {
        Self::open_with(store, dir, filter, false)
    }

    fn open_with(
        store: Arc<DocsStore>,
        dir: Option<&Path>,
        filter: Option<ChatFilter>,
        #[cfg_attr(not(test), allow(unused_variables))] hold_build: bool,
    ) -> Self {
        let inner = Arc::new(Inner {
            db: Mutex::new(None),
            reader: Mutex::new(None),
            legacy: RwLock::new(None),
            overlay: RwLock::new(HashMap::new()),
            ready: AtomicBool::new(false),
            store,
            dir: dir.map(Path::to_path_buf),
            filter,
            work: Mutex::new(Work::default()),
            wake: Condvar::new(),
            idle: Condvar::new(),
            #[cfg(test)]
            snapshot_reads: Default::default(),
            #[cfg(test)]
            full_reads: Default::default(),
            #[cfg(test)]
            hold_build: AtomicBool::new(hold_build),
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

    /// Whether the initial build / reconcile finished.
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> MessageIndexStats {
        let inner = &self.inner;
        if let Some(legacy) = read(&inner.legacy).as_ref() {
            return MessageIndexStats {
                chats: legacy.chat_count(),
                messages: legacy.message_count(),
                terms: legacy.term_count(),
            };
        }
        let overlay = read(&inner.overlay);
        inner
            .with_reader(|db| {
                let mut unsaved = 0;
                for doc in overlay.values().flatten() {
                    if !db.has_message(&doc.chat, &doc.message_id)? {
                        unsaved += 1;
                    }
                }
                Ok(MessageIndexStats {
                    chats: db.chat_count()?,
                    messages: db.totals()?.docs as usize + unsaved,
                    terms: db.term_count()?,
                })
            })
            .unwrap_or_default()
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

    /// Ranked hits for `query`: every query term must prefix a token of the
    /// message; BM25 over prose and tool text with a recency boost.
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
        let inner = &self.inner;
        if let Some(legacy) = read(&inner.legacy).as_ref() {
            return legacy.search(query, &options);
        }
        let terms = tokens::query_terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let overlay: Vec<Doc> = read(&inner.overlay).values().flatten().cloned().collect();
        inner
            .with_reader(|db| db.search(&terms, &options, &overlay))
            .unwrap_or_default()
    }

    /// Wait until every noted change is indexed. Shutdown and tests; bounded
    /// by `timeout`.
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
                && !work.busy;
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

    /// Stop the worker.
    pub fn shutdown(&self) {
        lock(&self.inner.work).shutdown = true;
        self.inner.wake.notify_one();
    }
}

impl Inner {
    fn accepts(&self, chat_id: &str) -> bool {
        self.filter.as_ref().is_none_or(|accept| accept(chat_id))
    }

    fn db_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(INDEX_FILE))
    }

    /// Run `f` on the worker's connection (no-op when the database could
    /// not be opened). A damaged file is replaced by an empty one and
    /// rebuilt from the store.
    fn with_db<T: Default>(&self, f: impl FnOnce(&mut Db) -> rusqlite::Result<T>) -> T {
        let mut guard = lock(&self.db);
        let Some(db) = guard.as_mut() else {
            return T::default();
        };
        match f(db) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "message index write failed");
                if db::is_corrupt(&error) {
                    drop(guard);
                    self.reset();
            }
                T::default()
            }
        }
    }

    /// Start over with an empty database (the old one is damaged); the
    /// next reconcile re-reads every chat.
    fn reset(&self) {
        let Some(path) = self.db_path() else {
            return;
        };
        tracing::warn!("message index database damaged; rebuilding");
        lock(&self.reader).take();
        let mut guard = lock(&self.db);
        guard.take();
        db::remove_files(&path);
        match Db::open(Some(&path)) {
            Ok(db) => *guard = Some(db),
            Err(error) => tracing::error!(%error, "message index database failed to reopen"),
        }
        drop(guard);
        if let Ok(reader) = Db::open_reader(&path) {
            *lock(&self.reader) = Some(reader);
        }
        lock(&self.work).reconcile = true;
    }

    /// Run `f` on the search connection (the worker's for an in-memory
    /// index).
    fn with_reader<T>(&self, f: impl FnOnce(&Db) -> rusqlite::Result<T>) -> Option<T> {
        let reader = lock(&self.reader);
        let result = if let Some(db) = reader.as_ref() {
            f(db)
        } else {
            drop(reader);
            let guard = lock(&self.db);
            f(guard.as_ref()?)
        };
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%error, "message index read failed");
                if db::is_corrupt(&error) {
                    self.request_rebuild();
                }
                None
            }
        }
    }

    /// Have the worker replace a damaged database (searches only report).
    fn request_rebuild(&self) {
        let mut work = lock(&self.work);
        work.rebuild = true;
        self.wake.notify_one();
    }

    /// Open the database; on a first build, load the legacy index so
    /// searches keep answering, build from the store, then drop the legacy
    /// file.
    fn start(&self) {
        let path = self.db_path();
        match Db::open(path.as_deref()) {
            Ok(db) => *lock(&self.db) = Some(db),
            Err(error) => {
                tracing::error!(%error, "message index database failed to open");
                return;
            }
        }
        if let Some(path) = &path {
            match Db::open_reader(path) {
                Ok(reader) => *lock(&self.reader) = Some(reader),
                Err(error) => tracing::warn!(%error, "message index reader failed to open"),
            }
        }
        let building = !self.with_db(|db| Ok(db.built()));
        if building {
            if let Some(legacy) = self.load_legacy() {
                *write(&self.legacy) = Some(legacy);
            }
            #[cfg(test)]
            while self.hold_build.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let started = Instant::now();
        self.reconcile();
        if building {
            self.with_db(|db| {
                db.optimize()?;
                db.set_built()
            });
            let totals = self.with_db(|db| db.totals());
            tracing::info!(
                messages = totals.docs,
                ms = started.elapsed().as_millis() as u64,
                "message index built"
            );
            write(&self.legacy).take();
            if let Some(dir) = &self.dir {
                for name in [
                    LEGACY_INDEX_FILE.to_owned(),
                    format!("{LEGACY_INDEX_FILE}.tmp"),
                ] {
                    let _ = std::fs::remove_file(dir.join(name));
                }
            }
        }
    }

    fn load_legacy(&self) -> Option<Index> {
        let path = self.dir.as_ref()?.join(LEGACY_INDEX_FILE);
        let bytes = std::fs::read(&path).ok()?;
        match persist::decode(&bytes) {
            Ok(index) => {
                tracing::info!(
                    messages = index.message_count(),
                    "message index: answering from the old index while the new one builds"
                );
                Some(index)
            }
            Err(error) => {
                tracing::debug!(%error, "message index: old index unreadable");
                None
            }
        }
    }

    /// Index one chat from an open doc: the turn in flight into the
    /// overlay, a finished transcript into the database.
    fn index_live(&self, chat_id: &str, doc: &SessionDoc) {
        if !self.accepts(chat_id) {
            return;
        }
        match doc.read_streaming_tail(STREAMING_TAIL_ENTRIES) {
            Ok(Some(entries)) => {
                let docs = overlay_docs(chat_id, message_texts(entries));
                write(&self.overlay).insert(chat_id.to_owned(), docs);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(chat = %chat_id, %error, "message index: live tail read failed");
            }
        }
        self.index_doc(chat_id, doc, None);
        write(&self.overlay).remove(chat_id);
    }

    /// Diff an open doc's whole transcript into the database.
    fn index_doc(&self, chat_id: &str, doc: &SessionDoc, marker: Option<Marker>) {
        #[cfg(test)]
        self.full_reads.fetch_add(1, Ordering::Relaxed);
        match doc.read_entries() {
            Ok(entries) => {
                let messages = message_texts(entries);
                self.with_db(|db| {
                    db.batch(|db| {
                        db.update_chat(chat_id, &messages)?;
                        if let Some(marker) = marker {
                            db.set_marker(chat_id, Some(marker))?;
                }
                        Ok(())
                    })
                });
            }
            Err(error) => {
                tracing::debug!(chat = %chat_id, %error, "message index: transcript read failed")
            }
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
        let markers = self.with_db(|db| db.markers());
        let mut keep: HashSet<String> = HashSet::new();
        let mut batch: Vec<(String, Vec<MessageText>, Marker)> = Vec::new();
        let mut read = 0usize;
        let write_batch = |batch: &mut Vec<(String, Vec<MessageText>, Marker)>| {
            if batch.is_empty() {
                return;
            }
            self.with_db(|db| {
                db.batch(|db| {
                    for (chat_id, messages, marker) in batch.iter() {
                        db.update_chat(chat_id, messages)?;
                        db.set_marker(chat_id, Some(*marker))?;
                    }
                    Ok(())
                })
            });
            batch.clear();
        };
        for (chat_id, saved_at, len) in versions {
            if !self.accepts(&chat_id) {
                continue;
            }
            keep.insert(chat_id.clone());
            let marker: Marker = (saved_at, len);
            if markers.get(&chat_id) == Some(&Some(marker)) {
                continue;
            }
            if let Some(doc) = open.get(&chat_id).and_then(Weak::upgrade) {
                // The live path owns an open doc; it is at least as new as
                // its snapshot.
                self.index_doc(&chat_id, &doc, Some(marker));
                continue;
            }
            match self.read_snapshot(&chat_id) {
                Ok(messages) => {
                    batch.push((chat_id, messages, marker));
                    read += 1;
                    if batch.len() >= RECONCILE_BATCH {
                        write_batch(&mut batch);
                    }
                }
                Err(error) => {
                    tracing::debug!(chat = %chat_id, %error, "message index: snapshot unreadable");
                }
            }
        }
        write_batch(&mut batch);
        // Chats whose snapshot or registry row is gone, unless a live doc
        // (not yet saved) still holds them.
        let stale: Vec<String> = markers
            .into_keys()
            .filter(|id| !keep.contains(id))
            .filter(|id| open.get(id).and_then(Weak::upgrade).is_none() || !self.accepts(id))
            .collect();
        if !stale.is_empty() {
            self.with_db(|db| {
                db.batch(|db| {
            for id in &stale {
                        db.remove_chat(id)?;
                    }
                    Ok(())
                })
            });
        }
        // Forget docs that closed, and any turn they left in the overlay.
        let closed: Vec<String> = {
            let mut work = lock(&self.work);
            let closed = work
                .open
                .iter()
                .filter(|(_, doc)| doc.strong_count() == 0)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            work.open.retain(|_, doc| doc.strong_count() > 0);
            closed
        };
        if !closed.is_empty() || !stale.is_empty() {
            let mut overlay = write(&self.overlay);
            for id in closed.iter().chain(&stale) {
                overlay.remove(id);
            }
        }
        if read > 0 || !stale.is_empty() {
            tracing::info!(
                read,
                removed = stale.len(),
                ms = started.elapsed().as_millis() as u64,
                "message index reconciled"
            );
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

/// A turn in flight as searchable documents.
fn overlay_docs(chat_id: &str, messages: Vec<MessageText>) -> Vec<Doc> {
    let chat: Arc<str> = chat_id.into();
    messages
        .into_iter()
        .map(|message| {
            let text = index::capped(&message.text, index::MAX_TEXT_BYTES);
            let tools = index::capped(&message.tools, index::MAX_TOOL_BYTES);
            let (_, len) = index::term_counts(text, tools);
            Doc {
                chat: chat.clone(),
                message_id: message.id.as_str().into(),
                role: message.role,
                created_at: message.created_at,
                text: text.into(),
                tools: tools.into(),
                len,
                hash: index::content_hash(&message),
            }
        })
        .collect()
}

fn worker(weak: Weak<Inner>) {
    {
        let Some(inner) = weak.upgrade() else { return };
        inner.start();
        inner.ready.store(true, Ordering::Release);
        inner.idle.notify_all();
    }
    let mut last_reconcile = Instant::now();
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
        if std::mem::take(&mut work.rebuild) {
            drop(work);
            inner.reset();
            continue;
        }
        let reconcile_due = work.reconcile || now.duration_since(last_reconcile) >= RECONCILE_EVERY;
        let removed = std::mem::take(&mut work.removed);
        if due_live.is_empty() && removed.is_empty() && !reconcile_due {
            if work.shutdown {
                drop(work);
                inner.with_db(|db| db.checkpoint());
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
            drop(
                inner
                    .wake
                    .wait_timeout(work, next.max(Duration::from_millis(5))),
            );
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
            {
                let mut overlay = write(&inner.overlay);
                for id in &removed {
                    overlay.remove(id);
                }
            }
            inner.with_db(|db| {
                db.batch(|db| {
                    for id in &removed {
                        db.remove_chat(id)?;
                    }
                    Ok(())
                })
            });
        }
        if reconcile_due {
            inner.reconcile();
            last_reconcile = Instant::now();
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
