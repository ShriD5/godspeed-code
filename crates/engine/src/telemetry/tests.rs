use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::usage_history::{Source, UsageLogRoots, UsageRecord};

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
}

fn at(d: u32, h: u32) -> i64 {
    Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0)
        .unwrap()
        .timestamp_millis()
}

const NO_DELAYS: [Duration; 0] = [];

/// A poster that records every ping and answers `outcomes` in turn
/// (then `Sent`).
fn recorder(
    outcomes: Vec<Attempt>,
) -> (
    Arc<Mutex<Vec<TelemetryPing>>>,
    impl FnMut(TelemetryPing) -> std::future::Ready<Attempt>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let mut outcomes = outcomes.into_iter();
    let post = move |ping: TelemetryPing| {
        lock(&log).push(ping);
        std::future::ready(outcomes.next().unwrap_or(Attempt::Sent))
    };
    (seen, post)
}

fn mark(telemetry: &Telemetry, d: NaiveDate, active: bool, approvals: u64) {
    let mut state = lock(&telemetry.inner.state);
    let mark = state.mark(d);
    mark.active |= active;
    mark.approvals += approvals;
}

#[tokio::test]
async fn sends_at_most_once_per_day_and_reports_the_previous_day() {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open_with_env(dir.path(), None);
    let sources = TelemetrySources::default();

    // Day 1: the first ping has nothing earlier to report.
    telemetry.mark_today(day(26), true);
    mark(&telemetry, day(26), false, 2);
    let (seen, mut post) = recorder(vec![]);
    assert!(
        telemetry
            .send_due(day(26), true, &sources, &NO_DELAYS, &mut post)
            .await
    );
    // A second chance the same day sends nothing.
    assert!(
        !telemetry
            .send_due(day(26), true, &sources, &NO_DELAYS, &mut post)
            .await
    );
    // Day 2 (background engine, no one used it yet): reports day 1.
    telemetry.mark_today(day(27), false);
    assert!(
        telemetry
            .send_due(day(27), false, &sources, &NO_DELAYS, &mut post)
            .await
    );
    let seen = lock(&seen).clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].day, day(26));
    assert!(seen[0].active);
    assert_eq!(seen[0].usage, None);
    assert_eq!(seen[0].install_id, telemetry.install_id());
    assert_eq!(seen[0].v, TELEMETRY_VERSION);
    assert_eq!(seen[1].day, day(27));
    assert!(!seen[1].active);
    let usage = seen[1].usage.as_ref().unwrap();
    assert_eq!(usage.day, day(26));
    assert_eq!(usage.approvals_answered, 2);
    assert_eq!(seen[1].install_id, seen[0].install_id);

    // The state survives a restart: same install id, nothing owed today.
    let reopened = Telemetry::open_with_env(dir.path(), None);
    assert_eq!(reopened.install_id(), telemetry.install_id());
    let (again, mut post) = recorder(vec![]);
    assert!(
        !reopened
            .send_due(day(27), true, &sources, &NO_DELAYS, &mut post)
            .await
    );
    assert!(lock(&again).is_empty());
}

#[tokio::test]
async fn an_idle_day_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open_with_env(dir.path(), None);
    let (seen, mut post) = recorder(vec![]);
    // The engine never ran (or saw activity) on day 27.
    assert!(
        !telemetry
            .send_due(
                day(27),
                true,
                &TelemetrySources::default(),
                &NO_DELAYS,
                &mut post
            )
            .await
    );
    assert!(lock(&seen).is_empty());
}

#[tokio::test]
async fn the_settings_switch_and_the_environment_turn_it_off() {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open_with_env(dir.path(), None);
    assert!(telemetry.settings().enabled);
    assert!(telemetry.settings().notice_pending);
    telemetry.mark_today(day(27), true);
    let settings = telemetry
        .update(TelemetrySettingsUpdate {
            enabled: Some(false),
            notice_shown: Some(true),
        })
        .unwrap();
    assert!(!settings.enabled && !settings.notice_pending && !settings.sending());
    let (seen, mut post) = recorder(vec![]);
    let sources = TelemetrySources::default();
    assert!(
        !telemetry
            .send_due(day(27), true, &sources, &NO_DELAYS, &mut post)
            .await
    );
    // Off sticks across restarts.
    assert!(
        !Telemetry::open_with_env(dir.path(), None)
            .settings()
            .enabled
    );

    // DO_NOT_TRACK / GODSPEED_TELEMETRY=0 win over an enabled switch.
    let env_dir = tempfile::tempdir().unwrap();
    let env_off = Telemetry::open_with_env(env_dir.path(), Some("DO_NOT_TRACK"));
    assert!(env_off.settings().enabled);
    assert_eq!(
        env_off.settings().disabled_by_env.as_deref(),
        Some("DO_NOT_TRACK")
    );
    env_off.mark_today(day(27), true);
    env_off.note_activity(true);
    {
        // Nothing is even recorded locally while off.
        let state = lock(&env_off.inner.state);
        assert!(state.days.is_empty());
    }
    lock(&env_off.inner.state).mark(day(27));
    assert!(
        !env_off
            .send_due(day(27), true, &sources, &NO_DELAYS, &mut post)
            .await
    );
    assert!(lock(&seen).is_empty());
}

