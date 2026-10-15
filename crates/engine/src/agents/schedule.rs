//! Scheduler math for timer triggers — pure functions of (trigger, persisted
//! state, now), so restarts recompute the same answer and tests drive any
//! clock they like.

use std::str::FromStr as _;

use chrono::{DateTime, TimeDelta, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use zeron_proto::agents::AgentTrigger;

/// A fire missed by more than this is recorded as skipped instead of run
/// late: a 09:00 report showing up at 14:00 after a laptop wakes is noise.
pub(crate) const MISFIRE_GRACE: TimeDelta = TimeDelta::minutes(15);
/// Shortest interval an agent may ask for. Each fire is a full agent run.
pub(crate) const MIN_INTERVAL_SECS: u64 = 60;
/// Bound on the backlog walk that only feeds the `collapsed` counter.
const MAX_BACKLOG_WALK: u32 = 10_000;

/// A parsed timer trigger. Event triggers (webhook, file watch) have none.
#[derive(Debug, Clone)]
pub(crate) enum Schedule {
    Cron { cron: Box<Cron>, tz: chrono_tz::Tz },
    Interval { every: TimeDelta },
    Once { at: DateTime<Utc> },
}

impl Schedule {
    /// `Ok(None)` for event triggers; `Err` is a user-facing validation message.
    pub(crate) fn parse(trigger: &AgentTrigger) -> Result<Option<Self>, String> {
        match trigger {
            AgentTrigger::Cron { expr, timezone } => {
                let cron = CronParser::builder()
                    .seconds(Seconds::Disallowed)
                    .year(Year::Disallowed)
                    .build()
                    .parse(expr.trim())
                    .map_err(|e| format!("Invalid cron expression {expr:?}: {e}"))?;
                let tz = chrono_tz::Tz::from_str(timezone.trim()).map_err(|_| {
                    format!("Unknown time zone {timezone:?} (use an IANA name like Europe/Berlin)")
                })?;
                Ok(Some(Self::Cron {
                    cron: Box::new(cron),
                    tz,
                }))
            }
            AgentTrigger::Interval { every_secs } => {
                if *every_secs < MIN_INTERVAL_SECS {
                    return Err(format!(
                        "Interval must be at least {MIN_INTERVAL_SECS} seconds"
                    ));
                }
                let every = i64::try_from(*every_secs)
                    .ok()
                    .and_then(TimeDelta::try_seconds)
                    .ok_or_else(|| "Interval is too long".to_string())?;
                Ok(Some(Self::Interval { every }))
            }
            AgentTrigger::Once { at } => Ok(Some(Self::Once { at: *at })),
            AgentTrigger::Webhook | AgentTrigger::FileWatch { .. } => Ok(None),
        }
    }

    /// The first occurrence strictly after `after`. Intervals count from
    /// `anchor` (the enable time) so a restart never shifts the grid; a
    /// one-shot that has not been handled yet is always "next", even in the
    /// past, so a missed one is reported rather than silently dropped.
    pub(crate) fn next_after(
        &self,
        after: DateTime<Utc>,
        anchor: DateTime<Utc>,
        handled: bool,
    ) -> Option<DateTime<Utc>> {
        match self {
            Self::Cron { cron, tz } => cron
                .find_next_occurrence(&after.with_timezone(tz), false)
                .ok()
                .map(|t| t.with_timezone(&Utc)),
            Self::Interval { every } => {
                let k = if after < anchor {
                    1
                } else {
                    (after - anchor).num_milliseconds() / every.num_milliseconds() + 1
                };
                anchor.checked_add_signed(every.checked_mul(i32::try_from(k).ok()?)?)
            }
            Self::Once { at } => (!handled).then_some(*at),
        }
    }

    /// The newest occurrence at or before `now`, given the first due one.
    fn latest_due(&self, first: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Cron { cron, tz } => cron
                .find_previous_occurrence(&now.with_timezone(tz), true)
                .ok()
                .map(|t| t.with_timezone(&Utc))
                .filter(|t| *t >= first)
                .unwrap_or(first),
            Self::Interval { every } => {
                let steps = (now - first).num_milliseconds() / every.num_milliseconds();
                i32::try_from(steps)
                    .ok()
                    .and_then(|steps| every.checked_mul(steps))
                    .and_then(|delta| first.checked_add_signed(delta))
                    .unwrap_or(first)
            }
            Self::Once { .. } => first,
        }
    }
}

/// Persisted per-trigger state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TriggerState {
    /// When the trigger (re)started counting: creation, edit or re-enable.
    pub anchor: DateTime<Utc>,
    /// The newest occurrence already handled (fired or recorded as skipped).
    pub last_scheduled: Option<DateTime<Utc>>,
}

