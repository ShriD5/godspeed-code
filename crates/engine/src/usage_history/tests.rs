use std::io::Write as _;

use chrono::{NaiveDate, TimeZone, Utc};
use zeron_doc::{MessageRole, SessionMessageEntry, TurnStats, TurnUsage};
use zeron_proto::{UsageBucketSize, UsageRange};

use super::aggregate::{self, ChatInfo, Registry};
use super::*;

fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
        .unwrap()
        .timestamp_millis()
}

fn iso(at_ms: i64) -> String {
    Utc.timestamp_millis_opt(at_ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageTokens {
    UsageTokens {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn record(source: Source, key: &str, at_ms: i64, provider: &str, model: &str) -> UsageRecord {
    UsageRecord {
        key: key.into(),
        source,
        at_ms,
        started_ms: None,
        provider: provider.into(),
        model: Some(model.into()),
        session_id: None,
        chat_id: None,
        cwd: None,
        tokens: tokens(1_000, 100, 0, 0),
    }
}

fn claude_line(
    at_ms: i64,
    msg: &str,
    req: &str,
    session: &str,
    model: &str,
    usage: &str,
) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{}","sessionId":"{session}","cwd":"/work/app","requestId":"{req}","message":{{"id":"{msg}","model":"{model}","role":"assistant","content":[{{"type":"text","text":"hi"}}],"usage":{usage}}}}}"#,
        iso(at_ms)
    )
}

fn codex_meta(at_ms: i64, id: &str, extra: &str) -> String {
    format!(
        r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"{id}","cwd":"/work/api"{extra}}}}}"#,
        iso(at_ms)
    )
}

fn codex_context(at_ms: i64, model: &str) -> String {
    format!(
        r#"{{"timestamp":"{}","type":"turn_context","payload":{{"model":"{model}","cwd":"/work/api"}}}}"#,
        iso(at_ms)
    )
}

fn codex_tokens(at_ms: i64, input: u64, cached: u64, output: u64) -> String {
    format!(
        r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":0}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{}}}}}}}}}"#,
        iso(at_ms),
        input + output
    )
}

// ---------------------------------------------------------------------------
// Log parsing
// ---------------------------------------------------------------------------

