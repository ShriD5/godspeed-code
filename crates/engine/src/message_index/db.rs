//! The on-disk index: SQLite with an FTS5 table, updated a chat at a time.
//!
//! - `docs` holds one compact row per indexed transcript entry: its chat,
//!   id, role, time, token count and content hash. `doc_text` beside it
//!   holds the (capped) prose and tool text that snippets are cut from.
//! - `fts` is a contentless FTS5 table (`detail=none`) holding the postings:
//!   one entry per distinct token of each entry, as [`super::tokens`]
//!   produced it (lowercased, CJK one per character, capped length), its
//!   prose and tool counts folded into a private-use suffix ([`posting`]).
//!   The `ascii` tokenizer splits only on the spaces between them, so a
//!   term's prefix range in the dictionary is exactly the old index's
//!   expansion: every query term must prefix a token of the entry.
//! - `chats` records each chat's snapshot marker, written in the same
//!   transaction as its rows, so a restart (or an interrupted first build)
//!   re-reads only chats whose snapshot moved.
//!
//! Ranking is the old index's BM25 (two fields, prefix discount, one idf per
//! expansion, recency boost), computed in Rust walk for walk: the
//! `fts5vocab` instance table lists one row per posting of the tokens a
//! query term expands to, exactly the posting lists the in-memory index
//! kept, so results, order and scores are the same.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use zeron_proto::{MessageSearchHit, MessageSearchRole};

use super::index::{
    self, Doc, MAX_TEXT_BYTES, MAX_TOOL_BYTES, Marker, MessageText, SearchOptions, capped,
    content_hash, term_counts,
};

/// Bumped when the schema or the token stream changes: an older file is
/// rebuilt from the store.
const SCHEMA_VERSION: i64 = 1;

/// Separates a posting's token from its counts: private use, so never part
/// of a token, and not ASCII, so the `ascii` tokenizer keeps it in.
const COUNTS: char = '\u{E000}';
/// Separates the prose count from the tool count.
const TOOL_COUNT: char = '\u{E001}';

/// Page cache per connection (KiB): the index stays on disk.
const CACHE_KIB: i64 = 2_048;

/// The first bytes of every SQLite database file.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

pub(crate) struct Db {
    conn: Connection,
    /// Each entry's `(token count, created_at)` indexed by rowid, as of a
    /// write generation: BM25 needs the length of every posting's document.
    /// 12 bytes a row; refreshed only after the index changes.
    lengths: RefCell<Option<(i64, Lengths)>>,
}

/// Whole-index counters kept in `meta`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub docs: u64,
    pub len: u64,
}

fn role_byte(role: MessageSearchRole) -> i64 {
    match role {
        MessageSearchRole::User => 0,
        MessageSearchRole::Assistant => 1,
    }
}

fn role_from(byte: i64) -> MessageSearchRole {
    if byte == 0 {
        MessageSearchRole::User
    } else {
        MessageSearchRole::Assistant
    }
}

/// Whether `error` means the file itself is damaged (rebuild it).
pub(crate) fn is_corrupt(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

/// Delete the database at `path` and its WAL companions.
pub(crate) fn remove_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// One posting as indexed: the token alone for a single prose occurrence
/// (most of them), else `token U+E000 prose U+E001 tools`.
fn posting(token: &str, text_tf: u16, tool_tf: u16) -> String {
    if (text_tf, tool_tf) == (1, 0) {
        token.to_owned()
    } else {
        format!("{token}{COUNTS}{text_tf}{TOOL_COUNT}{tool_tf}")
    }
}

/// A posting's `(token, prose count, tool count)`.
fn parse_posting(indexed: &str) -> (&str, u16, u16) {
    match indexed.split_once(COUNTS) {
        None => (indexed, 1, 0),
        Some((token, counts)) => {
            let (text, tools) = counts.split_once(TOOL_COUNT).unwrap_or((counts, "0"));
            (token, text.parse().unwrap_or(1), tools.parse().unwrap_or(0))
        }
    }
}

/// An entry's postings, space-separated, in a stable order (a contentless
/// table forgets a row given exactly what it indexed).
pub(crate) fn posting_stream(text: &str, tools: &str) -> String {
    let (counts, _) = term_counts(text, tools);
    let mut tokens: Vec<(&String, &(u16, u16))> = counts.iter().collect();
    tokens.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut out = String::with_capacity(text.len() / 2);
    for (token, &(text_tf, tool_tf)) in tokens {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&posting(token, text_tf, tool_tf));
    }
    out
}

