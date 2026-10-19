//! Claude Code and Codex transcript logs as usage records, read the way
//! T3 Code's usage page reads them (`claudeUsage.ts`, `codexUsage.ts`).
//!
//! - **Claude Code** (`<config>/projects/**/*.jsonl`): every `assistant`
//!   line with `message.usage`. Claude Code repeats a message's usage on
//!   each content block, and resumed or forked sessions copy lines into new
//!   files, so a record is keyed by `message.id` + `requestId` and the store
//!   keeps one row per key.
//! - **Codex** (`<home>/sessions/**/*.jsonl`, and `archived_sessions`):
//!   `token_count` events' `last_token_usage`, with the model carried from
//!   the latest `turn_context` and the session from the first
//!   `session_meta`. Identical consecutive events are re-emissions, and a
//!   forked or subagent rollout opens with its parent's history re-stamped
//!   in one burst; both are skipped. Codex's `input_tokens` include the
//!   cached ones, which are split out.
//!
//! Files are only ever read. Each keeps a read position (and the parser's
//! carried state) so a re-scan reads only what was appended; a file that
//! shrank or changed under its position is read again from the start.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeron_proto::UsageTokens;

use super::{Source, UsageRecord};

/// Bytes before the read position hashed to notice a rewritten file.
const GUARD_BYTES: u64 = 64;
/// Directory depth walked under a log root (Codex nests `YYYY/MM/DD`,
/// Claude Code `project/session/subagents`).
const MAX_DEPTH: usize = 6;
/// A forked rollout's copied history is written in one burst (T3 observed
/// gaps under 40 ms); the child's first real usage lands seconds later.
const FORK_COPY_MAX_GAP_MS: i64 = 1_000;

pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

fn timestamp_ms(value: Option<&Value>) -> Option<i64> {
    let raw = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.timestamp_millis())
}

fn count(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

/// One Claude Code transcript line as a record. `fallback_key` keys a line
/// that carries neither a message id nor a request id.
pub(super) fn parse_claude_line(line: &str, fallback_key: &str) -> Option<UsageRecord> {
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?.as_object()?;
    let at_ms = timestamp_ms(value.get("timestamp"))?;
    let model = text(message.get("model"))?;
    // Claude Code's stand-in for locally made messages (errors, interrupts).
    if model == "<synthetic>" {
        return None;
    }
    let tokens = UsageTokens {
        input: count(usage.get("input_tokens")),
        output: count(usage.get("output_tokens")),
        cache_read: count(usage.get("cache_read_input_tokens")),
        cache_write: count(usage.get("cache_creation_input_tokens")),
    };
    if tokens.total() == 0 {
        return None;
    }
    let message_id = text(message.get("id"));
    let request_id = text(value.get("requestId"));
    let key = match (&message_id, &request_id) {
        (None, None) => format!("claude:{fallback_key}"),
        (m, r) => format!(
            "claude:{}:{}",
            m.as_deref().unwrap_or(""),
            r.as_deref().unwrap_or("")
        ),
    };
    Some(UsageRecord {
        key,
        source: Source::ClaudeLog,
        at_ms,
        started_ms: None,
        provider: "claude-code".into(),
        model: Some(model),
        session_id: text(value.get("sessionId")),
        chat_id: None,
        cwd: text(value.get("cwd")),
        tokens,
    })
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// What a Codex rollout carries from line to line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(super) struct CodexState {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Hash of the last counted `last_token_usage`.
    pub last_usage: Option<u64>,
    pub saw_session_meta: bool,
    pub suppressing_fork_copies: bool,
    pub fork_anchor_ms: i64,
}

fn is_forked_session_meta(payload: &Value) -> bool {
    payload.get("forked_from_id").is_some_and(Value::is_string)
        || payload
            .pointer("/source/subagent/thread_spawn/parent_thread_id")
            .is_some_and(Value::is_string)
}

/// One Codex rollout line, folding its context into `state`; a record for
/// a counted `token_count`.
pub(super) fn parse_codex_line(line: &str, state: &mut CodexState) -> Option<UsageRecord> {
    const MARKERS: [&str; 3] = ["\"token_count\"", "\"turn_context\"", "\"session_meta\""];
    if !MARKERS.iter().any(|marker| line.contains(marker)) {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let payload = value.get("payload")?;
    match value.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            // Only the first meta is this file's own session; a fork repeats
            // its ancestors' right after it.
            if state.saw_session_meta {
                return None;
            }
            state.saw_session_meta = true;
            state.session_id = text(payload.get("id")).or_else(|| text(payload.get("session_id")));
            if let Some(cwd) = text(payload.get("cwd")) {
                state.cwd = Some(cwd);
            }
            if let Some(at) = timestamp_ms(value.get("timestamp"))
                && is_forked_session_meta(payload)
            {
                state.suppressing_fork_copies = true;
                state.fork_anchor_ms = at;
            }
            return None;
        }
        Some("turn_context") => {
            if let Some(model) = text(payload.get("model")) {
                state.model = Some(model);
            }
            if let Some(cwd) = text(payload.get("cwd")) {
                state.cwd = Some(cwd);
            }
            return None;
        }
        _ => {}
    }
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let last = payload.pointer("/info/last_token_usage")?.as_object()?;
    let at_ms = timestamp_ms(value.get("timestamp"))?;
    // An event before its turn_context can't be priced; it must not consume
    // the duplicate signature either, or its re-emission would be dropped.
    let model = state.model.clone()?;
    let signature = fnv1a(serde_json::to_string(last).ok()?.as_bytes());
    if state.last_usage == Some(signature) {
        return None;
    }
    state.last_usage = Some(signature);
    if state.suppressing_fork_copies {
        if at_ms - state.fork_anchor_ms < FORK_COPY_MAX_GAP_MS {
            state.fork_anchor_ms = at_ms;
            return None;
        }
        state.suppressing_fork_copies = false;
    }
    let input = count(last.get("input_tokens"));
    let cached = count(last.get("cached_input_tokens"));
    let cache_write = count(last.get("cache_write_input_tokens"));
    let tokens = UsageTokens {
        input: input.saturating_sub(cached).saturating_sub(cache_write),
        output: count(last.get("output_tokens")),
        cache_read: cached.min(input),
        cache_write,
    };
    if tokens.total() == 0 {
        return None;
    }
    let session = state.session_id.clone();
    // A rollout that moved (archived) leaves the same events in two files:
    // the key is the event, not the file.
    let key = format!(
        "codex:{}:{at_ms}:{signature:016x}",
        session.as_deref().unwrap_or("")
    );
    Some(UsageRecord {
        key,
        source: Source::CodexLog,
        at_ms,
        started_ms: None,
        provider: "codex".into(),
        model: Some(model),
        session_id: session,
        chat_id: None,
        cwd: state.cwd.clone(),
        tokens,
    })
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Every `*.jsonl` under `root` (bounded depth, symlinks not followed),
/// with its size and mtime.
pub(super) fn list_logs(root: &Path) -> Vec<(PathBuf, u64, i64)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if kind.is_file()
                && path.extension().is_some_and(|ext| ext == "jsonl")
                && let Ok(meta) = entry.metadata()
            {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                out.push((path, meta.len(), mtime));
            }
        }
    }
    out.sort();
    out
}