#[test]
fn claude_lines_parse_usage_and_skip_the_rest() {
    let at = ms(2026, 10, 17, 9, 30);
    let usage = r#"{"input_tokens":12,"output_tokens":340,"cache_read_input_tokens":48000,"cache_creation_input_tokens":1200}"#;
    let line = claude_line(at, "msg_1", "req_1", "sess-a", "claude-opus-5-5", usage);
    let r = logs::parse_claude_line(&line, "f:0").unwrap();
    assert_eq!(r.key, "claude:msg_1:req_1");
    assert_eq!(r.at_ms, at);
    assert_eq!(r.provider, "claude-code");
    assert_eq!(r.session_id.as_deref(), Some("sess-a"));
    assert_eq!(r.cwd.as_deref(), Some("/work/app"));
    assert_eq!(r.tokens, tokens(12, 340, 48_000, 1_200));

    let synthetic = claude_line(at, "m", "r", "s", "<synthetic>", usage);
    assert!(logs::parse_claude_line(&synthetic, "f:1").is_none());
    let zero = claude_line(
        at,
        "m",
        "r",
        "s",
        "claude-opus-5-5",
        r#"{"input_tokens":0,"output_tokens":0}"#,
    );
    assert!(logs::parse_claude_line(&zero, "f:2").is_none());
    let user = r#"{"type":"user","message":{"role":"user","content":"usage of assistant"}}"#;
    assert!(logs::parse_claude_line(user, "f:3").is_none());
    assert!(logs::parse_claude_line("{broken \"usage\" \"assistant\"", "f:4").is_none());
    // No ids: keyed by position.
    let anonymous = claude_line(at, "", "", "s", "claude-opus-5-5", usage)
        .replace(r#""requestId":"","#, "")
        .replace(r#""id":"","#, "");
    assert_eq!(
        logs::parse_claude_line(&anonymous, "f:5").unwrap().key,
        "claude:f:5"
    );
}

#[test]
fn codex_lines_carry_model_and_session_and_split_cached_input() {
    let t0 = ms(2026, 10, 17, 10, 0);
    let mut state = logs::CodexState::default();
    assert!(logs::parse_codex_line(&codex_meta(t0, "thread-1", ""), &mut state).is_none());
    // Before any turn_context: no model, not counted, and it must not eat
    // the duplicate signature.
    assert!(logs::parse_codex_line(&codex_tokens(t0 + 500, 1_000, 800, 50), &mut state).is_none());
    assert!(logs::parse_codex_line(&codex_context(t0 + 600, "gpt-5-codex"), &mut state).is_none());
    let r = logs::parse_codex_line(&codex_tokens(t0 + 5_000, 1_000, 800, 50), &mut state).unwrap();
    assert_eq!(r.provider, "codex");
    assert_eq!(r.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(r.session_id.as_deref(), Some("thread-1"));
    assert_eq!(r.cwd.as_deref(), Some("/work/api"));
    assert_eq!(r.tokens, tokens(200, 50, 800, 0), "cached input split out");
    // The identical re-emission is skipped; a new count is not.
    assert!(
        logs::parse_codex_line(&codex_tokens(t0 + 5_001, 1_000, 800, 50), &mut state).is_none()
    );
    assert!(logs::parse_codex_line(&codex_tokens(t0 + 9_000, 2_000, 0, 10), &mut state).is_some());
    // A second (ancestor) meta never reassigns the session.
    logs::parse_codex_line(&codex_meta(t0, "ancestor", ""), &mut state);
    assert_eq!(state.session_id.as_deref(), Some("thread-1"));
}

#[test]
fn a_forked_rollout_skips_its_copied_history() {
    let t0 = ms(2026, 10, 17, 11, 0);
    let mut state = logs::CodexState::default();
    let fork = codex_meta(t0, "child", r#","forked_from_id":"parent""#);
    logs::parse_codex_line(&fork, &mut state);
    logs::parse_codex_line(&codex_context(t0 + 1, "gpt-5-codex"), &mut state);
    // The copied burst, each line within a second of the last.
    assert!(logs::parse_codex_line(&codex_tokens(t0 + 10, 500, 0, 5), &mut state).is_none());
    assert!(logs::parse_codex_line(&codex_tokens(t0 + 30, 600, 0, 6), &mut state).is_none());
    // The child's own first turn, seconds later.
    let own = logs::parse_codex_line(&codex_tokens(t0 + 8_000, 700, 0, 7), &mut state).unwrap();
    assert_eq!(own.session_id.as_deref(), Some("child"));
}

// ---------------------------------------------------------------------------
// Incremental scanning
// ---------------------------------------------------------------------------

fn index_with_logs(dir: &Path) -> (UsageIndex, PathBuf, PathBuf) {
    let claude = dir.join("claude").join("projects");
    let codex = dir.join("codex");
    std::fs::create_dir_all(claude.join("-work-app")).unwrap();
    std::fs::create_dir_all(codex.join("sessions").join("2026").join("10").join("17")).unwrap();
    let index = UsageIndex::open(
        &dir.join("data"),
        UsageLogRoots {
            claude_projects: Some(claude.clone()),
            codex_home: Some(codex.clone()),
        },
        None,
    );
    (index, claude, codex)
}

fn all_records(index: &UsageIndex) -> Vec<UsageRecord> {
    let mut rows = index
        .with_conn(|conn| store::records_between(conn, i64::MIN, i64::MAX))
        .unwrap();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

#[test]
fn rescans_read_only_what_was_appended_and_restart_rewritten_files() {
    let dir = tempfile::tempdir().unwrap();
    let (index, claude, codex) = index_with_logs(dir.path());
    let at = ms(2026, 10, 17, 9, 0);
    let usage = r#"{"input_tokens":10,"output_tokens":5}"#;
    let file = claude.join("-work-app").join("sess-a.jsonl");
    // Claude Code writes one line per content block, each repeating the
    // message's usage (the streamed first copy may undercount output).
    let partial = r#"{"input_tokens":10,"output_tokens":1}"#;
    std::fs::write(
        &file,
        format!(
            "{}\n{}\n",
            claude_line(at, "msg_1", "req_1", "sess-a", "claude-opus-5-5", partial),
            claude_line(at, "msg_1", "req_1", "sess-a", "claude-opus-5-5", usage),
        ),
    )
    .unwrap();
    // Built per component: the file mark is keyed by the scanned path, which
    // uses `\` on Windows.
    let rollout = codex
        .join("sessions")
        .join("2026")
        .join("10")
        .join("17")
        .join("rollout-a.jsonl");
    std::fs::write(
        &rollout,
        format!(
            "{}\n{}\n{}\n",
            codex_meta(at, "thread-1", ""),
            codex_context(at + 1, "gpt-5-codex"),
            codex_tokens(at + 5_000, 100, 0, 10),
        ),
    )
    .unwrap();
    index.scan_now().unwrap();
    let rows = all_records(&index);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].tokens,
        tokens(10, 5, 0, 0),
        "one row per message, fullest copy"
    );

    // Appended lines, plus a half-written one: only whole lines count, and
    // the Codex model carries over from the earlier pass.
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .unwrap();
    write!(
        out,
        "{}\n{{\"timestamp\":",
        codex_tokens(at + 9_000, 300, 100, 20)
    )
    .unwrap();
    drop(out);
    index.scan_now().unwrap();
    let rows = all_records(&index);
    assert_eq!(rows.len(), 3);
    let newest = rows.iter().find(|r| r.at_ms == at + 9_000).unwrap();
    assert_eq!(newest.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(newest.tokens, tokens(200, 20, 100, 0));
    let mark = index
        .with_conn(|conn| store::file_mark(conn, &rollout.to_string_lossy()))
        .unwrap()
        .unwrap();
    let whole = std::fs::read(&rollout).unwrap();
    let last_newline = whole.iter().rposition(|b| *b == b'\n').unwrap() as u64 + 1;
    assert_eq!(mark.offset, last_newline, "the partial line waits");

    // Nothing changed: the pass reads nothing (the mark stays put).
    index.scan_now().unwrap();
    assert_eq!(all_records(&index).len(), 3);

    // A rewritten (shorter) file is read again from the top; its keys
    // dedupe against what's indexed.
    std::fs::write(
        &file,
        format!(
            "{}\n",
            claude_line(
                at + 60_000,
                "msg_2",
                "req_2",
                "sess-a",
                "claude-opus-5-5",
                usage
            )
        ),
    )
    .unwrap();
    index.scan_now().unwrap();
    let rows = all_records(&index);
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().any(|r| r.key == "claude:msg_2:req_2"));
}

#[test]
fn an_archived_rollout_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    let (index, _, codex) = index_with_logs(dir.path());
    let at = ms(2026, 10, 17, 9, 0);
    let body = format!(
        "{}\n{}\n{}\n",
        codex_meta(at, "thread-9", ""),
        codex_context(at + 1, "gpt-5-codex"),
        codex_tokens(at + 5_000, 100, 0, 10),
    );
    std::fs::write(codex.join("sessions/2026/10/17/rollout-9.jsonl"), &body).unwrap();
    std::fs::create_dir_all(codex.join("archived_sessions")).unwrap();
    std::fs::write(codex.join("archived_sessions/rollout-9.jsonl"), &body).unwrap();
    index.scan_now().unwrap();
    assert_eq!(all_records(&index).len(), 1);
}

// ---------------------------------------------------------------------------
// Godspeed turns
// ---------------------------------------------------------------------------

fn entry(id: &str, role: MessageRole, at: i64) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts: Vec::new(),
        created_at: at,
        device_id: "dev".into(),
        status: None,
        continuation_of: None,
        duration_ms: None,
        turn_stats: None,
    }
}

