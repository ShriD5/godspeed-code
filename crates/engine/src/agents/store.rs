//! `{data_dir}/agents.sqlite3` — agent definitions, per-trigger schedule
//! state and the run inbox. Device-scoped (like repos and worktree
//! settings): the device that hosts an agent is the one that fires it.
//!
//! Calls are short single-statement transactions on a local file, so they
//! run inline under a mutex rather than hopping to the blocking pool.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, TimeZone as _, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use zeron_proto::agents::{AgentDraft, AgentRun, AgentRunFilter, AgentRunStatus, AgentTriggerKind};

use super::schedule::TriggerState;
use crate::EngineError;

const FILE_NAME: &str = "agents.sqlite3";
/// Inbox rows kept per agent; older ones go on insert.
const MAX_RUNS_PER_AGENT: i64 = 1000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredAgent {
    pub id: String,
    pub draft: AgentDraft,
    pub workspace_path: String,
    pub webhook_token: String,
    pub consecutive_failures: u32,
    pub paused_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(crate) struct AgentStore {
    conn: Mutex<Connection>,
}

fn store_err(err: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("agents store: {err}"))
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn from_ms(v: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(v).single().unwrap_or_default()
}

fn enum_text<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn enum_parse<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).ok()
}

const RUN_COLUMNS: &str = "id, agent_id, agent_name, trigger, scheduled_at, started_at, \
     finished_at, status, summary, chat_id, collapsed, read, archived, created_at";

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<AgentRun> {
    let trigger: String = row.get(3)?;
    let status: String = row.get(7)?;
    Ok(AgentRun {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        agent_name: row.get(2)?,
        trigger: enum_parse(&trigger).unwrap_or(AgentTriggerKind::Manual),
        scheduled_at: from_ms(row.get(4)?),
        started_at: row.get::<_, Option<i64>>(5)?.map(from_ms),
        finished_at: row.get::<_, Option<i64>>(6)?.map(from_ms),
        status: enum_parse(&status).unwrap_or(AgentRunStatus::Failed),
        summary: row.get(8)?,
        chat_id: row.get(9)?,
        collapsed: row.get::<_, i64>(10)?.try_into().unwrap_or(0),
        read: row.get(11)?,
        archived: row.get(12)?,
        created_at: from_ms(row.get(13)?),
    })
}

fn agent_from_row(row: &Row<'_>) -> rusqlite::Result<(String, String, StoredAgentParts)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        StoredAgentParts {
            workspace_path: row.get(2)?,
            webhook_token: row.get(3)?,
            consecutive_failures: row.get::<_, i64>(4)?.try_into().unwrap_or(0),
            paused_reason: row.get(5)?,
            created_at: from_ms(row.get(6)?),
            updated_at: from_ms(row.get(7)?),
        },
    ))
}

