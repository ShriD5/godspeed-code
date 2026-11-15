//! Usage history: an indexed local record of every token this device's
//! agents spent, for Settings → Usage's history (`GetUsageHistory`).
//!
//! Two kinds of source feed one SQLite index ([`store`],
//! `usage-history.sqlite3` beside the other engine stores):
//!
//! - **Godspeed turns** ([`turns`]): the usage the host stamps on each
//!   finished turn ([`zeron_doc::TurnStats`]). The first scan backfills
//!   every stored chat from its snapshot; afterwards only chats whose
//!   snapshot changed are re-read, and a turn just stamped is read from its
//!   live doc ([`UsageIndex::note_turn`]) so it shows before the next save.
//! - **Agent logs** ([`logs`]): Claude Code transcripts and Codex sessions
//!   on this device, the way T3 Code's usage page reads them, for history
//!   from before Godspeed or outside it. Read-only and incremental (each
//!   file keeps a read position), bounded per pass.
//!
//! Godspeed's own Claude Code and Codex turns are in those logs too. A log
//! record inside a Godspeed turn of the same harness session is that turn
//! and is skipped; log records of a session a chat resumes (imported chats
//! continue their source session) are attributed to that chat.
//!
//! Scans run on a background thread, started by a query when the last one
//! is stale (never at boot: a device that never opens the page never reads
//! its logs). A query waits briefly for a running scan, then answers from
//! what is indexed and says whether it is still catching up.

mod aggregate;
mod logs;
mod store;
mod turns;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use zeron_proto::{UsageHistory, UsageHistoryRequest, UsageTokens};
use zeron_sync::DocsStore;

use crate::EngineError;
use crate::doc_host::DocHost;
use crate::workspace_host::WorkspaceHost;
use aggregate::{ChatInfo, Registry};

/// A finished scan this recent is reused by the next query.
const SCAN_FRESH: Duration = Duration::from_secs(20);
/// How long a query waits on a running scan before answering with what is
/// indexed so far.
const QUERY_WAIT: Duration = Duration::from_millis(1_500);
/// Log bytes read per chunk: a chunk's records are written before the next
/// is read, so a huge transcript never sits in memory whole.
const CHUNK_BYTES: u64 = 32 * 1024 * 1024;
/// Records kept in memory before a batch is written.
const BATCH: usize = 2_000;

/// Where a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Source {
    Godspeed = 0,
    ClaudeLog = 1,
    CodexLog = 2,
}

impl Source {
    fn from_i64(value: i64) -> Self {
        match value {
            1 => Source::ClaudeLog,
            2 => Source::CodexLog,
            _ => Source::Godspeed,
        }
    }
}

/// One usage record: a Godspeed turn, or one model call from a log.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageRecord {
    /// Stable identity (`gs:<chat>:<entry>`, `claude:<msg>:<req>`, …).
    pub key: String,
    pub source: Source,
    /// When the usage happened (a turn's end, a call's timestamp), epoch ms.
    pub at_ms: i64,
    /// Godspeed turns: when the prompt was sent.
    pub started_ms: Option<i64>,
    /// Kebab-case harness id.
    pub provider: String,
    pub model: Option<String>,
    /// Harness-native session id.
    pub session_id: Option<String>,
    /// Godspeed turns: the chat.
    pub chat_id: Option<String>,
    /// Log records: the working directory the agent ran in.
    pub cwd: Option<String>,
    pub tokens: UsageTokens,
}

/// One day of Godspeed's own turns (the telemetry ping's usage figures).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DayUsage {
    pub turns: u64,
    /// Finished turns per kebab-case harness id.
    pub by_harness: std::collections::BTreeMap<String, u64>,
    pub tokens: UsageTokens,
    /// Estimated USD at list API prices (priced models only).
    pub cost_usd: f64,
}

/// The agent log locations on this device.
#[derive(Debug, Clone, Default)]
pub struct UsageLogRoots {
    /// Claude Code's projects dir (`~/.claude/projects`, or under
    /// `$CLAUDE_CONFIG_DIR`).
    pub claude_projects: Option<PathBuf>,
    /// Codex's home (`~/.codex`, or `$CODEX_HOME`): `sessions` and
    /// `archived_sessions` are read.
    pub codex_home: Option<PathBuf>,
}

