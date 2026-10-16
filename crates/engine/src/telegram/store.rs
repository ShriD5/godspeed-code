//! `{data_dir}/telegram.sqlite3`: linked chats and their defaults, which
//! Godspeed chats belong to which Telegram chat, replies still owed,
//! approval cards, schedules made from Telegram, and the poll offset.
//! Device-scoped, like the agents store next door.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, TimeZone as _, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use zeron_proto::telegram::TelegramDefaults;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

use crate::EngineError;

const FILE_NAME: &str = "telegram.sqlite3";
const OFFSET_KEY: &str = "update_offset";
const DEFAULTS_KEY: &str = "defaults";

fn store_err(err: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("telegram store: {err}"))
}

fn json_err(err: serde_json::Error) -> EngineError {
    EngineError::Other(format!("telegram store: {err}"))
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn from_ms(v: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(v).single().unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LinkedChat {
    pub chat_id: i64,
    pub label: String,
    pub linked_at: DateTime<Utc>,
    pub timezone: String,
    pub defaults: TelegramDefaults,
    /// The Godspeed chat plain text continues.
    pub thread_id: Option<String>,
}

/// A message sent from Telegram whose reply is still owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingTurn {
    /// The user entry's id in the Godspeed chat.
    pub message_id: String,
    pub chat_id: String,
    pub tg_chat: i64,
    /// The Telegram message the reply answers.
    pub reply_to: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptState {
    Pending,
    Answered,
    /// Resolved somewhere else (the app, another chat) or abandoned.
    Closed,
}

impl PromptState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Closed => "closed",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "pending" => Self::Pending,
            "answered" => Self::Answered,
            _ => Self::Closed,
        }
    }
}

/// An approval or question card sent to one Telegram chat.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Prompt {
    pub id: i64,
    pub tg_chat: i64,
    pub message_id: Option<i64>,
    pub chat_id: String,
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    /// Answers so far, one per question already answered (in order).
    pub answers: Vec<UserInputAnswer>,
    pub state: PromptState,
    /// The card's text as sent, for appending outcomes.
    pub card: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScheduleRow {
    pub id: i64,
    pub agent_id: String,
    pub tg_chat: i64,
    pub label: String,
    pub created_at: DateTime<Utc>,
}

pub(crate) struct TelegramStore {
    conn: Mutex<Connection>,
}

const LINK_COLUMNS: &str = "chat_id, label, linked_at, timezone, defaults, thread_id";
const PROMPT_COLUMNS: &str =
    "id, tg_chat, message_id, chat_id, request_id, questions, answers, state, card";
const SCHEDULE_COLUMNS: &str = "id, agent_id, tg_chat, label, created_at";

fn link_from_row(row: &Row<'_>) -> rusqlite::Result<LinkedChat> {
    let defaults: String = row.get(4)?;
    Ok(LinkedChat {
        chat_id: row.get(0)?,
        label: row.get(1)?,
        linked_at: from_ms(row.get(2)?),
        timezone: row.get(3)?,
        defaults: serde_json::from_str(&defaults).unwrap_or_default(),
        thread_id: row.get(5)?,
    })
}

fn prompt_from_row(row: &Row<'_>) -> rusqlite::Result<Prompt> {
    let questions: String = row.get(5)?;
    let answers: String = row.get(6)?;
    let state: String = row.get(7)?;
    Ok(Prompt {
        id: row.get(0)?,
        tg_chat: row.get(1)?,
        message_id: row.get(2)?,
        chat_id: row.get(3)?,
        request_id: row.get(4)?,
        questions: serde_json::from_str(&questions).unwrap_or_default(),
        answers: serde_json::from_str(&answers).unwrap_or_default(),
        state: PromptState::parse(&state),
        card: row.get(8)?,
    })
}

fn schedule_from_row(row: &Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        tg_chat: row.get(2)?,
        label: row.get(3)?,
        created_at: from_ms(row.get(4)?),
    })
}

