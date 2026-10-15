//! Usage-limit classification shared by every driver and the engine.
//!
//! A turn the provider refused because the account ran out of its usage or
//! rate window can be resumed once the window resets; a turn that failed
//! because the conversation no longer fits the context window cannot, and
//! must never read as "limited" (t3code #19832: Claude's `blocking_limit`
//! with "Prompt is too long" was shown as a usage limit, and every retry
//! failed the same way). So the context-full check runs first and wins.
//!
//! Drivers with structured signals (Claude `rate_limit_event`, Codex
//! `codexErrorInfo` + `account/rateLimits/updated`, opencode's 429s) emit
//! [`AgentEvent::UsageLimit`] themselves. Everything else — ACP agents,
//! Cursor, Muse — only reports text, which [`classify`] reads; the engine
//! runs it over an errored turn that carried no structured signal.

use chrono::{DateTime, Datelike as _, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};

use zeron_proto::AgentEvent;

/// A usage-limit failure read from provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitHit {
    /// When the text says the window reopens; `None` when it doesn't say.
    pub reset_at: Option<DateTime<Utc>>,
}

/// The [`AgentEvent::UsageLimit`] a driver emits before its errored `Done`.
pub fn event(reset_at: Option<DateTime<Utc>>, window: Option<String>) -> AgentEvent {
    AgentEvent::UsageLimit { reset_at, window }
}