struct StoredAgentParts {
    workspace_path: String,
    webhook_token: String,
    consecutive_failures: u32,
    paused_reason: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

const AGENT_COLUMNS: &str = "id, draft, workspace_path, webhook_token, consecutive_failures, \
     paused_reason, created_at, updated_at";

/// Outcome of [`AgentStore::insert_run`].
pub(crate) enum Inserted {
    New(AgentRun),
    /// The run key was already taken: this fire was handled before.
    Existing(AgentRun),
}

impl AgentStore {
    pub(crate) fn open(data_dir: &Path) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join(FILE_NAME)).map_err(store_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS agents (
                 id TEXT PRIMARY KEY,
                 draft TEXT NOT NULL,
                 workspace_path TEXT NOT NULL,
                 webhook_token TEXT NOT NULL,
                 consecutive_failures INTEGER NOT NULL DEFAULT 0,
                 paused_reason TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS triggers (
                 agent_id TEXT NOT NULL,
                 trigger_key TEXT NOT NULL,
                 anchor_at INTEGER NOT NULL,
                 last_scheduled_at INTEGER,
                 PRIMARY KEY (agent_id, trigger_key)
             );
             CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY,
                 run_key TEXT NOT NULL UNIQUE,
                 agent_id TEXT NOT NULL,
                 agent_name TEXT NOT NULL,
                 trigger TEXT NOT NULL,
                 scheduled_at INTEGER NOT NULL,
                 started_at INTEGER,
                 finished_at INTEGER,
                 status TEXT NOT NULL,
                 summary TEXT,
                 chat_id TEXT,
                 collapsed INTEGER NOT NULL DEFAULT 0,
                 read INTEGER NOT NULL DEFAULT 0,
                 archived INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS runs_by_agent ON runs (agent_id, created_at DESC);
             CREATE INDEX IF NOT EXISTS runs_by_chat ON runs (chat_id);
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(store_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ── agents ────────────────────────────────────────────────────────────

    pub(crate) fn agents(&self) -> Result<Vec<StoredAgent>, EngineError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {AGENT_COLUMNS} FROM agents ORDER BY created_at"
            ))
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], agent_from_row)
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        Ok(rows.into_iter().filter_map(assemble_agent).collect())
    }

    pub(crate) fn agent(&self, id: &str) -> Result<Option<StoredAgent>, EngineError> {
        let row = self
            .conn()
            .query_row(
                &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE id = ?1"),
                params![id],
                agent_from_row,
            )
            .optional()
            .map_err(store_err)?;
        Ok(row.and_then(assemble_agent))
    }

    pub(crate) fn put_agent(&self, agent: &StoredAgent) -> Result<(), EngineError> {
        let draft =
            serde_json::to_string(&agent.draft).map_err(|e| EngineError::Other(e.to_string()))?;
        self.conn()
            .execute(
                "INSERT INTO agents (id, draft, workspace_path, webhook_token,
                     consecutive_failures, paused_reason, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (id) DO UPDATE SET draft = ?2, workspace_path = ?3,
                     webhook_token = ?4, consecutive_failures = ?5, paused_reason = ?6,
                     updated_at = ?8",
                params![
                    agent.id,
                    draft,
                    agent.workspace_path,
                    agent.webhook_token,
                    agent.consecutive_failures,
                    agent.paused_reason,
                    ms(agent.created_at),
                    ms(agent.updated_at),
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    pub(crate) fn delete_agent(&self, id: &str) -> Result<bool, EngineError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(store_err)?;
        let deleted = tx
            .execute("DELETE FROM agents WHERE id = ?1", params![id])
            .map_err(store_err)?;
        tx.execute("DELETE FROM triggers WHERE agent_id = ?1", params![id])
            .map_err(store_err)?;
        tx.execute("DELETE FROM runs WHERE agent_id = ?1", params![id])
            .map_err(store_err)?;
        tx.commit().map_err(store_err)?;
        Ok(deleted > 0)
    }

    /// Count a finished run against the agent: success resets the streak,
    /// failure extends it. Returns the new streak.
    pub(crate) fn record_outcome(&self, id: &str, failed: bool) -> Result<u32, EngineError> {
        let conn = self.conn();
        if failed {
            conn.execute(
                "UPDATE agents SET consecutive_failures = consecutive_failures + 1 WHERE id = ?1",
                params![id],
            )
        } else {
            conn.execute(
                "UPDATE agents SET consecutive_failures = 0 WHERE id = ?1",
                params![id],
            )
        }
        .map_err(store_err)?;
        let streak: Option<i64> = conn
            .query_row(
                "SELECT consecutive_failures FROM agents WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        Ok(streak.unwrap_or(0).try_into().unwrap_or(0))
    }

    // ── install identity ──────────────────────────────────────────────────

    /// The install that last owned this database (its device id), if stamped.
    pub(crate) fn install_id(&self) -> Result<Option<String>, EngineError> {
        self.conn()
            .query_row(
                "SELECT value FROM meta WHERE key = 'install_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)
    }

    pub(crate) fn set_install_id(&self, id: &str) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "INSERT INTO meta (key, value) VALUES ('install_id', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![id],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// Until when the agent's fires wait for a usage-limit reset.
    pub(crate) fn limited_until(&self, id: &str) -> Result<Option<DateTime<Utc>>, EngineError> {
        let value: Option<String> = self
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![format!("limited_until:{id}")],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        Ok(value.and_then(|v| v.parse::<i64>().ok()).map(from_ms))
    }

    pub(crate) fn set_limited_until(
        &self,
        id: &str,
        until: Option<DateTime<Utc>>,
    ) -> Result<(), EngineError> {
        let key = format!("limited_until:{id}");
        let conn = self.conn();
        match until {
            Some(until) => conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, ms(until).to_string()],
            ),
            None => conn.execute("DELETE FROM meta WHERE key = ?1", params![key]),
        }
        .map_err(store_err)?;
        Ok(())
    }

    pub(crate) fn set_paused(&self, id: &str, reason: Option<&str>) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "UPDATE agents SET paused_reason = ?2 WHERE id = ?1",
                params![id, reason],
            )
            .map_err(store_err)?;
        Ok(())
    }

    // ── trigger state ─────────────────────────────────────────────────────

    pub(crate) fn trigger_state(
        &self,
        agent_id: &str,
        key: &str,
    ) -> Result<Option<TriggerState>, EngineError> {
        self.conn()
            .query_row(
                "SELECT anchor_at, last_scheduled_at FROM triggers
                 WHERE agent_id = ?1 AND trigger_key = ?2",
                params![agent_id, key],
                |row| {
                    Ok(TriggerState {
                        anchor: from_ms(row.get(0)?),
                        last_scheduled: row.get::<_, Option<i64>>(1)?.map(from_ms),
                    })
                },
            )
            .optional()
            .map_err(store_err)
    }

    pub(crate) fn put_trigger_state(
        &self,
        agent_id: &str,
        key: &str,
        state: &TriggerState,
    ) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "INSERT INTO triggers (agent_id, trigger_key, anchor_at, last_scheduled_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (agent_id, trigger_key)
                 DO UPDATE SET anchor_at = ?3, last_scheduled_at = ?4",
                params![
                    agent_id,
                    key,
                    ms(state.anchor),
                    state.last_scheduled.map(ms)
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// Drop state for triggers the agent no longer has (`keep` = current keys).
    /// An empty `keep` resets every trigger (re-enable starts a fresh count).
    pub(crate) fn retain_triggers(
        &self,
        agent_id: &str,
        keep: &[String],
    ) -> Result<(), EngineError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT trigger_key FROM triggers WHERE agent_id = ?1")
            .map_err(store_err)?;
        let existing = stmt
            .query_map(params![agent_id], |row| row.get::<_, String>(0))
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        for key in existing.iter().filter(|key| !keep.contains(key)) {
            conn.execute(
                "DELETE FROM triggers WHERE agent_id = ?1 AND trigger_key = ?2",
                params![agent_id, key],
            )
            .map_err(store_err)?;
        }
        Ok(())
    }

    // ── runs ──────────────────────────────────────────────────────────────

    /// Insert a run unless its idempotency key was seen before.
    pub(crate) fn insert_run(
        &self,
        run_key: &str,
        run: &AgentRun,
    ) -> Result<Inserted, EngineError> {
        let conn = self.conn();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO runs (id, run_key, agent_id, agent_name, trigger,
                     scheduled_at, started_at, finished_at, status, summary, chat_id,
                     collapsed, read, archived, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    run.id,
                    run_key,
                    run.agent_id,
                    run.agent_name,
                    enum_text(run.trigger),
                    ms(run.scheduled_at),
                    run.started_at.map(ms),
                    run.finished_at.map(ms),
                    enum_text(run.status),
                    run.summary,
                    run.chat_id,
                    run.collapsed,
                    run.read,
                    run.archived,
                    ms(run.created_at),
                ],
            )
            .map_err(store_err)?;
        if inserted == 0 {
            let existing = conn
                .query_row(
                    &format!("SELECT {RUN_COLUMNS} FROM runs WHERE run_key = ?1"),
                    params![run_key],
                    run_from_row,
                )
                .map_err(store_err)?;
            return Ok(Inserted::Existing(existing));
        }
        conn.execute(
            "DELETE FROM runs WHERE agent_id = ?1 AND id NOT IN (
                 SELECT id FROM runs WHERE agent_id = ?1 ORDER BY created_at DESC LIMIT ?2)",
            params![run.agent_id, MAX_RUNS_PER_AGENT],
        )
        .map_err(store_err)?;
        Ok(Inserted::New(run.clone()))
    }

    pub(crate) fn update_run(&self, run: &AgentRun) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "UPDATE runs SET started_at = ?2, finished_at = ?3, status = ?4, summary = ?5,
                     chat_id = ?6, collapsed = ?7, scheduled_at = ?8, read = ?9
                 WHERE id = ?1",
                params![
                    run.id,
                    run.started_at.map(ms),
                    run.finished_at.map(ms),
                    enum_text(run.status),
                    run.summary,
                    run.chat_id,
                    run.collapsed,
                    ms(run.scheduled_at),
                    run.read,
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// The agent's queued / running / needs-approval run, if any.
    pub(crate) fn active_run(&self, agent_id: &str) -> Result<Option<AgentRun>, EngineError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs WHERE agent_id = ?1
                     AND status IN ('queued', 'running', 'needs-approval')
                     ORDER BY created_at DESC LIMIT 1"
                ),
                params![agent_id],
                run_from_row,
            )
            .optional()
            .map_err(store_err)
    }

    pub(crate) fn latest_run(&self, agent_id: &str) -> Result<Option<AgentRun>, EngineError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs WHERE agent_id = ?1
                     ORDER BY created_at DESC LIMIT 1"
                ),
                params![agent_id],
                run_from_row,
            )
            .optional()
            .map_err(store_err)
    }

    /// Every run left active by a previous engine process.
    pub(crate) fn active_runs(&self) -> Result<Vec<AgentRun>, EngineError> {
        self.list_runs(&AgentRunFilter {
            statuses: vec![
                AgentRunStatus::Queued,
                AgentRunStatus::Running,
                AgentRunStatus::NeedsApproval,
            ],
            include_archived: true,
            limit: Some(500),
            ..Default::default()
        })
    }

    pub(crate) fn list_runs(&self, filter: &AgentRunFilter) -> Result<Vec<AgentRun>, EngineError> {
        let mut sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE 1 = 1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(agent_id) = &filter.agent_id {
            args.push(Box::new(agent_id.clone()));
            sql.push_str(&format!(" AND agent_id = ?{}", args.len()));
        }
        if let Some(chat_id) = &filter.chat_id {
            args.push(Box::new(chat_id.clone()));
            sql.push_str(&format!(" AND chat_id = ?{}", args.len()));
        }
        if !filter.statuses.is_empty() {
            let mut slots = Vec::new();
            for status in &filter.statuses {
                args.push(Box::new(enum_text(*status)));
                slots.push(format!("?{}", args.len()));
            }
            sql.push_str(&format!(" AND status IN ({})", slots.join(", ")));
        }
        if filter.unread_only {
            sql.push_str(" AND read = 0");
        }
        if !filter.include_archived {
            sql.push_str(" AND archived = 0");
        }
        let limit = filter.limit.unwrap_or(100).clamp(1, 500);
        sql.push_str(&format!(" ORDER BY created_at DESC LIMIT {limit}"));
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(args.iter().map(|arg| arg.as_ref())),
                run_from_row,
            )
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    pub(crate) fn mark_runs(
        &self,
        ids: &[String],
        read: Option<bool>,
        archived: Option<bool>,
    ) -> Result<usize, EngineError> {
        let conn = self.conn();
        let mut changed = 0;
        for id in ids {
            if let Some(read) = read {
                changed += conn
                    .execute(
                        "UPDATE runs SET read = ?2 WHERE id = ?1 AND read != ?2",
                        params![id, read],
                    )
                    .map_err(store_err)?;
            }
            if let Some(archived) = archived {
                changed += conn
                    .execute(
                        "UPDATE runs SET archived = ?2 WHERE id = ?1 AND archived != ?2",
                        params![id, archived],
                    )
                    .map_err(store_err)?;
            }
        }
        Ok(changed)
    }

    /// (unread, needs-approval) counts over unarchived runs.
    pub(crate) fn inbox_counts(&self) -> Result<(u32, u32), EngineError> {
        self.conn()
            .query_row(
                "SELECT
                     COALESCE(SUM(read = 0), 0),
                     COALESCE(SUM(status = 'needs-approval'), 0)
                 FROM runs WHERE archived = 0",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?.try_into().unwrap_or(0),
                        row.get::<_, i64>(1)?.try_into().unwrap_or(0),
                    ))
                },
            )
            .map_err(store_err)
    }
}