impl UsageLogRoots {
    /// The locations the agents use, honoring the same env overrides as the
    /// chat importer.
    pub fn detect() -> Self {
        let roots = crate::chat_import::ImportRoots::detect();
        Self {
            claude_projects: Some(roots.claude_projects),
            codex_home: Some(roots.codex_home),
        }
    }

    fn dirs(&self) -> Vec<(PathBuf, Source)> {
        let mut dirs = Vec::new();
        if let Some(dir) = &self.claude_projects {
            dirs.push((dir.clone(), Source::ClaudeLog));
        }
        if let Some(home) = &self.codex_home {
            dirs.push((home.join("sessions"), Source::CodexLog));
            dirs.push((home.join("archived_sessions"), Source::CodexLog));
        }
        dirs
    }
}

/// The engine pieces the Godspeed side reads.
#[derive(Clone)]
pub struct UsageDeps {
    pub store: Arc<DocsStore>,
    pub workspace: WorkspaceHost,
    pub doc_host: DocHost,
}

#[derive(Default)]
struct ScanState {
    running: bool,
    finished_at: Option<Instant>,
    /// At least one full scan finished since boot (the backfill, the first
    /// time this index exists).
    caught_up: bool,
}

struct Inner {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
    roots: UsageLogRoots,
    deps: Option<UsageDeps>,
    /// Chats with a turn stamped since their last read.
    dirty: Mutex<HashSet<String>>,
    scan: Mutex<ScanState>,
    scan_done: Condvar,
}

#[derive(Clone)]
pub struct UsageIndex {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn store_err(err: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("usage history: {err}"))
}

/// The zone a request's days follow: the named one, else this host's,
/// else UTC.
fn resolve_zone(name: Option<&str>) -> chrono_tz::Tz {
    name.and_then(|zone| zone.trim().parse().ok())
        .or_else(|| zeron_proto::local_time_zone().and_then(|zone| zone.parse().ok()))
        .unwrap_or(chrono_tz::UTC)
}

impl UsageIndex {
    /// The index in `data_dir`. The database opens on first use.
    pub fn open(data_dir: &Path, roots: UsageLogRoots, deps: Option<UsageDeps>) -> Self {
        Self {
            inner: Arc::new(Inner {
                path: data_dir.join(store::FILE_NAME),
                conn: Mutex::new(None),
                roots,
                deps,
                dirty: Mutex::new(HashSet::new()),
                scan: Mutex::new(ScanState::default()),
                scan_done: Condvar::new(),
            }),
        }
    }

    /// A turn of `chat_id` was just stamped: read it from the live doc on
    /// the next scan, and make that scan due.
    pub fn note_turn(&self, chat_id: &str) {
        lock(&self.inner.dirty).insert(chat_id.to_owned());
        lock(&self.inner.scan).finished_at = None;
    }

    /// Whether a stamped turn of `chat_id` awaits the next scan.
    #[cfg(test)]
    pub(crate) fn noted(&self, chat_id: &str) -> bool {
        lock(&self.inner.dirty).contains(chat_id)
    }