impl TriggerState {
    pub(crate) fn next_fire(&self, schedule: &Schedule) -> Option<DateTime<Utc>> {
        schedule.next_after(
            self.last_scheduled.unwrap_or(self.anchor),
            self.anchor,
            self.last_scheduled.is_some(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Due {
    /// Nothing due; sleep until `next` (`None` = never again).
    Idle { next: Option<DateTime<Utc>> },
    /// Run now for `scheduled_at`; `collapsed` earlier occurrences fold into it.
    Fire {
        scheduled_at: DateTime<Utc>,
        collapsed: u32,
    },
    /// The newest due occurrence is past the grace window: record one skip
    /// for it and everything before it.
    Missed {
        scheduled_at: DateTime<Utc>,
        collapsed: u32,
    },
}

/// What a timer trigger owes at `now`. A backlog (engine down, laptop
/// asleep) always resolves to a single run or a single skip for the newest
/// occurrence — never a burst of catch-up runs.
pub(crate) fn evaluate(schedule: &Schedule, state: &TriggerState, now: DateTime<Utc>) -> Due {
    let Some(first) = state.next_fire(schedule) else {
        return Due::Idle { next: None };
    };
    if first > now {
        return Due::Idle { next: Some(first) };
    }
    let latest = schedule.latest_due(first, now);
    let collapsed = count_between(schedule, state, first, latest);
    if now - latest > MISFIRE_GRACE {
        Due::Missed {
            scheduled_at: latest,
            collapsed,
        }
    } else {
        Due::Fire {
            scheduled_at: latest,
            collapsed,
        }
    }
}

/// Occurrences in `[first, latest)` — the ones folded into `latest`.
fn count_between(
    schedule: &Schedule,
    state: &TriggerState,
    first: DateTime<Utc>,
    latest: DateTime<Utc>,
) -> u32 {
    if let Schedule::Interval { every } = schedule {
        return u32::try_from((latest - first).num_milliseconds() / every.num_milliseconds())
            .unwrap_or(u32::MAX);
    }
    let mut count = 0;
    let mut cursor = first;
    while cursor < latest && count < MAX_BACKLOG_WALK {
        count += 1;
        match schedule.next_after(cursor, state.anchor, true) {
            Some(next) => cursor = next,
            None => break,
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn cron(expr: &str, tz: &str) -> Schedule {
        Schedule::parse(&AgentTrigger::Cron {
            expr: expr.into(),
            timezone: tz.into(),
        })
        .unwrap()
        .unwrap()
    }

    fn fresh(anchor: DateTime<Utc>) -> TriggerState {
        TriggerState {
            anchor,
            last_scheduled: None,
        }
    }

    #[test]
    fn rejects_bad_cron_timezone_seconds_and_short_intervals() {
        let parse = |t: AgentTrigger| Schedule::parse(&t).map(|_| ());
        assert!(
            parse(AgentTrigger::Cron {
                expr: "61 * * * *".into(),
                timezone: "UTC".into()
            })
            .is_err()
        );
        assert!(
            parse(AgentTrigger::Cron {
                expr: "0 0 9 * * *".into(),
                timezone: "UTC".into()
            })
            .is_err(),
            "6-field (seconds) patterns are not 5-field cron"
        );
        let err = parse(AgentTrigger::Cron {
            expr: "0 9 * * *".into(),
            timezone: "Mars/Olympus".into(),
        })
        .unwrap_err();
        assert!(err.contains("time zone"), "{err}");
        assert!(parse(AgentTrigger::Interval { every_secs: 59 }).is_err());
        assert!(parse(AgentTrigger::Interval { every_secs: 60 }).is_ok());
        assert!(matches!(Schedule::parse(&AgentTrigger::Webhook), Ok(None)));
    }

    #[test]
    fn cron_follows_wall_clock_in_its_timezone_across_dst() {
        // 09:00 Berlin is 08:00 UTC in winter (CET) and 07:00 UTC in summer.
        let s = cron("0 9 * * *", "Europe/Berlin");
        let winter = s.next_after(utc(2026, 3, 27, 12, 0), utc(2026, 1, 1, 0, 0), true);
        assert_eq!(winter, Some(utc(2026, 3, 28, 8, 0)));
        // DST starts 2026-03-29 02:00 local.
        let summer = s.next_after(utc(2026, 3, 28, 12, 0), utc(2026, 1, 1, 0, 0), true);
        assert_eq!(summer, Some(utc(2026, 3, 29, 7, 0)));
    }

    #[test]
    fn cron_in_the_spring_gap_runs_right_after_it_and_fall_back_runs_once() {
        // New York springs forward 2026-03-08 02:00 → 03:00: 02:30 does not
        // exist, so a fixed-time job runs at the first valid minute (03:00 EDT).
        let s = cron("30 2 * * *", "America/New_York");
        let anchor = utc(2026, 1, 1, 0, 0);
        let next = s.next_after(utc(2026, 3, 8, 5, 0), anchor, true).unwrap();
        assert_eq!(next, utc(2026, 3, 8, 7, 0), "03:00 EDT = 07:00 UTC");

        // Falls back 2026-11-01 02:00 → 01:00: 01:30 happens twice; a fixed
        // job fires once, at the first (EDT) pass.
        let s = cron("30 1 * * *", "America/New_York");
        let first = s.next_after(utc(2026, 11, 1, 4, 0), anchor, true).unwrap();
        assert_eq!(first, utc(2026, 11, 1, 5, 30), "01:30 EDT = 05:30 UTC");
        let after = s.next_after(first, anchor, true).unwrap();
        assert_eq!(after, utc(2026, 11, 2, 6, 30), "next day's 01:30 EST");
    }

    #[test]
    fn interval_grid_is_anchored_and_survives_restarts() {
        let s = Schedule::parse(&AgentTrigger::Interval { every_secs: 600 })
            .unwrap()
            .unwrap();
        let anchor = utc(2026, 5, 1, 10, 3);
        assert_eq!(fresh(anchor).next_fire(&s), Some(utc(2026, 5, 1, 10, 13)));
        // Recomputed from persisted state only: same grid after a restart.
        let state = TriggerState {
            anchor,
            last_scheduled: Some(utc(2026, 5, 1, 10, 33)),
        };
        assert_eq!(state.next_fire(&s), Some(utc(2026, 5, 1, 10, 43)));
    }

    #[test]
    fn due_fire_within_grace_and_skip_beyond_it() {
        let s = cron("0 9 * * *", "UTC");
        let state = TriggerState {
            anchor: utc(2026, 6, 1, 0, 0),
            last_scheduled: Some(utc(2026, 6, 1, 9, 0)),
        };
        assert_eq!(
            evaluate(&s, &state, utc(2026, 6, 2, 8, 59)),
            Due::Idle {
                next: Some(utc(2026, 6, 2, 9, 0))
            }
        );
        assert_eq!(
            evaluate(&s, &state, utc(2026, 6, 2, 9, 14)),
            Due::Fire {
                scheduled_at: utc(2026, 6, 2, 9, 0),
                collapsed: 0
            },
            "14 minutes late is inside the 15 minute grace"
        );
        assert_eq!(
            evaluate(&s, &state, utc(2026, 6, 2, 9, 16)),
            Due::Missed {
                scheduled_at: utc(2026, 6, 2, 9, 0),
                collapsed: 0
            }
        );
    }

    #[test]
    fn backlog_collapses_into_the_newest_occurrence() {
        // Hourly job, engine down from 09:30 to 14:05: one run for 14:00
        // standing in for 10:00..13:00.
        let s = cron("0 * * * *", "UTC");
        let state = TriggerState {
            anchor: utc(2026, 6, 1, 0, 0),
            last_scheduled: Some(utc(2026, 6, 1, 9, 0)),
        };
        assert_eq!(
            evaluate(&s, &state, utc(2026, 6, 1, 14, 5)),
            Due::Fire {
                scheduled_at: utc(2026, 6, 1, 14, 0),
                collapsed: 4
            }
        );
        // Down until 14:40: even the newest is past grace → one skip row.
        assert_eq!(
            evaluate(&s, &state, utc(2026, 6, 1, 14, 40)),
            Due::Missed {
                scheduled_at: utc(2026, 6, 1, 14, 0),
                collapsed: 4
            }
        );
        // Intervals collapse the same way.
        let every = Schedule::parse(&AgentTrigger::Interval { every_secs: 300 })
            .unwrap()
            .unwrap();
        let state = fresh(utc(2026, 6, 1, 9, 0));
        assert_eq!(
            evaluate(&every, &state, utc(2026, 6, 1, 9, 31)),
            Due::Fire {
                scheduled_at: utc(2026, 6, 1, 9, 30),
                collapsed: 5
            }
        );
    }

    #[test]
    fn one_shot_fires_once_and_a_missed_one_is_reported() {
        let at = utc(2026, 7, 4, 12, 0);
        let s = Schedule::parse(&AgentTrigger::Once { at })
            .unwrap()
            .unwrap();
        // Created after its time and only 5 minutes late: still runs.
        let state = fresh(utc(2026, 7, 4, 12, 5));
        assert_eq!(
            evaluate(&s, &state, utc(2026, 7, 4, 12, 5)),
            Due::Fire {
                scheduled_at: at,
                collapsed: 0
            }
        );
        assert_eq!(
            evaluate(&s, &state, utc(2026, 7, 5, 0, 0)),
            Due::Missed {
                scheduled_at: at,
                collapsed: 0
            }
        );
        let handled = TriggerState {
            anchor: state.anchor,
            last_scheduled: Some(at),
        };
        assert_eq!(
            evaluate(&s, &handled, utc(2026, 7, 5, 0, 0)),
            Due::Idle { next: None }
        );
    }
}