#[tokio::test(start_paused = true)]
async fn retries_with_backoff_then_drops_the_day() {
    let tries = Arc::new(AtomicUsize::new(0));
    let counted = tries.clone();
    let started = tokio::time::Instant::now();
    let sent = send_with_retry(
        || {
            counted.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Attempt::Retry)
        },
        &RETRY_DELAYS,
    )
    .await;
    assert!(!sent);
    assert_eq!(tries.load(Ordering::SeqCst), RETRY_DELAYS.len() + 1);
    let waited: Duration = RETRY_DELAYS.iter().sum();
    assert!(started.elapsed() >= waited);

    // A rejection (4xx) stops at once; a later success counts.
    let tries = AtomicUsize::new(0);
    assert!(
        !send_with_retry(
            || {
                tries.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Attempt::Stop)
            },
            &RETRY_DELAYS
        )
        .await
    );
    assert_eq!(tries.load(Ordering::SeqCst), 1);

    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open_with_env(dir.path(), None);
    telemetry.mark_today(day(26), true);
    telemetry.mark_today(day(27), true);
    let (seen, mut post) = recorder(vec![Attempt::Retry, Attempt::Retry]);
    assert!(
        telemetry
            .send_due(
                day(27),
                true,
                &TelemetrySources::default(),
                &RETRY_DELAYS,
                &mut post
            )
            .await
    );
    // Retries resend the identical ping (the edge upserts on install+day).
    let seen = lock(&seen).clone();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|ping| *ping == seen[0]));

    // All attempts failing: today is done, and the next day reports the
    // latest day instead.
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open_with_env(dir.path(), None);
    telemetry.mark_today(day(25), true);
    telemetry.mark_today(day(26), true);
    let (_, mut failing) = recorder(vec![Attempt::Retry; 10]);
    let sources = TelemetrySources::default();
    assert!(
        !telemetry
            .send_due(day(26), true, &sources, &RETRY_DELAYS, &mut failing)
            .await
    );
    assert!(
        !telemetry
            .send_due(day(26), true, &sources, &NO_DELAYS, &mut failing)
            .await,
        "at most one attempt run per day"
    );
    telemetry.mark_today(day(27), true);
    let (seen, mut post) = recorder(vec![]);
    assert!(
        telemetry
            .send_due(day(27), true, &sources, &NO_DELAYS, &mut post)
            .await
    );
    assert_eq!(
        lock(&seen)[0].usage.as_ref().map(|usage| usage.day),
        Some(day(26))
    );
}

fn turn(key: &str, at_ms: i64, harness: &str, model: &str, input: u64, output: u64) -> UsageRecord {
    UsageRecord {
        key: key.into(),
        source: Source::Godspeed,
        at_ms,
        started_ms: None,
        provider: harness.into(),
        model: Some(model.into()),
        session_id: None,
        chat_id: Some("chat".into()),
        cwd: Some("/Users/someone/secret-repo".into()),
        tokens: zeron_proto::UsageTokens {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
        },
    }
}

#[test]
fn usage_comes_from_the_usage_history_index() {
    let dir = tempfile::tempdir().unwrap();
    let index = UsageIndex::open(dir.path(), UsageLogRoots::default(), None);
    index.insert_for_test(&[
        turn(
            "gs:a:1",
            at(26, 1),
            "claude-code",
            "claude-opus-5-5",
            1_000_000,
            0,
        ),
        turn(
            "gs:a:2",
            at(26, 12),
            "claude-code",
            "claude-opus-5-5",
            0,
            100_000,
        ),
        turn("gs:b:1", at(26, 20), "codex", "gpt-unknown", 500, 50),
        turn("gs:c:1", at(26, 21), "some-new-harness", "x", 1, 1),
        // The next day isn't part of day 26.
        turn("gs:a:3", at(27, 0), "claude-code", "claude-opus-5-5", 9, 9),
    ]);
    let in_day = |from: DateTime<Utc>, to: DateTime<Utc>| {
        assert_eq!(to - from, chrono::Duration::days(1));
        assert_eq!(from.timestamp_millis(), at(26, 0));
        7
    };
    let sources = TelemetrySources {
        usage: Some(index),
        chats_started: Some(Arc::new(in_day)),
        zoomie_runs: Some(Arc::new(|_, _| 2)),
    };
    let usage = sources.collect(day(26), 5);
    assert_eq!(usage.day, day(26));
    assert_eq!(usage.turns_finished, 4);
    assert_eq!(usage.chats_started, 7);
    assert_eq!(usage.zoomie_runs, 2);
    assert_eq!(usage.approvals_answered, 5);
    assert_eq!(
        usage.harnesses,
        BTreeMap::from([
            ("claude-code".to_owned(), 2),
            ("codex".to_owned(), 1),
            // Unknown harness ids never travel as free text.
            ("other".to_owned(), 1),
        ])
    );
    assert_eq!(usage.tokens.input, 1_000_501);
    assert_eq!(usage.tokens.output, 100_051);
    // Opus 5.5: 1M input at $4 + 100K output at $20.
    assert!((usage.spend_usd - 6.0).abs() < 1e-9, "{}", usage.spend_usd);
    // Nothing path-, chat- or model-shaped leaks into the payload.
    let json = serde_json::to_string(&usage).unwrap();
    assert!(!json.contains("secret-repo") && !json.contains("\"chat\"") && !json.contains("opus"));
}