    /// The history for `request`, after letting a due scan run (briefly).
    pub async fn history(&self, request: UsageHistoryRequest) -> Result<UsageHistory, EngineError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.ensure_scan(true);
            let indexing = this.wait_for_scan(QUERY_WAIT);
            let registry = this.registry();
            let mut history =
                this.query(&request, chrono::Utc::now().timestamp_millis(), &registry)?;
            history.sources.indexing = indexing;
            Ok(history)
        })
        .await
        .map_err(|e| EngineError::Other(e.to_string()))?
    }

    /// Godspeed's own turns that ended in `[from_ms, to_ms)`, after reading
    /// any chat stamped or saved since the last pass. Agent logs are never
    /// read for this (only the usage page does that). Blocking.
    pub fn godspeed_between(&self, from_ms: i64, to_ms: i64) -> Result<DayUsage, EngineError> {
        // Share the scan slot with the page's scans: wait out a running one,
        // then catch up on chats alone.
        let claimed = {
            let scan = lock(&self.inner.scan);
            let (mut scan, _) = self
                .inner
                .scan_done
                .wait_timeout_while(scan, Duration::from_secs(30), |s| s.running)
                .unwrap_or_else(PoisonError::into_inner);
            if scan.running {
                false
            } else {
                scan.running = true;
                true
            }
        };
        if claimed {
            let scanned = match &self.inner.deps {
                Some(deps) => self.scan_chats(deps),
                None => Ok(()),
            };
            let mut scan = lock(&self.inner.scan);
            scan.running = false;
            self.inner.scan_done.notify_all();
            drop(scan);
            if let Err(err) = scanned {
                tracing::debug!(error = %err, "usage history chat scan failed");
            }
        }
        let records = self.with_conn(|conn| store::records_between(conn, from_ms, to_ms))?;
        Ok(aggregate::godspeed_totals(&records))
    }

    /// Insert records directly (tests of consumers like telemetry).
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, records: &[UsageRecord]) {
        self.with_conn(|conn| store::upsert(conn, records)).unwrap();
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T, EngineError> {
        let mut conn = lock(&self.inner.conn);
        if conn.is_none() {
            *conn = Some(store::open(&self.inner.path).map_err(store_err)?);
        }
        f(conn.as_mut().expect("opened")).map_err(store_err)
    }

    /// Start a background scan unless one is running or (with `if_stale`)
    /// the last finished moments ago.
    fn ensure_scan(&self, if_stale: bool) {
        {
            let mut scan = lock(&self.inner.scan);
            if scan.running
                || (if_stale && scan.finished_at.is_some_and(|at| at.elapsed() < SCAN_FRESH))
            {
                return;
            }
            scan.running = true;
        }
        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("usage-history-scan".into())
            .spawn(move || {
                let started = Instant::now();
                if let Err(err) = this.scan_now() {
                    tracing::warn!(error = %err, "usage history scan failed");
                }
                tracing::debug!(
                    ms = started.elapsed().as_millis() as u64,
                    "usage history scanned"
                );
                let mut scan = lock(&this.inner.scan);
                scan.running = false;
                scan.finished_at = Some(Instant::now());
                scan.caught_up = true;
                this.inner.scan_done.notify_all();
            });
        if spawned.is_err() {
            lock(&self.inner.scan).running = false;
        }
    }

    /// Wait up to `timeout` for a running scan. `true` when the index is
    /// still catching up afterwards.
    fn wait_for_scan(&self, timeout: Duration) -> bool {
        let scan = lock(&self.inner.scan);
        let (scan, _) = self
            .inner
            .scan_done
            .wait_timeout_while(scan, timeout, |s| s.running)
            .unwrap_or_else(PoisonError::into_inner);
        scan.running || !scan.caught_up
    }

    /// One full pass, on the calling thread: Godspeed chats, then logs.
    fn scan_now(&self) -> Result<(), EngineError> {
        if let Some(deps) = &self.inner.deps {
            self.scan_chats(deps)?;
        }
        self.scan_logs()
    }

    /// Re-read every registry chat whose snapshot changed since it was
    /// indexed, and every chat stamped since (from its live doc).
    fn scan_chats(&self, deps: &UsageDeps) -> Result<(), EngineError> {
        let chats: HashSet<String> = deps
            .workspace
            .read_chats()?
            .into_iter()
            .map(|chat| chat.id)
            .collect();
        let versions = deps
            .store
            .snapshot_versions()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let marks: HashMap<String, (i64, i64)> = self
            .with_conn(|conn| store::chat_marks(conn))?
            .into_iter()
            .map(|(id, saved_at, len)| (id, (saved_at, len)))
            .collect();
        for (chat_id, saved_at, len) in versions {
            // Registry rows only: rollback copies share the store.
            if !chats.contains(&chat_id) || marks.get(&chat_id) == Some(&(saved_at, len)) {
                continue;
            }
            let entries = deps
                .store
                .load_snapshot(&chat_id)
                .ok()
                .flatten()
                .and_then(|bytes| {
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes).ok()?;
                    zeron_doc::SessionDoc::from_doc(raw).read_entries().ok()
                });
            let records = entries
                .map(|entries| turns::turn_records(&chat_id, &entries))
                .unwrap_or_default();
            self.with_conn(|conn| {
                let tx = conn.transaction()?;
                store::upsert(&tx, &records)?;
                store::set_chat_mark(&tx, &chat_id, saved_at, len)?;
                tx.commit()
            })?;
        }
        let dirty: Vec<String> = lock(&self.inner.dirty).drain().collect();
        for chat_id in dirty {
            let Ok(handle) = deps.doc_host.open(&chat_id) else {
                continue;
            };
            let Ok(entries) = handle.doc().read_entries() else {
                continue;
            };
            let records = turns::turn_records(&chat_id, &entries);
            self.with_conn(|conn| {
                let tx = conn.transaction()?;
                store::upsert(&tx, &records)?;
                tx.commit()
            })?;
        }
        Ok(())
    }

    /// Read what was appended to every agent log since the last pass.
    fn scan_logs(&self) -> Result<(), EngineError> {
        for (dir, source) in self.inner.roots.dirs() {
            for (path, size, mtime_ms) in logs::list_logs(&dir) {
                self.scan_log(&path, source, size, mtime_ms)?;
            }
        }
        Ok(())
    }

    fn scan_log(
        &self,
        path: &Path,
        source: Source,
        size: u64,
        mtime_ms: i64,
    ) -> Result<(), EngineError> {
        let key = path.to_string_lossy().into_owned();
        let mark = self.with_conn(|conn| store::file_mark(conn, &key))?;
        if mark
            .as_ref()
            .is_some_and(|m| m.offset == size && m.mtime_ms == mtime_ms)
        {
            return Ok(());
        }
        let mut from = mark.map(|m| (m.offset, m.guard, m.state));
        loop {
            let resume = from.as_ref().map(|(o, g, s)| (*o, *g, s.as_str()));
            let scan = match logs::scan_file(path, source, resume, size, CHUNK_BYTES) {
                Ok(scan) => scan,
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "usage log unreadable");
                    return Ok(());
                }
            };
            for chunk in scan.records.chunks(BATCH) {
                self.with_conn(|conn| {
                    let tx = conn.transaction()?;
                    store::upsert(&tx, chunk)?;
                    tx.commit()
                })?;
            }
            let done = scan.read == 0 || scan.offset >= size;
            // The mtime is only recorded once the file is read to its end,
            // so a pass cut short resumes the file next time.
            let mark = store::FileMark {
                offset: scan.offset,
                mtime_ms: if done { mtime_ms } else { 0 },
                guard: scan.guard,
                state: scan.state,
            };
            self.with_conn(|conn| store::set_file_mark(conn, &key, source, &mark))?;
            if done {
                return Ok(());
            }
            from = Some((mark.offset, mark.guard, mark.state));
        }
    }

    /// What the registry says about chats and projects right now.
    fn registry(&self) -> Registry {
        let Some(deps) = &self.inner.deps else {
            return Registry::default();
        };
        let spaces = deps.workspace.read_spaces().unwrap_or_default();
        let project_of = |space_id: Option<&str>| {
            let space = spaces.iter().find(|s| Some(s.id.as_str()) == space_id)?;
            Some((space.path.clone(), space.display_name().to_owned()))
        };
        let chats = deps
            .workspace
            .read_chats()
            .unwrap_or_default()
            .into_iter()
            .map(|chat| {
                let info = ChatInfo {
                    title: chat.title.clone(),
                    project: project_of(chat.space_id.as_deref()),
                    harness: chat.config.as_ref().and_then(|c| {
                        serde_json::to_value(c.harness)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                    }),
                    session_id: chat.harness_session_id.clone().filter(|s| !s.is_empty()),
                };
                (chat.id, info)
            })
            .collect();
        Registry {
            chats,
            projects: spaces
                .iter()
                .map(|s| (s.path.clone(), s.display_name().to_owned()))
                .collect(),
        }
    }

    /// Answer `request` at `now_ms` from the index. Pure apart from the
    /// read.
    fn query(
        &self,
        request: &UsageHistoryRequest,
        now_ms: i64,
        registry: &Registry,
    ) -> Result<UsageHistory, EngineError> {
        let tz = resolve_zone(request.time_zone.as_deref());
        let (first, turns_total, claude_files, codex_files) = self.with_conn(|conn| {
            Ok((
                store::first_at(conn)?,
                store::count_turns(conn)?,
                store::count_files(conn, Source::ClaudeLog)?,
                store::count_files(conn, Source::CodexLog)?,
            ))
        })?;
        let bounds = aggregate::bounds(request.range, &tz, now_ms, first);
        // A day either side: turns just outside the range still claim the
        // log lines inside it.
        let day = 24 * 60 * 60 * 1000;
        let records = self.with_conn(|conn| {
            store::records_between(
                conn,
                bounds.start().timestamp_millis() - day,
                bounds.end.timestamp_millis() + day,
            )
        })?;
        let mut history = aggregate::aggregate(request.range, &tz, &bounds, &records, registry);
        history.sources.godspeed_turns = turns_total;
        history.sources.claude_files = claude_files;
        history.sources.codex_files = codex_files;
        Ok(history)
    }
}

#[cfg(test)]
mod tests;