#[test]
fn stamped_turns_become_records_timed_from_their_prompt() {
    let t0 = ms(2026, 10, 17, 12, 0);
    let mut first_segment = entry("a1", MessageRole::Assistant, t0 + 1_000);
    first_segment.duration_ms = Some(20_000);
    let mut last_segment = entry("a2", MessageRole::Assistant, t0 + 21_000);
    last_segment.duration_ms = Some(9_000);
    last_segment.turn_stats = Some(TurnStats {
        harness: Some("claude-code".into()),
        model: Some("claude-opus-5-5".into()),
        usage: Some(TurnUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: Some(30),
            cache_write_tokens: None,
        }),
        session_id: Some("sess-a".into()),
        ..Default::default()
    });
    let mut no_usage = entry("b1", MessageRole::Assistant, t0 + 70_000);
    no_usage.turn_stats = Some(TurnStats {
        harness: Some("cursor".into()),
        ..Default::default()
    });
    let entries = vec![
        entry("u1", MessageRole::User, t0),
        first_segment,
        last_segment,
        entry("u2", MessageRole::User, t0 + 60_000),
        no_usage,
    ];
    let records = turns::turn_records("chat-1", &entries);
    assert_eq!(records.len(), 1, "a stamp without usage is no record");
    let r = &records[0];
    assert_eq!(r.key, "gs:chat-1:a2");
    assert_eq!(r.started_ms, Some(t0));
    assert_eq!(r.at_ms, t0 + 30_000);
    assert_eq!(r.session_id.as_deref(), Some("sess-a"));
    assert_eq!(r.tokens, tokens(10, 20, 30, 0));
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