/// A bound above every string that starts with `prefix`.
fn prefix_end(prefix: &str) -> String {
    format!("{prefix}\u{10FFFF}")
}

/// One term's postings: `(token length, rowid, prose tf, tool tf)`, like the
/// in-memory index's posting lists.
type Postings = Vec<(usize, i64, u16, u16)>;

/// `(token count, created_at)` by rowid; `len == 0` marks a missing row.
type Lengths = Vec<(u32, i64)>;

impl Db {
    /// Open (or create) the index at `path`; `None` keeps it in memory. A
    /// file that is not a database, will not open, or was written by another
    /// schema starts over empty.
    pub(crate) fn open(path: Option<&Path>) -> rusqlite::Result<Self> {
        let open = |path: Option<&Path>| -> rusqlite::Result<Self> {
            let conn = match path {
                Some(path) => {
                    let header = std::fs::read(path)
                        .ok()
                        .map(|bytes| bytes.len() >= 16 && bytes[..16] == SQLITE_HEADER[..]);
                    if header == Some(false) {
                        // Garbage (or a truncated file): SQLite would only
                        // notice on a later read.
                        remove_files(path);
                    }
                    Connection::open_with_flags(
                        path,
                        OpenFlags::SQLITE_OPEN_READ_WRITE
                            | OpenFlags::SQLITE_OPEN_CREATE
                            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?
                }
                None => Connection::open_in_memory()?,
            };
            let db = Self {
                conn,
                lengths: RefCell::new(None),
            };
            db.init()?;
            Ok(db)
        };
        match open(path) {
            Ok(db) => Ok(db),
            Err(error) => {
                let Some(path) = path else {
                    return Err(error);
                };
                tracing::warn!(%error, "message index database unusable; rebuilding");
                remove_files(path);
                open(Some(path))
            }
        }
    }

    /// A second connection for searches: WAL lets it read while the worker
    /// writes.
    pub(crate) fn open_reader(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "cache_size", -CACHE_KIB)?;
        Ok(Self {
            conn,
            lengths: RefCell::new(None),
        })
    }

