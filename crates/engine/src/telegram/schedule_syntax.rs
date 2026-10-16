//! `/schedule …` into an always-on agent trigger. Clock times are read in
//! the chat's own time zone (cron triggers carry it), not UTC.

use zeron_proto::agents::AgentTrigger;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScheduleSpec {
    pub trigger: AgentTrigger,
    /// "daily 09:00", for lists and confirmations.
    pub label: String,
    pub prompt: String,
}

const MIN_INTERVAL_MINUTES: u64 = 1;
const MAX_INTERVAL_MINUTES: u64 = 7 * 24 * 60;
const WEEKDAYS: [(&str, &str, u8); 7] = [
    ("sun", "Sun", 0),
    ("mon", "Mon", 1),
    ("tue", "Tue", 2),
    ("wed", "Wed", 3),
    ("thu", "Thu", 4),
    ("fri", "Fri", 5),
    ("sat", "Sat", 6),
];

pub(crate) fn usage(timezone: &str) -> String {
    format!(
        "Usage:\n\
         /schedule hourly <prompt>\n\
         /schedule daily HH:MM <prompt>\n\
         /schedule weekdays HH:MM <prompt>\n\
         /schedule weekly <mon..sun> HH:MM <prompt>\n\
         /schedule every 30m <prompt>  (or 2h)\n\n\
         Times are in {timezone}; change it with /tz Area/City.\n\
         Example: /schedule weekdays 09:00 Summarize yesterday's merged PRs"
    )
}

fn clock(raw: &str) -> Option<(u32, u32)> {
    let (hour, minute) = raw.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    (hour < 24 && minute < 60 && raw.len() <= 5).then_some((hour, minute))
}

fn weekday(raw: &str) -> Option<(&'static str, u8)> {
    let raw = raw.to_ascii_lowercase();
    WEEKDAYS
        .iter()
        .find(|(short, _, _)| raw.starts_with(short) && raw.len() <= 9)
        .map(|(_, label, number)| (*label, *number))
}

fn interval_minutes(raw: &str) -> Option<u64> {
    let raw = raw.to_ascii_lowercase();
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    let count: u64 = digits.parse().ok()?;
    let minutes = match &raw[digits.len()..] {
        "m" | "min" | "mins" | "minute" | "minutes" => count,
        "h" | "hr" | "hrs" | "hour" | "hours" => count.checked_mul(60)?,
        _ => return None,
    };
    (MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES)
        .contains(&minutes)
        .then_some(minutes)
}

fn cron(expr: String, timezone: &str) -> AgentTrigger {
    AgentTrigger::Cron {
        expr,
        timezone: timezone.to_string(),
    }
}

/// Parse the words after `/schedule`. The error is the user-facing reply.
pub(crate) fn parse(args: &str, timezone: &str) -> Result<ScheduleSpec, String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let bad = || usage(timezone);
    let rest = |from: usize| words.get(from..).map(|w| w.join(" ")).unwrap_or_default();
    let (trigger, label, prompt) = match words.first().map(|w| w.to_ascii_lowercase()).as_deref() {
        Some("hourly") => (cron("0 * * * *".into(), timezone), "hourly".into(), rest(1)),
        Some(kind @ ("daily" | "weekdays")) => {
            let (hour, minute) = words.get(1).and_then(|w| clock(w)).ok_or_else(bad)?;
            let days = if kind == "daily" { "*" } else { "1-5" };
            (
                cron(format!("{minute} {hour} * * {days}"), timezone),
                format!("{kind} {hour:02}:{minute:02}"),
                rest(2),
            )
        }
        Some("weekly") => {
            let (day, number) = words.get(1).and_then(|w| weekday(w)).ok_or_else(bad)?;
            let (hour, minute) = words.get(2).and_then(|w| clock(w)).ok_or_else(bad)?;
            (
                cron(format!("{minute} {hour} * * {number}"), timezone),
                format!("weekly {day} {hour:02}:{minute:02}"),
                rest(3),
            )
        }
        Some("every") => {
            let minutes = words
                .get(1)
                .and_then(|w| interval_minutes(w))
                .ok_or_else(bad)?;
            let label = if minutes % 60 == 0 {
                format!("every {}h", minutes / 60)
            } else {
                format!("every {minutes}m")
            };
            (
                AgentTrigger::Interval {
                    every_secs: minutes * 60,
                },
                label,
                rest(2),
            )
        }
        _ => return Err(bad()),
    };
    if prompt.trim().is_empty() {
        return Err(bad());
    }
    Ok(ScheduleSpec {
        trigger,
        label,
        prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TZ: &str = "Asia/Kolkata";

    fn cron_of(spec: &ScheduleSpec) -> (&str, &str) {
        match &spec.trigger {
            AgentTrigger::Cron { expr, timezone } => (expr, timezone),
            other => panic!("expected cron, got {other:?}"),
        }
    }

    #[test]
    fn clock_schedules_become_cron_in_the_chat_zone() {
        let daily = parse("daily 9:05 Summarize PRs", TZ).unwrap();
        assert_eq!(cron_of(&daily), ("5 9 * * *", TZ));
        assert_eq!(daily.label, "daily 09:05");
        assert_eq!(daily.prompt, "Summarize PRs");

        let weekdays = parse("weekdays 18:30 Wrap up", TZ).unwrap();
        assert_eq!(cron_of(&weekdays).0, "30 18 * * 1-5");

        let weekly = parse("weekly Friday 08:00 Release notes", TZ).unwrap();
        assert_eq!(cron_of(&weekly).0, "0 8 * * 5");
        assert_eq!(weekly.label, "weekly Fri 08:00");

        let sunday = parse("weekly sun 23:59 Backup", TZ).unwrap();
        assert_eq!(cron_of(&sunday).0, "59 23 * * 0");

        let hourly = parse("hourly Check the queue", TZ).unwrap();
        assert_eq!(cron_of(&hourly).0, "0 * * * *");
    }

    #[test]
    fn every_is_an_interval() {
        let spec = parse("every 30m Poll the deploy", TZ).unwrap();
        assert_eq!(spec.trigger, AgentTrigger::Interval { every_secs: 1800 });
        assert_eq!(spec.label, "every 30m");
        let spec = parse("every 2h Triage issues", TZ).unwrap();
        assert_eq!(spec.trigger, AgentTrigger::Interval { every_secs: 7200 });
        assert_eq!(spec.label, "every 2h");
    }

    #[test]
    fn malformed_schedules_explain_the_syntax() {
        for bad in [
            "",
            "daily",
            "daily 25:00 x",
            "daily 09:60 x",
            "daily 09:00",
            "weekly someday 09:00 x",
            "weekly mon x",
            "every 0m x",
            "every 30s x",
            "monthly 09:00 x",
        ] {
            let err = parse(bad, TZ).unwrap_err();
            assert!(err.contains("Usage"), "{bad}: {err}");
            assert!(err.contains(TZ), "{err}");
        }
    }
}