fn tz(name: &str) -> chrono_tz::Tz {
    name.parse().unwrap()
}

#[test]
fn days_follow_the_viewers_zone() {
    // 20:00 UTC on Oct 16 is Oct 17 01:30 in Kolkata and Oct 16 13:00 in
    // Los Angeles.
    let at = ms(2026, 10, 16, 20, 0);
    let now = ms(2026, 10, 17, 20, 0);
    let records = vec![record(
        Source::Godspeed,
        "gs:c:1",
        at,
        "claude-code",
        "claude-opus-5-5",
    )];
    let day_of = |zone: &str| {
        let zone = tz(zone);
        let bounds = aggregate::bounds(UsageRange::Week, &zone, now, None);
        let history = aggregate::aggregate(
            UsageRange::Week,
            &zone,
            &bounds,
            &records,
            &Registry::default(),
        );
        assert_eq!(history.buckets.len(), 7);
        history
            .buckets
            .iter()
            .find(|b| !b.points.is_empty())
            .map(|b| b.day)
            .unwrap()
    };
    assert_eq!(
        day_of("Asia/Kolkata"),
        NaiveDate::from_ymd_opt(2026, 10, 17).unwrap()
    );
    assert_eq!(
        day_of("America/Los_Angeles"),
        NaiveDate::from_ymd_opt(2026, 10, 16).unwrap()
    );
    assert_eq!(
        day_of("UTC"),
        NaiveDate::from_ymd_opt(2026, 10, 16).unwrap()
    );
}

#[test]
fn ranges_bucket_by_local_hour_day_or_week() {
    let la = tz("America/Los_Angeles");
    // Today on a spring-forward day has 23 local hours, and ends at the
    // next local midnight.
    let now = ms(2026, 3, 8, 20, 0);
    let today = aggregate::bounds(UsageRange::Today, &la, now, None);
    assert_eq!(today.size, UsageBucketSize::Hour);
    assert_eq!(today.buckets.len(), 23);
    assert_eq!((today.end - today.start()).num_hours(), 23);
    // 30 days are 30 local days ending today; the range's end is the next
    // local midnight.
    let month = aggregate::bounds(UsageRange::Month, &la, now, None);
    assert_eq!(month.buckets.len(), 30);
    assert_eq!(
        month.buckets.last().unwrap().1,
        NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()
    );
    // All: from the first record's day, daily up to 92 days, weekly past.
    let all = aggregate::bounds(UsageRange::All, &la, now, Some(ms(2026, 2, 1, 20, 0)));
    assert_eq!(all.size, UsageBucketSize::Day);
    assert_eq!(
        all.buckets[0].1,
        NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()
    );
    let long = aggregate::bounds(UsageRange::All, &la, now, Some(ms(2025, 6, 4, 20, 0)));
    assert_eq!(long.size, UsageBucketSize::Week);
    assert_eq!(
        long.buckets[0].1,
        NaiveDate::from_ymd_opt(2025, 6, 2).unwrap(),
        "weeks start Monday"
    );
    let empty = aggregate::bounds(UsageRange::All, &la, now, None);
    assert_eq!(empty.buckets.len(), 1);
}

