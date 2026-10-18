//! The usage index on disk: `usage-history.sqlite3` beside the other
//! engine stores. One row per usage record (a Godspeed turn or one model
//! call from a Claude Code / Codex log), the read position of every log
//! file, and the snapshot marker of every chat already indexed.
//!
//! It is a derived index: a schema change rebuilds it from the sources.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use super::{Source, UsageRecord};

pub(super) const FILE_NAME: &str = "usage-history.sqlite3";
const SCHEMA_VERSION: i64 = 1;

/// A log file's read position: everything before `offset` is indexed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FileMark {
    pub offset: u64,
    pub mtime_ms: i64,
    /// Hash of the bytes just before `offset`, to notice a rewritten file.
    pub guard: u64,
    /// The parser's carried state at `offset` (Codex: model, session).
    pub state: String,
}

pub(super) fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS records;
             DROP TABLE IF EXISTS files;
             DROP TABLE IF EXISTS chats;",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS records (
             key TEXT PRIMARY KEY,
             source INTEGER NOT NULL,
             at_ms INTEGER NOT NULL,
             started_ms INTEGER,
             provider TEXT NOT NULL,
             model TEXT,
             session_id TEXT,
             chat_id TEXT,
             cwd TEXT,
             input INTEGER NOT NULL,
             output INTEGER NOT NULL,
             cache_read INTEGER NOT NULL,
             cache_write INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS records_at ON records (at_ms);
         CREATE TABLE IF NOT EXISTS files (
             path TEXT PRIMARY KEY,
             source INTEGER NOT NULL,
             offset INTEGER NOT NULL,
             mtime_ms INTEGER NOT NULL,
             guard INTEGER NOT NULL,
             state TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS chats (
             chat_id TEXT PRIMARY KEY,
             saved_at INTEGER NOT NULL,
             len INTEGER NOT NULL
         );",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(conn)
}

/// Keep `records`. A Godspeed turn replaces its row (a re-read stamp is the
/// truth); a log record keeps the larger of each count, since Claude Code
/// repeats a message's usage on every content block and a streamed copy can
/// carry a partial output count.
pub(super) fn upsert(conn: &Connection, records: &[UsageRecord]) -> rusqlite::Result<()> {
    let mut turn = conn.prepare_cached(
        "INSERT OR REPLACE INTO records
             (key, source, at_ms, started_ms, provider, model, session_id, chat_id, cwd,
              input, output, cache_read, cache_write)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?;
    let mut log = conn.prepare_cached(
        "INSERT INTO records
             (key, source, at_ms, started_ms, provider, model, session_id, chat_id, cwd,
              input, output, cache_read, cache_write)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT (key) DO UPDATE SET
             input = max(input, excluded.input),
             output = max(output, excluded.output),
             cache_read = max(cache_read, excluded.cache_read),
             cache_write = max(cache_write, excluded.cache_write)",
    )?;
    for r in records {
        let statement = if r.source == Source::Godspeed {
            &mut turn
        } else {
            &mut log
        };
        statement.execute(params![
            r.key,
            r.source as i64,
            r.at_ms,
            r.started_ms,
            r.provider,
            r.model,
            r.session_id,
            r.chat_id,
            r.cwd,
            r.tokens.input as i64,
            r.tokens.output as i64,
            r.tokens.cache_read as i64,
            r.tokens.cache_write as i64,
        ])?;
    }
    Ok(())
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    let count = |ix: usize| -> rusqlite::Result<u64> { Ok(row.get::<_, i64>(ix)?.max(0) as u64) };
    Ok(UsageRecord {
        key: row.get(0)?,
        source: Source::from_i64(row.get(1)?),
        at_ms: row.get(2)?,
        started_ms: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        session_id: row.get(6)?,
        chat_id: row.get(7)?,
        cwd: row.get(8)?,
        tokens: zeron_proto::UsageTokens {
            input: count(9)?,
            output: count(10)?,
            cache_read: count(11)?,
            cache_write: count(12)?,
        },
    })
}

/// Records with `at_ms` in `[from, to)`.
pub(super) fn records_between(
    conn: &Connection,
    from: i64,
    to: i64,
) -> rusqlite::Result<Vec<UsageRecord>> {
    let mut statement = conn.prepare_cached(
        "SELECT key, source, at_ms, started_ms, provider, model, session_id, chat_id, cwd,
                input, output, cache_read, cache_write
         FROM records WHERE at_ms >= ?1 AND at_ms < ?2",
    )?;
    statement
        .query_map(params![from, to], record_from_row)?
        .collect()
}

/// The oldest record's time, if any.
pub(super) fn first_at(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT min(at_ms) FROM records", [], |row| row.get(0))
}

pub(super) fn count_turns(conn: &Connection) -> rusqlite::Result<u64> {
    conn.query_row(
        "SELECT count(*) FROM records WHERE source = ?1",
        params![Source::Godspeed as i64],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
}

pub(super) fn count_files(conn: &Connection, source: Source) -> rusqlite::Result<u64> {
    conn.query_row(
        "SELECT count(*) FROM files WHERE source = ?1",
        params![source as i64],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
}

pub(super) fn file_mark(conn: &Connection, path: &str) -> rusqlite::Result<Option<FileMark>> {
    conn.query_row(
        "SELECT offset, mtime_ms, guard, state FROM files WHERE path = ?1",
        params![path],
        |row| {
            Ok(FileMark {
                offset: row.get::<_, i64>(0)?.max(0) as u64,
                mtime_ms: row.get(1)?,
                guard: row.get::<_, i64>(2)? as u64,
                state: row.get(3)?,
            })
        },
    )
    .optional()
}

pub(super) fn set_file_mark(
    conn: &Connection,
    path: &str,
    source: Source,
    mark: &FileMark,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO files (path, source, offset, mtime_ms, guard, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            path,
            source as i64,
            mark.offset as i64,
            mark.mtime_ms,
            mark.guard as i64,
            mark.state
        ],
    )?;
    Ok(())
}

pub(super) fn chat_marks(conn: &Connection) -> rusqlite::Result<Vec<(String, i64, i64)>> {
    let mut statement = conn.prepare("SELECT chat_id, saved_at, len FROM chats")?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
}

pub(super) fn set_chat_mark(
    conn: &Connection,
    chat_id: &str,
    saved_at: i64,
    len: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO chats (chat_id, saved_at, len) VALUES (?1, ?2, ?3)",
        params![chat_id, saved_at, len],
    )?;
    Ok(())
}