fn guard_at(file: &mut std::fs::File, offset: u64) -> std::io::Result<u64> {
    let start = offset.saturating_sub(GUARD_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (offset - start) as usize];
    file.read_exact(&mut buf)?;
    Ok(fnv1a(&buf))
}

/// What one pass over a file produced.
#[derive(Debug, Default)]
pub(super) struct FileScan {
    pub records: Vec<UsageRecord>,
    pub offset: u64,
    pub guard: u64,
    pub state: String,
    /// Bytes read this pass.
    pub read: u64,
}

/// Read `path` from its stored position (`from`, with the parser `state`
/// saved there and the `guard` hash of the bytes before it), at most
/// `budget` bytes, stopping at a line end. A position that no longer fits
/// the file restarts it from the top. Only whole lines are consumed: a
/// partly written last line waits for the next pass.
pub(super) fn scan_file(
    path: &Path,
    source: Source,
    from: Option<(u64, u64, &str)>,
    size: u64,
    budget: u64,
) -> std::io::Result<FileScan> {
    let mut file = std::fs::File::open(path)?;
    let mut offset = 0u64;
    let mut codex = CodexState::default();
    if let Some((at, guard, state)) = from
        && at <= size
        && guard_at(&mut file, at)? == guard
    {
        offset = at;
        if source == Source::CodexLog {
            codex = serde_json::from_str(state).unwrap_or_default();
        }
    }
    file.seek(SeekFrom::Start(offset))?;
    let path_key = format!("{:016x}", fnv1a(path.to_string_lossy().as_bytes()));
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut line = Vec::new();
    let mut records = Vec::new();
    let mut read = 0u64;
    while read < budget {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)? as u64;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let at = offset;
        offset += n;
        read += n;
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let record = match source {
            Source::ClaudeLog => parse_claude_line(text, &format!("{path_key}:{at}")),
            Source::CodexLog => parse_codex_line(text, &mut codex),
            Source::Godspeed => None,
        };
        records.extend(record);
    }
    let mut file = reader.into_inner();
    let guard = guard_at(&mut file, offset)?;
    let state = match source {
        Source::CodexLog => serde_json::to_string(&codex).unwrap_or_default(),
        _ => String::new(),
    };
    Ok(FileScan {
        records,
        offset,
        guard,
        state,
        read,
    })
}