#[test]
fn costs_price_cache_traffic_and_leave_unknown_models_unpriced() {
    let utc = chrono_tz::UTC;
    let now = ms(2026, 10, 17, 20, 0);
    let mut opus = record(
        Source::Godspeed,
        "gs:c:1",
        now - 1_000,
        "claude-code",
        "claude-opus-5-5[1m]",
    );
    opus.tokens = tokens(1_000_000, 100_000, 2_000_000, 100_000);
    let mut mystery = record(
        Source::Godspeed,
        "gs:c:2",
        now - 2_000,
        "muse",
        "muse-large",
    );
    mystery.tokens = tokens(500, 500, 0, 0);
    let mut nameless = record(Source::Godspeed, "gs:c:3", now - 3_000, "cursor", "x");
    nameless.model = None;
    nameless.tokens = tokens(7, 0, 0, 0);
    let records = vec![opus, mystery, nameless];
    let bounds = aggregate::bounds(UsageRange::Today, &utc, now, None);
    let h = aggregate::aggregate(
        UsageRange::Today,
        &utc,
        &bounds,
        &records,
        &Registry::default(),
    );
    // $4 + $2 + $0.40 + $0.50.
    assert!(
        (h.total.cost_usd - 6.9).abs() < 1e-9,
        "{}",
        h.total.cost_usd
    );
    assert!((h.total.cache_savings_usd - 7.6).abs() < 1e-9);
    assert_eq!(h.total.unpriced_tokens, 1_007);
    assert_eq!(h.total.turns, 3);
    let model = |key: &str| h.by_model.iter().find(|s| s.key == key).unwrap();
    assert_eq!(model("claude-opus-5-5").amount.tokens.cache_read, 2_000_000);
    assert!(model("muse-large").amount.wholly_unpriced());
    assert!(model("unknown").amount.wholly_unpriced());
    assert_eq!(h.by_model[0].key, "claude-opus-5-5", "most expensive first");
    assert_eq!(h.by_provider[0].label, "Claude Code");
    assert_eq!(h.prices_checked, zeron_proto::pricing::PRICES_CHECKED);
}

fn registry() -> Registry {
    let mut chats = HashMap::new();
    chats.insert(
        "chat-live".to_owned(),
        ChatInfo {
            title: Some("Fix the parser".into()),
            project: Some(("/work/app".into(), "app".into())),
            harness: Some("claude-code".into()),
            session_id: Some("sess-live".into()),
        },
    );
    chats.insert(
        "chat-imported".to_owned(),
        ChatInfo {
            title: Some("Imported Codex thread".into()),
            project: Some(("/work/api".into(), "api".into())),
            harness: Some("codex".into()),
            session_id: Some("thread-old".into()),
        },
    );
    Registry {
        chats,
        projects: vec![
            ("/work/app".into(), "app".into()),
            ("/work/api".into(), "api".into()),
        ],
    }
}