impl TelegramStore {
    pub fn open(data_dir: &Path) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join(FILE_NAME)).map_err(store_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS links (
                 chat_id INTEGER PRIMARY KEY,
                 label TEXT NOT NULL,
                 linked_at INTEGER NOT NULL,
                 timezone TEXT NOT NULL,
                 defaults TEXT NOT NULL,
                 thread_id TEXT
             );
             CREATE TABLE IF NOT EXISTS threads (
                 chat_id TEXT PRIMARY KEY,
                 tg_chat INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS turns (
                 message_id TEXT PRIMARY KEY,
                 chat_id TEXT NOT NULL,
                 tg_chat INTEGER NOT NULL,
                 reply_to INTEGER,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS bot_messages (
                 tg_chat INTEGER NOT NULL,
                 message_id INTEGER NOT NULL,
                 chat_id TEXT NOT NULL,
                 PRIMARY KEY (tg_chat, message_id)
             );
             CREATE TABLE IF NOT EXISTS prompts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 tg_chat INTEGER NOT NULL,
                 message_id INTEGER,
                 chat_id TEXT NOT NULL,
                 request_id TEXT NOT NULL,
                 questions TEXT NOT NULL,
                 answers TEXT NOT NULL DEFAULT '[]',
                 state TEXT NOT NULL,
                 card TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL,
                 UNIQUE (tg_chat, chat_id, request_id)
             );
             CREATE TABLE IF NOT EXISTS schedules (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 agent_id TEXT NOT NULL UNIQUE,
                 tg_chat INTEGER NOT NULL,
                 label TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS notified_runs (
                 run_id TEXT PRIMARY KEY,
                 notified_at INTEGER NOT NULL
             );
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

    fn meta(&self, key: &str) -> Result<Option<String>, EngineError> {
        self.conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(store_err)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = ?2",
                params![key, value],
            )
            .map_err(store_err)?;
        Ok(())
    }

    // ── poll offset + global defaults ─────────────────────────────────────

    pub fn update_offset(&self) -> Result<i64, EngineError> {
        Ok(self
            .meta(OFFSET_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub fn set_update_offset(&self, offset: i64) -> Result<(), EngineError> {
        self.set_meta(OFFSET_KEY, &offset.to_string())
    }

    pub fn global_defaults(&self) -> Result<TelegramDefaults, EngineError> {
        Ok(self
            .meta(DEFAULTS_KEY)?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default())
    }

    pub fn set_global_defaults(&self, defaults: &TelegramDefaults) -> Result<(), EngineError> {
        self.set_meta(
            DEFAULTS_KEY,
            &serde_json::to_string(defaults).map_err(json_err)?,
        )
    }

    // ── links ─────────────────────────────────────────────────────────────

    pub fn link(&self, chat: &LinkedChat) -> Result<(), EngineError> {
        let defaults = serde_json::to_string(&chat.defaults).map_err(json_err)?;
        self.conn()
            .execute(
                "INSERT INTO links (chat_id, label, linked_at, timezone, defaults, thread_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)
                 ON CONFLICT (chat_id) DO UPDATE SET label = ?2",
                params![
                    chat.chat_id,
                    chat.label,
                    ms(chat.linked_at),
                    chat.timezone,
                    defaults
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// Forget the chat and what it still owes; its schedules (ordinary
    /// agents) keep running and come back if the chat links again.
    pub fn unlink(&self, chat_id: i64) -> Result<bool, EngineError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(store_err)?;
        let removed = tx
            .execute("DELETE FROM links WHERE chat_id = ?1", params![chat_id])
            .map_err(store_err)?;
        tx.execute("DELETE FROM turns WHERE tg_chat = ?1", params![chat_id])
            .map_err(store_err)?;
        tx.execute(
            "UPDATE prompts SET state = 'closed' WHERE tg_chat = ?1 AND state = 'pending'",
            params![chat_id],
        )
        .map_err(store_err)?;
        tx.commit().map_err(store_err)?;
        Ok(removed > 0)
    }

    pub fn linked(&self, chat_id: i64) -> Result<Option<LinkedChat>, EngineError> {
        self.conn()
            .query_row(
                &format!("SELECT {LINK_COLUMNS} FROM links WHERE chat_id = ?1"),
                params![chat_id],
                link_from_row,
            )
            .optional()
            .map_err(store_err)
    }

    pub fn linked_chats(&self) -> Result<Vec<LinkedChat>, EngineError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {LINK_COLUMNS} FROM links ORDER BY linked_at"
            ))
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], link_from_row)
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    pub fn set_defaults(
        &self,
        chat_id: i64,
        defaults: &TelegramDefaults,
    ) -> Result<(), EngineError> {
        let defaults = serde_json::to_string(defaults).map_err(json_err)?;
        self.conn()
            .execute(
                "UPDATE links SET defaults = ?2 WHERE chat_id = ?1",
                params![chat_id, defaults],
            )
            .map_err(store_err)?;
        Ok(())
    }

    pub fn set_timezone(&self, chat_id: i64, timezone: &str) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "UPDATE links SET timezone = ?2 WHERE chat_id = ?1",
                params![chat_id, timezone],
            )
            .map_err(store_err)?;
        Ok(())
    }

    pub fn set_thread(&self, tg_chat: i64, thread: Option<&str>) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "UPDATE links SET thread_id = ?2 WHERE chat_id = ?1",
                params![tg_chat, thread],
            )
            .map_err(store_err)?;
        Ok(())
    }

    // ── threads + reply routing ───────────────────────────────────────────

    pub fn add_thread(
        &self,
        chat_id: &str,
        tg_chat: i64,
        now: DateTime<Utc>,
    ) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "INSERT INTO threads (chat_id, tg_chat, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (chat_id) DO NOTHING",
                params![chat_id, tg_chat, ms(now)],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// The Telegram chat a Godspeed chat was started from.
    pub fn thread_owner(&self, chat_id: &str) -> Result<Option<i64>, EngineError> {
        self.conn()
            .query_row(
                "SELECT tg_chat FROM threads WHERE chat_id = ?1",
                params![chat_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)
    }

    pub fn add_turn(&self, turn: &PendingTurn, now: DateTime<Utc>) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "INSERT INTO turns (message_id, chat_id, tg_chat, reply_to, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (message_id) DO NOTHING",
                params![
                    turn.message_id,
                    turn.chat_id,
                    turn.tg_chat,
                    turn.reply_to,
                    ms(now)
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    pub fn pending_turns(&self) -> Result<Vec<PendingTurn>, EngineError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT message_id, chat_id, tg_chat, reply_to FROM turns ORDER BY created_at")
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PendingTurn {
                    message_id: r.get(0)?,
                    chat_id: r.get(1)?,
                    tg_chat: r.get(2)?,
                    reply_to: r.get(3)?,
                })
            })
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    pub fn remove_turn(&self, message_id: &str) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "DELETE FROM turns WHERE message_id = ?1",
                params![message_id],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// Replying to this bot message continues `chat_id`.
    pub fn remember_message(
        &self,
        tg_chat: i64,
        message_id: i64,
        chat_id: &str,
    ) -> Result<(), EngineError> {
        self.conn()
            .execute(
                "INSERT INTO bot_messages (tg_chat, message_id, chat_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT (tg_chat, message_id) DO UPDATE SET chat_id = ?3",
                params![tg_chat, message_id, chat_id],
            )
            .map_err(store_err)?;
        Ok(())
    }

    pub fn chat_for_message(
        &self,
        tg_chat: i64,
        message_id: i64,
    ) -> Result<Option<String>, EngineError> {
        self.conn()
            .query_row(
                "SELECT chat_id FROM bot_messages WHERE tg_chat = ?1 AND message_id = ?2",
                params![tg_chat, message_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)
    }

    // ── approval / question cards ─────────────────────────────────────────

    /// Record a card about to be sent; `None` when this chat already has
    /// one for the request.
    pub fn insert_prompt(
        &self,
        tg_chat: i64,
        chat_id: &str,
        request_id: &str,
        questions: &[UserInputQuestion],
        now: DateTime<Utc>,
    ) -> Result<Option<i64>, EngineError> {
        let questions = serde_json::to_string(questions).map_err(json_err)?;
        let conn = self.conn();
        let inserted = conn
            .execute(
                "INSERT INTO prompts (tg_chat, chat_id, request_id, questions, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5)
                 ON CONFLICT (tg_chat, chat_id, request_id) DO NOTHING",
                params![tg_chat, chat_id, request_id, questions, ms(now)],
            )
            .map_err(store_err)?;
        Ok((inserted > 0).then(|| conn.last_insert_rowid()))
    }

    pub fn prompt(&self, id: i64) -> Result<Option<Prompt>, EngineError> {
        self.conn()
            .query_row(
                &format!("SELECT {PROMPT_COLUMNS} FROM prompts WHERE id = ?1"),
                params![id],
                prompt_from_row,
            )
            .optional()
            .map_err(store_err)
    }

    pub fn prompt_for_message(
        &self,
        tg_chat: i64,
        message_id: i64,
    ) -> Result<Option<Prompt>, EngineError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {PROMPT_COLUMNS} FROM prompts WHERE tg_chat = ?1 AND message_id = ?2"
                ),
                params![tg_chat, message_id],
                prompt_from_row,
            )
            .optional()
            .map_err(store_err)
    }

    pub fn pending_prompts(&self) -> Result<Vec<Prompt>, EngineError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {PROMPT_COLUMNS} FROM prompts WHERE state = 'pending' ORDER BY id"
            ))
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], prompt_from_row)
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    pub fn save_prompt(&self, prompt: &Prompt) -> Result<(), EngineError> {
        let answers = serde_json::to_string(&prompt.answers).map_err(json_err)?;
        self.conn()
            .execute(
                "UPDATE prompts SET message_id = ?2, answers = ?3, state = ?4, card = ?5
                 WHERE id = ?1",
                params![
                    prompt.id,
                    prompt.message_id,
                    answers,
                    prompt.state.as_str(),
                    prompt.card
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    // ── schedules ─────────────────────────────────────────────────────────

    pub fn add_schedule(
        &self,
        agent_id: &str,
        tg_chat: i64,
        label: &str,
        now: DateTime<Utc>,
    ) -> Result<ScheduleRow, EngineError> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO schedules (agent_id, tg_chat, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![agent_id, tg_chat, label, ms(now)],
        )
        .map_err(store_err)?;
        Ok(ScheduleRow {
            id: conn.last_insert_rowid(),
            agent_id: agent_id.to_string(),
            tg_chat,
            label: label.to_string(),
            created_at: from_ms(ms(now)),
        })
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>, EngineError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedules ORDER BY id"
            ))
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], schedule_from_row)
            .map_err(store_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    pub fn schedule_for_agent(&self, agent_id: &str) -> Result<Option<ScheduleRow>, EngineError> {
        self.conn()
            .query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE agent_id = ?1"),
                params![agent_id],
                schedule_from_row,
            )
            .optional()
            .map_err(store_err)
    }

    pub fn delete_schedule(&self, id: i64) -> Result<(), EngineError> {
        self.conn()
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])
            .map_err(store_err)?;
        Ok(())
    }

    /// `true` the first time a run is recorded as notified.
    pub fn mark_notified(&self, run_id: &str, now: DateTime<Utc>) -> Result<bool, EngineError> {
        let inserted = self
            .conn()
            .execute(
                "INSERT INTO notified_runs (run_id, notified_at) VALUES (?1, ?2)
                 ON CONFLICT (run_id) DO NOTHING",
                params![run_id, ms(now)],
            )
            .map_err(store_err)?;
        Ok(inserted > 0)
    }

    pub fn is_notified(&self, run_id: &str) -> Result<bool, EngineError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM notified_runs WHERE run_id = ?1",
                params![run_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(store_err)?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: i64) -> LinkedChat {
        LinkedChat {
            chat_id: id,
            label: "@me".into(),
            linked_at: Utc::now(),
            timezone: "UTC".into(),
            defaults: TelegramDefaults::default(),
            thread_id: None,
        }
    }

    #[test]
    fn links_threads_and_reply_routes_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TelegramStore::open(tmp.path()).unwrap();
        store.link(&chat(10)).unwrap();
        store.set_thread(10, Some("c1")).unwrap();
        store.remember_message(10, 99, "c1").unwrap();
        drop(store);
        let store = TelegramStore::open(tmp.path()).unwrap();
        assert_eq!(
            store.linked(10).unwrap().unwrap().thread_id.as_deref(),
            Some("c1")
        );
        assert_eq!(
            store.chat_for_message(10, 99).unwrap().as_deref(),
            Some("c1")
        );
        assert!(store.unlink(10).unwrap());
        assert!(store.linked(10).unwrap().is_none());
        assert!(!store.unlink(10).unwrap());
    }

    #[test]
    fn prompts_are_one_per_chat_and_request() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TelegramStore::open(tmp.path()).unwrap();
        let now = Utc::now();
        let id = store.insert_prompt(10, "c1", "r1", &[], now).unwrap();
        assert!(id.is_some());
        assert!(
            store
                .insert_prompt(10, "c1", "r1", &[], now)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .insert_prompt(11, "c1", "r1", &[], now)
                .unwrap()
                .is_some()
        );
        let mut prompt = store.prompt(id.unwrap()).unwrap().unwrap();
        prompt.state = PromptState::Answered;
        prompt.message_id = Some(5);
        store.save_prompt(&prompt).unwrap();
        assert_eq!(
            store.prompt_for_message(10, 5).unwrap().unwrap().state,
            PromptState::Answered
        );
        assert_eq!(store.pending_prompts().unwrap().len(), 1);
    }

    #[test]
    fn runs_are_notified_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TelegramStore::open(tmp.path()).unwrap();
        assert!(store.mark_notified("run", Utc::now()).unwrap());
        assert!(!store.mark_notified("run", Utc::now()).unwrap());
        assert!(store.is_notified("run").unwrap());
    }
}