fn assemble_agent((id, draft, parts): (String, String, StoredAgentParts)) -> Option<StoredAgent> {
    // A row this build can't read (newer trigger kind) is skipped, not fatal.
    let draft = match serde_json::from_str::<AgentDraft>(&draft) {
        Ok(draft) => draft,
        Err(err) => {
            tracing::warn!(agent = %id, %err, "agents: unreadable definition skipped");
            return None;
        }
    };
    Some(StoredAgent {
        id,
        draft,
        workspace_path: parts.workspace_path,
        webhook_token: parts.webhook_token,
        consecutive_failures: parts.consecutive_failures,
        paused_reason: parts.paused_reason,
        created_at: parts.created_at,
        updated_at: parts.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, agent: &str, status: AgentRunStatus, at: i64) -> AgentRun {
        AgentRun {
            id: id.into(),
            agent_id: agent.into(),
            agent_name: "A".into(),
            trigger: AgentTriggerKind::Cron,
            scheduled_at: from_ms(at),
            started_at: None,
            finished_at: None,
            status,
            summary: None,
            chat_id: None,
            collapsed: 0,
            read: false,
            archived: false,
            created_at: from_ms(at),
        }
    }

    #[test]
    fn run_keys_are_idempotent_and_filters_apply() {
        let dir = tempfile::tempdir().unwrap();
        let store = AgentStore::open(dir.path()).unwrap();
        let first = run("r1", "a", AgentRunStatus::Queued, 1_000);
        assert!(matches!(
            store.insert_run("a:t:1000", &first).unwrap(),
            Inserted::New(_)
        ));
        let dup = run("r2", "a", AgentRunStatus::Queued, 1_000);
        match store.insert_run("a:t:1000", &dup).unwrap() {
            Inserted::Existing(existing) => assert_eq!(existing.id, "r1"),
            Inserted::New(_) => panic!("same key must not insert twice"),
        }
        store
            .insert_run("a:t:2000", &run("r3", "a", AgentRunStatus::Skipped, 2_000))
            .unwrap();
        store
            .insert_run("b:t:3000", &run("r4", "b", AgentRunStatus::Failed, 3_000))
            .unwrap();
        assert_eq!(store.active_run("a").unwrap().unwrap().id, "r1");
        assert!(store.active_run("b").unwrap().is_none());

        let ids = |filter: AgentRunFilter| -> Vec<String> {
            store
                .list_runs(&filter)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(ids(AgentRunFilter::default()), ["r4", "r3", "r1"]);
        assert_eq!(
            ids(AgentRunFilter {
                agent_id: Some("a".into()),
                statuses: vec![AgentRunStatus::Skipped],
                ..Default::default()
            }),
            ["r3"]
        );
        assert_eq!(
            store
                .mark_runs(&["r3".into(), "r4".into()], Some(true), Some(true))
                .unwrap(),
            4
        );
        assert_eq!(
            ids(AgentRunFilter {
                unread_only: true,
                ..Default::default()
            }),
            ["r1"]
        );
        assert_eq!(store.inbox_counts().unwrap(), (1, 0));
        assert!(store.delete_agent("a").is_ok());
        assert_eq!(
            ids(AgentRunFilter {
                include_archived: true,
                ..Default::default()
            }),
            ["r4"]
        );
    }
}