    fn init(&self) -> rusqlite::Result<()> {
        let conn = &self.conn;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -CACHE_KIB)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // Checkpoints shrink the WAL back instead of leaving it at its peak.
        conn.pragma_update(None, "journal_size_limit", 1_048_576)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS vocab;
                 DROP TABLE IF EXISTS fts;
                 DROP TABLE IF EXISTS doc_text;
                 DROP TABLE IF EXISTS docs;
                 DROP TABLE IF EXISTS chats;
                 DROP TABLE IF EXISTS meta;",
            )?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS chats (
                 chat_id TEXT PRIMARY KEY,
                 saved_at INTEGER,
                 len INTEGER
             );
             CREATE TABLE IF NOT EXISTS docs (
                 rowid INTEGER PRIMARY KEY,
                 chat_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 role INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 len INTEGER NOT NULL,
                 hash INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS docs_message ON docs (chat_id, message_id);
             CREATE TABLE IF NOT EXISTS doc_text (
                 rowid INTEGER PRIMARY KEY,
                 text TEXT NOT NULL,
                 tools TEXT NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
                 postings, content='', detail='none', columnsize=0, tokenize='ascii'
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS vocab USING fts5vocab(fts, instance);",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    fn meta(&self, key: &str) -> rusqlite::Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
            .query_row([key], |row| row.get(0))
            .optional()?
            .unwrap_or(0))
    }

    fn set_meta(&self, key: &str, value: i64) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)")?
            .execute(params![key, value])?;
        Ok(())
    }

    fn add_meta(&self, key: &str, by: i64) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = value + excluded.value",
            )?
            .execute(params![key, by])?;
        Ok(())
    }

    /// Whether a full build from the store has finished once.
    pub(crate) fn built(&self) -> bool {
        self.meta("built").unwrap_or(0) == 1
    }

    pub(crate) fn set_built(&self) -> rusqlite::Result<()> {
        self.set_meta("built", 1)
    }

    pub(crate) fn totals(&self) -> rusqlite::Result<Totals> {
        Ok(Totals {
            docs: self.meta("docs")?.max(0) as u64,
            len: self.meta("len")?.max(0) as u64,
        })
    }

    pub(crate) fn has_message(&self, chat_id: &str, message_id: &str) -> rusqlite::Result<bool> {
        self.conn
            .prepare_cached("SELECT 1 FROM docs WHERE chat_id = ?1 AND message_id = ?2")?
            .exists(params![chat_id, message_id])
    }

    pub(crate) fn chat_count(&self) -> rusqlite::Result<usize> {
        self.conn
            .query_row("SELECT count(DISTINCT chat_id) FROM docs", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| n as usize)
    }

    /// Distinct tokens indexed (a scan of the dictionary: stats only).
    pub(crate) fn term_count(&self) -> rusqlite::Result<usize> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT term FROM vocab")?;
        let mut rows = stmt.query([])?;
        let mut tokens: HashSet<String> = HashSet::new();
        while let Some(row) = rows.next()? {
            let (token, _, _) = parse_posting(row.get_ref(0)?.as_str()?);
            if !tokens.contains(token) {
                tokens.insert(token.to_owned());
            }
        }
        Ok(tokens.len())
    }

    /// Run `f` in one transaction (which bumps the write generation).
    pub(crate) fn batch<T>(
        &mut self,
        f: impl FnOnce(&Self) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = f(self).and_then(|value| {
            self.add_meta("generation", 1)?;
            Ok(value)
        });
        match result {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn marker(&self, chat_id: &str) -> rusqlite::Result<Option<Marker>> {
        Ok(self
            .conn
            .prepare_cached("SELECT saved_at, len FROM chats WHERE chat_id = ?1")?
            .query_row([chat_id], |row| {
                Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .optional()?
            .and_then(|(saved_at, len)| Some((saved_at?, len?))))
    }

    pub(crate) fn markers(&self) -> rusqlite::Result<HashMap<String, Option<Marker>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT chat_id, saved_at, len FROM chats")?;
        let rows = stmt.query_map([], |row| {
            let marker = match (row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?) {
                (Some(saved_at), Some(len)) => Some((saved_at, len)),
                _ => None,
            };
            Ok((row.get::<_, String>(0)?, marker))
        })?;
        rows.collect()
    }

    pub(crate) fn set_marker(&self, chat_id: &str, marker: Option<Marker>) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO chats (chat_id, saved_at, len) VALUES (?1, ?2, ?3)
                 ON CONFLICT (chat_id) DO UPDATE SET saved_at = excluded.saved_at, len = excluded.len",
            )?
            .execute(params![chat_id, marker.map(|m| m.0), marker.map(|m| m.1)])?;
        Ok(())
    }

    /// Drop a row's postings (keeping its `docs` row); returns its length.
    fn unpost(&self, rowid: i64) -> rusqlite::Result<i64> {
        let row = self
            .conn
            .prepare_cached(
                "SELECT doc_text.text, doc_text.tools, docs.len FROM docs
                 JOIN doc_text ON doc_text.rowid = docs.rowid WHERE docs.rowid = ?1",
            )?
            .query_row([rowid], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .optional()?;
        let Some((text, tools, len)) = row else {
            return Ok(0);
        };
        // A contentless table forgets a row given exactly what it indexed.
        self.conn
            .prepare_cached("INSERT INTO fts (fts, rowid, postings) VALUES ('delete', ?1, ?2)")?
            .execute(params![rowid, posting_stream(&text, &tools)])?;
        Ok(len)
    }

    fn delete(&self, rowid: i64) -> rusqlite::Result<i64> {
        let len = self.unpost(rowid)?;
        self.conn
            .prepare_cached("DELETE FROM docs WHERE rowid = ?1")?
            .execute([rowid])?;
        self.conn
            .prepare_cached("DELETE FROM doc_text WHERE rowid = ?1")?
            .execute([rowid])?;
        Ok(len)
    }

    /// Write `message` at `rowid` (a re-streamed entry keeps its row, like
    /// the in-memory index kept its slot) or a new row; returns its length.
    fn post(
        &self,
        rowid: Option<i64>,
        chat_id: &str,
        message: &MessageText,
        hash: u64,
    ) -> rusqlite::Result<i64> {
        let text = capped(&message.text, MAX_TEXT_BYTES);
        let tools = capped(&message.tools, MAX_TOOL_BYTES);
        let (_, len) = term_counts(text, tools);
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO docs (rowid, chat_id, message_id, role, created_at, len, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                rowid,
                chat_id,
                message.id,
                role_byte(message.role),
                message.created_at,
                len,
                hash as i64,
            ])?;
        let rowid = rowid.unwrap_or_else(|| self.conn.last_insert_rowid());
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO doc_text (rowid, text, tools) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![rowid, text, tools])?;
        self.conn
            .prepare_cached("INSERT INTO fts (rowid, postings) VALUES (?1, ?2)")?
            .execute(params![rowid, posting_stream(text, tools)])?;
        Ok(i64::from(len))
    }

    /// Make `messages` the chat's indexed transcript: unchanged entries are
    /// skipped by content hash, changed ones re-indexed in place, new ones
    /// added and vanished ones dropped. Call inside [`Self::batch`]. Returns
    /// whether anything changed.
    pub(crate) fn update_chat(
        &self,
        chat_id: &str,
        messages: &[MessageText],
    ) -> rusqlite::Result<bool> {
        let mut previous: HashMap<String, (i64, u64)> = self
            .conn
            .prepare_cached("SELECT message_id, rowid, hash FROM docs WHERE chat_id = ?1")?
            .query_map([chat_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get(1)?, row.get::<_, i64>(2)? as u64),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if !self
            .conn
            .prepare_cached("SELECT 1 FROM chats WHERE chat_id = ?1")?
            .exists([chat_id])?
        {
            self.set_marker(chat_id, None)?;
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(messages.len());
        let (mut docs, mut len) = (0i64, 0i64);
        let mut changed = false;
        for message in messages {
            if !seen.insert(message.id.as_str()) {
                continue;
            }
            let hash = content_hash(message);
            let rowid = match previous.remove(message.id.as_str()) {
                Some((_, old)) if old == hash => continue,
                Some((rowid, _)) => {
                    len -= self.unpost(rowid)?;
                    docs -= 1;
                    Some(rowid)
                }
                None => None,
            };
            len += self.post(rowid, chat_id, message, hash)?;
            docs += 1;
            changed = true;
        }
        for (_, (rowid, _)) in previous {
            len -= self.delete(rowid)?;
            docs -= 1;
            changed = true;
        }
        self.add_meta("docs", docs)?;
        self.add_meta("len", len)?;
        Ok(changed)
    }

    /// Forget a chat. Call inside [`Self::batch`].
    pub(crate) fn remove_chat(&self, chat_id: &str) -> rusqlite::Result<bool> {
        let rowids: Vec<i64> = self
            .conn
            .prepare_cached("SELECT rowid FROM docs WHERE chat_id = ?1")?
            .query_map([chat_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut len = 0;
        for &rowid in &rowids {
            len += self.delete(rowid)?;
        }
        self.add_meta("docs", -(rowids.len() as i64))?;
        self.add_meta("len", -len)?;
        let removed = self
            .conn
            .prepare_cached("DELETE FROM chats WHERE chat_id = ?1")?
            .execute([chat_id])?;
        Ok(removed > 0 || !rowids.is_empty())
    }

    /// Merge the FTS segments (after a bulk build): smaller and faster.
    pub(crate) fn optimize(&self) -> rusqlite::Result<()> {
        self.conn
            .execute("INSERT INTO fts (fts) VALUES ('optimize')", [])?;
        self.checkpoint()
    }

    /// Fold the WAL into the database and truncate it.
    pub(crate) fn checkpoint(&self) -> rusqlite::Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
    }

    /// Every posting of the tokens starting with `term`.
    fn postings(&self, term: &str) -> rusqlite::Result<Postings> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT term, doc FROM vocab WHERE term >= ?1 AND term < ?2")?;
        let mut rows = stmt.query(params![term, prefix_end(term)])?;
        let mut out: Postings = Vec::new();
        while let Some(row) = rows.next()? {
            let (token, text_tf, tool_tf) = parse_posting(row.get_ref(0)?.as_str()?);
            if token.starts_with(term) {
                out.push((token.len(), row.get(1)?, text_tf, tool_tf));
            }
        }
        Ok(out)
    }

    /// Run `f` with every entry's `(length, created_at)`, reloading them
    /// only when the index changed since the last search.
    fn with_lengths<T>(&self, f: impl FnOnce(&Lengths) -> T) -> rusqlite::Result<T> {
        let generation = self.meta("generation")?;
        let mut cache = self.lengths.borrow_mut();
        if cache.as_ref().is_none_or(|(at, _)| *at != generation) {
            let mut lengths: Lengths = Vec::new();
            let mut stmt = self
                .conn
                .prepare_cached("SELECT rowid, len, created_at FROM docs ORDER BY rowid")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let rowid = row.get::<_, i64>(0)?.max(0) as usize;
                if rowid >= lengths.len() {
                    lengths.resize(rowid + 1, (0, 0));
                }
                lengths[rowid] = ((row.get::<_, i64>(1)? as u32).max(1), row.get(2)?);
            }
            *cache = Some((generation, lengths));
        }
        Ok(f(&cache.as_ref().expect("filled above").1))
    }

    fn load_doc(&self, rowid: i64) -> rusqlite::Result<Option<Doc>> {
        self.conn
            .prepare_cached(
                "SELECT docs.chat_id, docs.message_id, docs.role, docs.created_at, docs.len,
                        docs.hash, doc_text.text, doc_text.tools
                 FROM docs JOIN doc_text ON doc_text.rowid = docs.rowid WHERE docs.rowid = ?1",
            )?
            .query_row([rowid], |row| {
                Ok(Doc {
                    chat: Arc::from(row.get::<_, String>(0)?),
                    message_id: row.get::<_, String>(1)?.into_boxed_str(),
                    role: role_from(row.get(2)?),
                    created_at: row.get(3)?,
                    len: row.get::<_, i64>(4)? as u32,
                    hash: row.get::<_, i64>(5)? as u64,
                    text: row.get::<_, String>(6)?.into_boxed_str(),
                    tools: row.get::<_, String>(7)?.into_boxed_str(),
                })
            })
            .optional()
    }

    fn chat_of(&self, rowid: i64) -> rusqlite::Result<Option<(String, String)>> {
        self.conn
            .prepare_cached("SELECT chat_id, message_id FROM docs WHERE rowid = ?1")?
            .query_row([rowid], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
    }

    /// Ranked hits for `terms`, with the entries `overlay` provides scored
    /// alongside (and shadowing stored copies of the same messages).
    pub(crate) fn search(
        &self,
        terms: &[String],
        options: &SearchOptions,
        overlay: &[Doc],
    ) -> rusqlite::Result<Vec<MessageSearchHit>> {
        if terms.is_empty() || options.limit == 0 {
            return Ok(Vec::new());
        }
        let totals = self.totals()?;
        let n = totals.docs + overlay.len() as u64;
        if n == 0 {
            return Ok(Vec::new());
        }
        let total_len = totals.len + overlay.iter().map(|doc| u64::from(doc.len)).sum::<u64>();
        let mut postings = Vec::with_capacity(terms.len());
        let mut dfs = Vec::with_capacity(terms.len());
        for term in terms {
            let list = self.postings(term)?;
            let overlay_df = overlay
                .iter()
                .filter(|doc| index::doc_has_prefix(doc, term))
                .count() as u64;
            dfs.push(list.len() as u64 + overlay_df);
            postings.push(list);
        }
        if dfs.contains(&0) {
            return Ok(Vec::new());
        }
        let scorer = index::Scorer::new(n, total_len, &dfs);
        let shadowed: HashSet<(&str, &str)> = overlay
            .iter()
            .map(|doc| (&*doc.chat, &*doc.message_id))
            .collect();

        // Stored candidates as (score, rowid), best first.
        let ranked = self.rank(terms, &postings, &scorer, options.now_ms)?;
        let mut overlay_ranked: Vec<(f32, &Doc)> = overlay
            .iter()
            .filter_map(|doc| Some((scorer.score(doc, terms, options.now_ms)?, doc)))
            .collect();
        overlay_ranked.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));

        // Merge, loading stored documents only as they are reached.
        let mut hits = Vec::new();
        let mut per_chat: HashMap<String, usize> = HashMap::new();
        let mut stored = ranked.into_iter().peekable();
        let mut live = overlay_ranked.into_iter().peekable();
        while hits.len() < options.limit {
            // Stored entries win ties (the overlay is newest, like the old
            // index's later slots).
            let take_live = match (stored.peek(), live.peek()) {
                (None, None) => break,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some((s, _)), Some((l, _))) => l > s,
            };
            let (score, doc) = if take_live {
                let (score, doc) = live.next().expect("peeked");
                (score, Cow::Borrowed(doc))
            } else {
                let (score, rowid) = stored.next().expect("peeked");
                let Some((chat, message)) = self.chat_of(rowid)? else {
                    continue;
                };
                let capped_chat = options
                    .per_chat
                    .is_some_and(|cap| per_chat.get(&chat).copied().unwrap_or(0) >= cap);
                if shadowed.contains(&(chat.as_str(), message.as_str()))
                    || options.chat_filter.is_some_and(|accept| !accept(&chat))
                    || capped_chat
                {
                    continue;
                }
                let Some(doc) = self.load_doc(rowid)? else {
                    continue;
                };
                (score, Cow::Owned(doc))
            };
            if options.chat_filter.is_some_and(|accept| !accept(&doc.chat)) {
                continue;
            }
            if let Some(cap) = options.per_chat {
                let seen = per_chat.entry(doc.chat.to_string()).or_default();
                if *seen >= cap {
                    continue;
                }
                *seen += 1;
            }
            hits.push(index::hit(&doc, terms, score));
        }
        Ok(hits)
    }

    /// The in-memory index's ranking, walk for walk: per term, every posting
    /// of its expansions scores the documents that matched the terms before
    /// it, keeping each document's best expansion.
    fn rank(
        &self,
        terms: &[String],
        postings: &[Postings],
        scorer: &index::Scorer,
        now_ms: i64,
    ) -> rusqlite::Result<Vec<(f32, i64)>> {
        self.with_lengths(|lengths| {
            // `matched[row] == i`: the row matched the first `i` terms.
            let rows = lengths.len();
            let mut matched = vec![0u16; rows];
            let mut best = vec![0f32; rows];
            let mut score = vec![0f32; rows];
            let mut candidates: Vec<usize> = Vec::new();
            for (i, (term, postings)) in terms.iter().zip(postings).enumerate() {
                let round = i as u16;
                let mut touched: Vec<usize> = Vec::new();
                for &(token_len, doc, text_tf, tool_tf) in postings {
                    let doc = doc as usize;
                    // A posting always has its row; skip any that raced a write.
                    let Some(&(len, _)) = lengths.get(doc).filter(|l| l.0 > 0) else {
                        continue;
                    };
                    let state = matched[doc];
                    if state != round && state != round + 1 {
                        continue;
                    }
                    let s = scorer.posting(i, token_len, term.len(), text_tf, tool_tf, len as f32);
                    if state == round {
                        matched[doc] = round + 1;
                        best[doc] = s;
                        touched.push(doc);
                    } else if s > best[doc] {
                        best[doc] = s;
                    }
                }
                for &doc in &touched {
                    score[doc] += best[doc];
                }
                if touched.is_empty() {
                    return Vec::new();
                }
                candidates = touched;
            }
            let all = terms.len() as u16;
            let mut ranked: Vec<(f32, i64)> = candidates
                .into_iter()
                .filter(|&doc| matched[doc] == all)
                .map(|doc| {
                    (
                        score[doc] * index::recency(lengths[doc].1, now_ms),
                        doc as i64,
                    )
                })
                .collect();
            ranked.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            ranked
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str) -> MessageText {
        MessageText {
            id: id.into(),
            role: MessageSearchRole::Assistant,
            created_at: 0,
            text: text.into(),
            tools: String::new(),
        }
    }

    fn search(db: &Db, query: &str) -> Vec<String> {
        let terms = super::super::tokens::query_terms(query);
        let options = SearchOptions {
            limit: 10,
            ..Default::default()
        };
        db.search(&terms, &options, &[])
            .unwrap()
            .into_iter()
            .map(|hit| hit.message_id)
            .collect()
    }

    #[test]
    fn postings_keep_the_old_tokens_and_counts() {
        assert_eq!(
            posting_stream("fix workspace_search.rs (Café) fix", "cargo fix"),
            "café cargo\u{E000}0\u{E001}1 fix\u{E000}2\u{E001}1 rs search workspace"
        );
        assert_eq!(posting_stream("全文検索", ""), "全 文 検 索");
        assert_eq!(posting_stream("  -- ", ""), "");
        for (token, text, tools) in [("fix", 2, 1), ("café", 1, 0), ("x", 0, 3)] {
            assert_eq!(
                parse_posting(&posting(token, text, tools)),
                (token, text, tools)
            );
        }
    }

    #[test]
    fn updates_replace_and_drop_rows_and_keep_totals() {
        let mut db = Db::open(None).unwrap();
        let both = [message("a", "hello world"), message("b", "other words")];
        db.batch(|db| db.update_chat("c", &both)).unwrap();
        assert_eq!(db.totals().unwrap(), Totals { docs: 2, len: 4 });
        assert_eq!(search(&db, "wor"), ["a", "b"]);
        // Unchanged: nothing written.
        assert!(!db.batch(|db| db.update_chat("c", &both)).unwrap());
        // Edited in place and dropped.
        let rowid = |db: &Db| -> i64 {
            db.conn
                .query_row("SELECT rowid FROM docs WHERE message_id = 'a'", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        let before = rowid(&db);
        assert!(
            db.batch(|db| db.update_chat("c", &[message("a", "hello there")]))
                .unwrap()
        );
        assert_eq!(rowid(&db), before, "a re-indexed entry keeps its row");
        assert_eq!(db.totals().unwrap(), Totals { docs: 1, len: 2 });
        assert!(search(&db, "world").is_empty());
        assert_eq!(search(&db, "there"), ["a"]);
        assert_eq!(
            db.term_count().unwrap(),
            2,
            "deleted rows leave no terms behind"
        );
        assert!(db.batch(|db| db.remove_chat("c")).unwrap());
        assert_eq!(db.totals().unwrap(), Totals::default());
        assert!(search(&db, "hello").is_empty());
        assert_eq!(db.term_count().unwrap(), 0);
    }

    #[test]
    fn markers_round_trip_and_reopen_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        {
            let mut db = Db::open(Some(&path)).unwrap();
            db.batch(|db| {
                db.update_chat("c", &[message("a", "persisted text")])?;
                db.set_marker("c", Some((7, 9)))
            })
            .unwrap();
            assert!(!db.built());
            db.set_built().unwrap();
        }
        let db = Db::open(Some(&path)).unwrap();
        assert!(db.built());
        assert_eq!(db.marker("c").unwrap(), Some((7, 9)));
        assert_eq!(search(&db, "persist"), ["a"]);
        let reader = Db::open_reader(&path).unwrap();
        assert_eq!(search(&reader, "persist"), ["a"]);
        // Garbage on disk starts over.
        drop((db, reader));
        remove_files(&path);
        std::fs::write(&path, b"not a database at all").unwrap();
        let db = Db::open(Some(&path)).unwrap();
        assert!(!db.built());
        assert!(search(&db, "persist").is_empty());
    }
}