/// The text names a full context window (or an over-long prompt): never a
/// usage limit, however it is worded around it.
pub fn is_context_full(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "context window",
        "context length",
        "context_length",
        "contextwindowexceeded",
        "context_window_exceeded",
        "maximum context",
        "too many tokens",
        "token limit",
        "max_tokens",
        "maximum output length",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Billing failures that a reset never clears: out of credits, no plan.
fn is_billing(lower: &str) -> bool {
    [
        "insufficient_quota",
        "credit balance",
        "billing",
        "payment",
        "out of credits",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// `429` as a status code, not a digit run inside a bigger number.
fn mentions_429(lower: &str) -> bool {
    lower.match_indices("429").any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + 3..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit()) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// Read a usage/rate-limit failure out of provider text, with its reset time
/// when the text gives one. `None` for anything else, including a full
/// context window.
pub fn classify(text: &str, now: DateTime<Utc>) -> Option<LimitHit> {
    let lower = text.to_ascii_lowercase();
    if lower.trim().is_empty() || is_context_full(&lower) || is_billing(&lower) {
        return None;
    }
    let limited = [
        "usage limit",
        "rate limit",
        "rate-limit",
        "rate_limit",
        "ratelimit",
        "too many requests",
        "quota exceeded",
        "exceeded your quota",
        "quota exhausted",
        "resource_exhausted",
        "resource exhausted",
        "hit your limit",
        "hit your usage",
        "hit your session limit",
        "hit your weekly limit",
        "session limit",
        "weekly limit",
        "hour limit",
        "limit reached",
        "limit exceeded",
        "usagelimitexceeded",
        "usage_limit",
        "out of usage",
        "usage cap",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || mentions_429(&lower);
    limited.then(|| LimitHit {
        reset_at: parse_reset(text, now),
    })
}

/// Text-to-event: the [`AgentEvent::UsageLimit`] for a limit failure.
pub fn event_from_text(text: &str, now: DateTime<Utc>) -> Option<AgentEvent> {
    classify(text, now).map(|hit| event(hit.reset_at, None))
}

// ── reset times ─────────────────────────────────────────────────────────

/// Phrases after which a relative duration names the wait.
const DURATION_LEADS: [&str; 9] = [
    "try again in",
    "resets in",
    "reset in",
    "retry in",
    "retry after",
    "retry-after:",
    "available in",
    "again in",
    "wait",
];

/// Phrases after which a wall-clock time (or date) names the reset.
const CLOCK_LEADS: [&str; 8] = [
    "resets at",
    "reset at",
    "resets",
    "try again at",
    "retry after",
    "available at",
    "again at",
    "until",
];

/// When provider text says the limit lifts: a relative wait ("try again in
/// 4 days 2 hours"), a wall-clock time with an optional date and IANA zone
/// ("resets 4:40pm (Europe/London)", "resets Oct 20 at 9am"), an RFC 3339
/// stamp, or epoch seconds after a `resets_at` key. Pure given `now`;
/// zone-less clock times read as this machine's local time.
pub fn parse_reset(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_ascii_lowercase();
    if let Some(at) = rfc3339_in(text) {
        return Some(at);
    }
    if let Some(at) = epoch_after_key(&lower) {
        return Some(at);
    }
    for lead in DURATION_LEADS {
        for (at, _) in lower.match_indices(lead) {
            let bare_seconds = lead == "retry-after:";
            if let Some(wait) = parse_duration(&lower[at + lead.len()..], bare_seconds) {
                return Some(now + wait);
            }
        }
    }
    for lead in CLOCK_LEADS {
        for (at, _) in lower.match_indices(lead) {
            let rest = &text[at + lead.len()..];
            if let Some(at) = parse_clock(rest, now) {
                return Some(at);
            }
        }
    }
    None
}

fn rfc3339_in(text: &str) -> Option<DateTime<Utc>> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '(' | ')'))
        .filter(|token| token.len() >= 20 && token.as_bytes().get(4) == Some(&b'-'))
        .find_map(|token| {
            DateTime::parse_from_rfc3339(token.trim_end_matches('.'))
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })
}

fn epoch_after_key(lower: &str) -> Option<DateTime<Utc>> {
    ["resets_at", "reset_at", "resetsat", "resetat"]
        .iter()
        .find_map(|key| {
            let at = lower.find(key)?;
            let digits: String = lower[at + key.len()..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            let secs: i64 = digits.parse().ok()?;
            // Seconds, or milliseconds from providers that send those.
            let secs = if secs > 100_000_000_000 {
                secs / 1000
            } else {
                secs
            };
            (secs > 1_000_000_000)
                .then(|| Utc.timestamp_opt(secs, 0).single())
                .flatten()
        })
}

/// `4 days 2 hours 13 minutes`, `2h 13m`, `4h12m`, `120 seconds`, `90s`.
/// Stops at the first token that is neither a quantity nor a joiner. A bare
/// number counts as seconds only where the caller says so (the
/// `retry-after` header); elsewhere "16:40" is a clock time, not 16 seconds.
fn parse_duration(rest: &str, bare_seconds: bool) -> Option<Duration> {
    let mut total = Duration::zero();
    let mut found = false;
    let mut chars = rest.trim_start_matches([' ', ':']).chars().peekable();
    loop {
        while chars
            .peek()
            .is_some_and(|c| c.is_whitespace() || matches!(c, ',' | '~'))
        {
            chars.next();
        }
        // Joiners between quantities.
        let lookahead: String = chars.clone().take(4).collect();
        if lookahead.starts_with("and ") {
            chars.nth(3);
            continue;
        }
        let number: String = std::iter::from_fn(|| chars.next_if(char::is_ascii_digit)).collect();
        if number.is_empty() {
            break;
        }
        let Ok(n) = number.parse::<i64>() else {
            break;
        };
        while chars.peek().is_some_and(|c| *c == ' ') {
            chars.next();
        }
        let unit: String =
            std::iter::from_fn(|| chars.next_if(char::is_ascii_alphabetic)).collect();
        let step = match unit.as_str() {
            "d" | "day" | "days" => Duration::days(n),
            "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(n),
            "m" | "min" | "mins" | "minute" | "minutes" => Duration::minutes(n),
            "s" | "sec" | "secs" | "second" | "seconds" => Duration::seconds(n),
            "" if bare_seconds && !found && chars.peek().is_none_or(|c| *c != ':') => {
                Duration::seconds(n)
            }
            _ => break,
        };
        total += step;
        found = true;
    }
    (found && total > Duration::zero()).then_some(total)
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// `4:40pm (Europe/London)`, `3pm`, `16:40`, `Oct 20 at 9am`, `Oct 20, 8:00 PM`.
/// Bare numbers ("resets 5") are not times. The next such moment after `now`.
fn parse_clock(rest: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let text = rest.trim_start_matches([' ', ':', '·']);
    let lower = text.to_ascii_lowercase();
    let mut cursor = 0usize;
    // Optional "Oct 20" (with a trailing "," or "at").
    let mut date: Option<(u32, u32)> = None;
    if let Some(month) = MONTHS.iter().position(|m| lower.starts_with(m)) {
        let trimmed = lower[3..]
            .trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '.')
            .trim_start();
        let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
        let day: u32 = digits.parse().ok()?;
        cursor = lower.len() - trimmed.len() + digits.len();
        let tail = lower[cursor..].trim_start_matches([',', ' ']);
        let tail = tail.strip_prefix("at ").unwrap_or(tail);
        cursor = lower.len() - tail.len();
        date = Some((month as u32 + 1, day));
    }
    let clock = &lower[cursor..];
    let hour_digits: String = clock.chars().take_while(char::is_ascii_digit).collect();
    if hour_digits.is_empty() || hour_digits.len() > 2 {
        return None;
    }
    let mut hour: u32 = hour_digits.parse().ok()?;
    let mut tail = &clock[hour_digits.len()..];
    let mut minute = 0u32;
    let mut has_colon = false;
    if let Some(after) = tail.strip_prefix(':') {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() != 2 {
            return None;
        }
        minute = digits.parse().ok()?;
        tail = &after[2..];
        has_colon = true;
    }
    let spaced = tail.trim_start();
    let meridiem = if spaced.starts_with("am") || spaced.starts_with("a.m") {
        Some(false)
    } else if spaced.starts_with("pm") || spaced.starts_with("p.m") {
        Some(true)
    } else {
        None
    };
    if meridiem.is_none() && !has_colon {
        return None;
    }
    if let Some(pm) = meridiem {
        if hour == 0 || hour > 12 {
            return None;
        }
        hour = match (hour, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (h, true) => h + 12,
            (h, false) => h,
        };
        tail = spaced.get(2..).unwrap_or("");
    }
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    // A zone in parentheses right after the time: "(Europe/London)".
    let original_tail = &text[text.len() - tail.len()..];
    let zone = original_tail
        .trim_start()
        .strip_prefix('(')
        .and_then(|z| z.split(')').next())
        .and_then(|z| z.trim().parse::<chrono_tz::Tz>().ok());
    match zone {
        Some(tz) => next_moment(&tz, now, date, time),
        None => next_moment(&Local, now, date, time),
    }
}

/// The first `date time` in `tz` after `now` (today or tomorrow without a
/// date; this year or next with one).
fn next_moment<Z: TimeZone>(
    tz: &Z,
    now: DateTime<Utc>,
    date: Option<(u32, u32)>,
    time: NaiveTime,
) -> Option<DateTime<Utc>> {
    let local_now = now.with_timezone(tz);
    let candidates: Vec<NaiveDate> = match date {
        Some((month, day)) => [local_now.year(), local_now.year() + 1]
            .iter()
            .filter_map(|year| NaiveDate::from_ymd_opt(*year, month, day))
            .collect(),
        None => {
            let today = local_now.date_naive();
            vec![today, today.succ_opt()?]
        }
    };
    candidates.into_iter().find_map(|day| {
        let at = tz
            .from_local_datetime(&day.and_time(time))
            .earliest()?
            .with_timezone(&Utc);
        (at > now).then_some(at)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
    }

    #[test]
    fn context_full_is_never_a_limit() {
        for text in [
            "Prompt is too long: 1000123 tokens > 1000000 maximum",
            "Prompt is too long (blocking_limit)",
            "This model's maximum context length is 200000 tokens",
            "context_length_exceeded: rate limit of the context window",
            "The conversation no longer fits in the model's context window.",
        ] {
            assert!(classify(text, now()).is_none(), "{text}");
            assert!(is_context_full(text), "{text}");
        }
    }

    #[test]
    fn provider_wordings_classify_as_limits() {
        for text in [
            "Claude usage limit reached — try again after the limit resets.",
            "You've hit your session limit · resets 4:40pm (Europe/London)",
            "You've hit your usage limit. Upgrade to Pro or try again in 4 days 2 hours 13 minutes.",
            "HTTP 429 Too Many Requests",
            "exceeded retry limit, last status: 429 Too Many Requests",
            "RESOURCE_EXHAUSTED: Quota exceeded for quota metric 'Generate Content'",
            "Rate limit exceeded. Please wait.",
            "You've hit your usage limit for Opus. Switch models or try again later.",
            "usageLimitExceeded",
        ] {
            assert!(classify(text, now()).is_some(), "{text}");
        }
        for text in [
            "boom: the build is broken",
            "Prompt 1004299 tokens",
            "Your credit balance is too low to access the API",
            "insufficient_quota: You exceeded your current quota, check billing",
            "",
        ] {
            assert!(classify(text, now()).is_none(), "{text}");
        }
    }

    #[test]
    fn relative_waits_parse() {
        let at = |text: &str| parse_reset(text, now()).map(|t| t - now());
        assert_eq!(
            at("try again in 4 days 2 hours 13 minutes."),
            Some(Duration::days(4) + Duration::hours(2) + Duration::minutes(13))
        );
        assert_eq!(
            at("Rate limited, resets in 2h 13m"),
            Some(Duration::minutes(133))
        );
        assert_eq!(at("try again in 4h12m"), Some(Duration::minutes(252)));
        assert_eq!(at("retry-after: 120"), Some(Duration::seconds(120)));
        assert_eq!(
            at("Please retry after 90 seconds and 1 minute"),
            Some(Duration::seconds(150))
        );
        assert_eq!(at("try again later"), None);
    }

    #[test]
    fn clock_times_parse_in_their_zone() {
        // 4:40pm London on 15 Oct 2026 (BST, UTC+1) = 15:40 UTC, after noon.
        assert_eq!(
            parse_reset(
                "You've hit your session limit · resets 4:40pm (Europe/London)",
                now()
            ),
            Some(Utc.with_ymd_and_hms(2026, 10, 15, 15, 40, 0).unwrap())
        );
        // 7am New York already passed today (11:00 UTC): tomorrow.
        assert_eq!(
            parse_reset("resets 7am (America/New_York)", now()),
            Some(Utc.with_ymd_and_hms(2026, 10, 16, 11, 0, 0).unwrap())
        );
        // A clock time after "retry after" is not a count of seconds.
        assert_eq!(
            parse_reset("Usage limit reached. Retry after 16:40 (UTC).", now()),
            Some(Utc.with_ymd_and_hms(2026, 10, 15, 16, 40, 0).unwrap())
        );
        // A date names the day.
        assert_eq!(
            parse_reset("weekly limit · resets Oct 20 at 8pm (UTC)", now()),
            Some(Utc.with_ymd_and_hms(2026, 10, 20, 20, 0, 0).unwrap())
        );
        // Bare numbers are not times.
        assert_eq!(parse_reset("resets 5 things", now()), None);
    }

    #[test]
    fn stamps_parse() {
        assert_eq!(
            parse_reset("limit; resets_at=1792087200", now()),
            Some(Utc.timestamp_opt(1_792_087_200, 0).unwrap())
        );
        assert_eq!(
            parse_reset("quota resets 2026-10-15T18:00:00Z", now()),
            Some(Utc.with_ymd_and_hms(2026, 10, 15, 18, 0, 0).unwrap())
        );
    }
}