#[test]
fn log_lines_inside_a_godspeed_turn_are_that_turn() {
    let utc = chrono_tz::UTC;
    let now = ms(2026, 10, 17, 20, 0);
    let t0 = ms(2026, 10, 17, 12, 0);
    // A Godspeed turn 12:00:00–12:01:00 in session sess-live.
    let mut turn = record(
        Source::Godspeed,
        "gs:chat-live:a1",
        t0 + 60_000,
        "claude-code",
        "claude-opus-5-5",
    );
    turn.started_ms = Some(t0);
    turn.chat_id = Some("chat-live".into());
    turn.session_id = Some("sess-live".into());
    // Its own calls, as Claude Code logged them: dropped.
    let mut inside = record(
        Source::ClaudeLog,
        "claude:m1:r1",
        t0 + 20_000,
        "claude-code",
        "claude-opus-5-5",
    );
    inside.session_id = Some("sess-live".into());
    let mut trailing = inside.clone();
    trailing.key = "claude:m2:r2".into();
    trailing.at_ms = t0 + 75_000;
    // The same session used from the terminal an hour later: counted, and
    // the chat's.
    let mut later = inside.clone();
    later.key = "claude:m3:r3".into();
    later.at_ms = t0 + 3_600_000;
    // Another session at the same moment: counted, placed by its cwd.
    let mut other = inside.clone();
    other.key = "claude:m4:r4".into();
    other.session_id = Some("sess-cli".into());
    other.cwd = Some("/work/app/crates/core".into());
    // A Codex thread an imported chat resumes: that chat's.
    let mut imported = record(Source::CodexLog, "codex:t:1", t0, "codex", "gpt-5-codex");
    imported.session_id = Some("thread-old".into());
    // A Codex session nobody resumes, outside every project.
    let mut stray = record(Source::CodexLog, "codex:t:2", t0, "codex", "gpt-5-codex");
    stray.session_id = Some("thread-stray".into());
    stray.cwd = Some("/tmp/scratch".into());
    let records = vec![turn, inside, trailing, later, other, imported, stray];
    let bounds = aggregate::bounds(UsageRange::Today, &utc, now, None);
    let h = aggregate::aggregate(UsageRange::Today, &utc, &bounds, &records, &registry());

    assert_eq!(h.sources.deduplicated, 2);
    assert_eq!((h.total.turns, h.total.calls), (1, 4));
    let chat = |title: &str| h.top_chats.iter().find(|c| c.title == title).unwrap();
    let live = chat("Fix the parser");
    assert_eq!(live.chat_id.as_deref(), Some("chat-live"));
    assert_eq!((live.amount.turns, live.amount.calls), (1, 1));
    assert_eq!(chat("Imported Codex thread").amount.calls, 1);
    let cli = h
        .top_chats
        .iter()
        .filter(|c| c.chat_id.is_none())
        .collect::<Vec<_>>();
    assert_eq!(cli.len(), 2);
    assert!(
        cli.iter()
            .any(|c| c.title == "Claude Code session" && c.project.as_deref() == Some("app"))
    );
    assert!(
        cli.iter()
            .any(|c| c.title == "Codex session" && c.project.as_deref() == Some("scratch"))
    );
    assert_eq!(h.sessions, 4);
    let project = |label: &str| h.by_project.iter().find(|p| p.label == label).unwrap();
    // The turn, the later CLI call and the subfolder session.
    assert_eq!(project("app").amount.turns + project("app").amount.calls, 3);
    assert_eq!(project("api").amount.calls, 1);
    assert_eq!(project("scratch").key, "/tmp/scratch");
    // Hourly buckets: everything sits in the 12:00 and 13:00 hours.
    let busy: Vec<u8> = h
        .buckets
        .iter()
        .filter(|b| !b.points.is_empty())
        .filter_map(|b| b.hour)
        .collect();
    assert_eq!(busy, [12, 13]);
}

#[test]
fn a_turn_without_its_own_session_uses_the_chats() {
    let utc = chrono_tz::UTC;
    let now = ms(2026, 10, 17, 20, 0);
    let t0 = ms(2026, 10, 17, 12, 0);
    // Stamped before turn stats carried the session.
    let mut turn = record(
        Source::Godspeed,
        "gs:chat-live:a1",
        t0 + 60_000,
        "claude-code",
        "claude-opus-5-5",
    );
    turn.started_ms = Some(t0);
    turn.chat_id = Some("chat-live".into());
    let mut inside = record(
        Source::ClaudeLog,
        "claude:m1:r1",
        t0 + 20_000,
        "claude-code",
        "claude-opus-5-5",
    );
    inside.session_id = Some("sess-live".into());
    let bounds = aggregate::bounds(UsageRange::Today, &utc, now, None);
    let h = aggregate::aggregate(
        UsageRange::Today,
        &utc,
        &bounds,
        &[turn, inside],
        &registry(),
    );
    assert_eq!(h.sources.deduplicated, 1);
    assert_eq!(h.total.calls, 0);
}

#[test]
fn records_outside_the_range_are_left_out() {
    let utc = chrono_tz::UTC;
    let now = ms(2026, 10, 17, 20, 0);
    let records = vec![
        record(
            Source::Godspeed,
            "gs:c:old",
            ms(2026, 10, 10, 23, 59),
            "codex",
            "gpt-5",
        ),
        record(
            Source::Godspeed,
            "gs:c:in",
            ms(2026, 10, 11, 0, 0),
            "codex",
            "gpt-5",
        ),
        record(
            Source::Godspeed,
            "gs:c:future",
            ms(2026, 10, 18, 0, 0),
            "codex",
            "gpt-5",
        ),
    ];
    let bounds = aggregate::bounds(UsageRange::Week, &utc, now, None);
    let h = aggregate::aggregate(
        UsageRange::Week,
        &utc,
        &bounds,
        &records,
        &Registry::default(),
    );
    assert_eq!(h.total.turns, 1);
    assert_eq!(
        h.buckets[0].points.len(),
        1,
        "the first day's bucket holds it"
    );
}

// ---------------------------------------------------------------------------
// End to end over the store
// ---------------------------------------------------------------------------

#[test]
fn the_index_answers_from_logs_and_turns_together() {
    let dir = tempfile::tempdir().unwrap();
    let (index, claude, _) = index_with_logs(dir.path());
    let now = Utc::now().timestamp_millis();
    let usage = r#"{"input_tokens":1000,"output_tokens":100,"cache_read_input_tokens":5000}"#;
    std::fs::write(
        claude.join("-work-app").join("sess-cli.jsonl"),
        format!(
            "{}\n",
            claude_line(
                now - 60_000,
                "m",
                "r",
                "sess-cli",
                "claude-sonnet-5-5",
                usage
            )
        ),
    )
    .unwrap();
    index.scan_now().unwrap();
    let mut turn = record(
        Source::Godspeed,
        "gs:chat:1",
        now - 30_000,
        "codex",
        "gpt-5-codex",
    );
    turn.chat_id = Some("chat".into());
    index
        .with_conn(|conn| store::upsert(conn, &[turn]))
        .unwrap();
    let request = UsageHistoryRequest {
        range: UsageRange::Week,
        time_zone: Some("UTC".into()),
    };
    let h = index.query(&request, now, &Registry::default()).unwrap();
    assert_eq!(h.time_zone, "UTC");
    assert_eq!((h.total.turns, h.total.calls), (1, 1));
    assert_eq!(h.sources.claude_files, 1);
    assert_eq!(h.sources.godspeed_turns, 1);
    assert_eq!(h.by_provider.len(), 2);
    // A bad zone name falls back rather than failing.
    let fallback = index
        .query(
            &UsageHistoryRequest {
                range: UsageRange::Today,
                time_zone: Some("Not/AZone".into()),
            },
            now,
            &Registry::default(),
        )
        .unwrap();
    assert!(!fallback.time_zone.is_empty());
}

#[test]
fn godspeed_between_counts_only_godspeed_turns_in_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let index = UsageIndex::open(dir.path(), UsageLogRoots::default(), None);
    let mut opus = record(
        Source::Godspeed,
        "gs:a:1",
        ms(2026, 10, 26, 9, 0),
        "claude-code",
        "claude-opus-5-5",
    );
    opus.tokens = tokens(1_000_000, 100_000, 0, 0);
    let codex = record(
        Source::Godspeed,
        "gs:b:1",
        ms(2026, 10, 26, 23, 59),
        "codex",
        "no-such-model",
    );
    // A Claude Code log line (history outside Godspeed) and a turn the next
    // day are both left out.
    let log = record(
        Source::ClaudeLog,
        "claude:m:r",
        ms(2026, 10, 26, 10, 0),
        "claude-code",
        "claude-opus-5-5",
    );
    let tomorrow = record(
        Source::Godspeed,
        "gs:a:2",
        ms(2026, 10, 27, 0, 0),
        "claude-code",
        "claude-opus-5-5",
    );
    index.insert_for_test(&[opus, codex, log, tomorrow]);
    let day = index
        .godspeed_between(ms(2026, 10, 26, 0, 0), ms(2026, 10, 27, 0, 0))
        .unwrap();
    assert_eq!(day.turns, 2);
    assert_eq!(day.by_harness.get("claude-code"), Some(&1));
    assert_eq!(day.by_harness.get("codex"), Some(&1));
    assert_eq!(day.tokens, tokens(1_001_000, 100_100, 0, 0));
    // 1M input at $4 + 100K output at $20; the unknown model is unpriced.
    assert!((day.cost_usd - 6.0).abs() < 1e-9, "{}", day.cost_usd);
}
