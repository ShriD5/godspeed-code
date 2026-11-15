//! Idle compaction: compact a parked Claude chat with a large context shortly
//! before its prompt cache expires.
//!
//! Coming back to a long chat after the cache has gone cold re-reads the
//! whole history at the full cache-write price. Compacting while the cache is
//! still warm reads that history at the cached price once, and the next real
//! turn starts from the summary. T3 Code does the same
//! (`apps/server/src/orchestration-v2/IdleCompaction.ts`).
//!
//! The policy, all of it read off the run the engine already drives:
//!
//! - **When.** The cache expires one lifetime after the last provider request
//!   began: the turn's start, or the last top-level tool result (the CLI sends
//!   the next request right after it). Claude reports the lifetime on every
//!   request that writes cache ([`AgentEvent::PromptCache`]): 1 hour on
//!   subscription logins, 5 minutes on API keys. The compaction starts
//!   [`LEAD`] before expiry, and never once fewer than a quarter of that lead
//!   remains: a compaction against a cold cache pays the full re-read it was
//!   meant to avoid and still loses detail, so it is skipped.
//! - **Only 1-hour caches.** On a 5-minute cache the compaction would start
//!   after three idle minutes, a pause to read the diff. It would fire on
//!   nearly every turn, each time trading the agent's working detail for a
//!   summary, and a user back within the last two minutes would pay for the
//!   compaction instead of a cheap cached read. API-key chats are left alone.
//! - **Only large contexts.** [`threshold`]: half the context window, kept
//!   between 100K (Claude Code's floor for its own idle compaction) and 200K
//!   (its default, and T3's). On the default 200K window that is 100K, below
//!   Claude's own auto-compaction; on a 1M window it is 200K.
//! - **Only idle chats.** The session is parked (no turn, no pending question
//!   or approval), with nothing queued, no accepted message on its way, no
//!   background work or live subagent, no pending agent update. The chat is
//!   not archived, settled, snoozed or usage-limited, and a sub-agent's parent
//!   is not running. A blocker re-checks every [`RETRY`] until the window
//!   closes. One idle spell compacts at most once.
//! - **Reaping.** The 30-minute idle reaper would end a parked 1-hour session
//!   long before its compaction is due. Resuming a reaped session to compact
//!   it rebuilds the system prompt (git status, date, tools), and any
//!   difference misses the cache, so the compaction would pay the full
//!   re-read itself. Instead the reaper waits for a pending compaction, and
//!   once it is done the session is reaped on the original schedule: the
//!   compaction is not activity.
//! - **A user returning mid-compaction.** The live run marks itself
//!   compacting: sends wait in the visible queue
//!   ([`crate::SessionsEngine::defers_to_turn_end`]) and drain once the
//!   compaction ends. A message already in the mailbox runs after it.

use std::time::Duration;

use chrono::{DateTime, Utc};
use zeron_doc::MessagePart;

/// Start this long before the cache expires: time to send the request while
/// the cache is still warm (T3 uses the same lead).
pub(crate) const LEAD: Duration = Duration::from_secs(2 * 60);
/// Context below which a cold re-read is cheap enough to keep the detail.
pub(crate) const MIN_TOKENS_FLOOR: u64 = 100_000;
/// The threshold never asks for more than this, however large the window.
pub(crate) const MIN_TOKENS_CEILING: u64 = 200_000;
/// The shortest cache lifetime worth compacting for (see the module docs).
const MIN_NATURAL_TTL: Duration = Duration::from_secs(30 * 60);
/// A blocked compaction checks again this often while its window is open.
pub(crate) const RETRY: Duration = Duration::from_secs(30);
/// Longest one wait trusts the monotonic clock: it stops while a laptop
/// sleeps, the provider's cache clock does not.
pub(crate) const MAX_SLEEP: Duration = Duration::from_secs(60);
/// A started compaction holds the idle reaper off for at most this long.
pub(crate) const MAX_RUN: Duration = Duration::from_secs(10 * 60);
/// The summary kept on the transcript row, at most.
pub(crate) const SUMMARY_MAX_BYTES: usize = 24 * 1024;
/// What the run's mailbox receives.
pub(crate) const COMMAND: &str = "/compact";

/// Environment overrides, read once per run (tests and the live check).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Knobs {
    /// `GODSPEED_IDLE_COMPACTION`: `0` never compacts, `1` always may,
    /// whatever the setting says.
    pub forced: Option<bool>,
    /// `GODSPEED_IDLE_COMPACT_TTL_MS`: the cache lifetime to plan with,
    /// instead of the one Claude reports. The lead shrinks to half of it.
    pub ttl: Option<Duration>,
    /// `GODSPEED_IDLE_COMPACT_MIN_TOKENS`: the context threshold.
    pub min_tokens: Option<u64>,
}

impl Knobs {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            forced: var("GODSPEED_IDLE_COMPACTION").map(|v| v.trim() != "0"),
            ttl: var("GODSPEED_IDLE_COMPACT_TTL_MS")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis),
            min_tokens: var("GODSPEED_IDLE_COMPACT_MIN_TOKENS")
                .and_then(|v| v.trim().parse::<u64>().ok()),
        }
    }
}

/// The context size worth compacting at, for a context window.
pub(crate) fn threshold(window: Option<u64>, knobs: &Knobs) -> u64 {
    if let Some(min) = knobs.min_tokens {
        return min;
    }
    window.map_or(MIN_TOKENS_FLOOR, |w| {
        (w / 2).clamp(MIN_TOKENS_FLOOR, MIN_TOKENS_CEILING)
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Parked or working; a compaction may be planned.
    Waiting,
    /// `/compact` sent, its turn not started yet.
    Requested,
    /// The compaction's turn is streaming.
    Running,
    /// This idle spell is done: compacted, skipped or failed. The next real
    /// turn starts over.
    Spent,
}

/// The run loop's view of one runtime's cache and context. Wall-clock times
/// throughout (see [`MAX_SLEEP`]).
#[derive(Debug)]
pub(crate) struct Tracker {
    knobs: Knobs,
    ttl: Option<Duration>,
    last_request: Option<DateTime<Utc>>,
    tokens: Option<u64>,
    window: Option<u64>,
    phase: Phase,
    retry_at: Option<DateTime<Utc>>,
    started: Option<tokio::time::Instant>,
    pre_tokens: Option<u64>,
    summary: Option<String>,
}

impl Tracker {
    pub fn new(knobs: Knobs) -> Self {
        Self {
            knobs,
            ttl: None,
            last_request: None,
            tokens: None,
            window: None,
            phase: Phase::Waiting,
            retry_at: None,
            started: None,
            pre_tokens: None,
            summary: None,
        }
    }

    pub fn knobs(&self) -> &Knobs {
        &self.knobs
    }

    /// A real turn began (a prompt, a steer, a self-continued wake): its
    /// first request starts the cache clock, and a new idle spell may compact.
    pub fn turn_started(&mut self, at: DateTime<Utc>) {
        self.last_request = Some(at);
        if self.phase == Phase::Spent {
            self.phase = Phase::Waiting;
        }
        self.retry_at = None;
    }

    /// A top-level tool result: the CLI sends the next request with it.
    pub fn tool_result(&mut self, at: DateTime<Utc>) {
        if !self.compacting() {
            self.last_request = Some(at);
        }
    }

    /// Claude's reported cache lifetime. A request that wrote nothing reports
    /// none, and the last one stands.
    pub fn note_ttl(&mut self, secs: u64) {
        self.ttl = Some(Duration::from_secs(secs));
    }

    /// The context measurement (`None` fields keep the previous value).
    pub fn note_context(&mut self, tokens: Option<u64>, window: Option<u64>) {
        if tokens.is_some() {
            self.tokens = tokens;
        }
        if window.is_some() {
            self.window = window;
        }
    }

    /// The summary a compaction kept; only an idle compaction's is shown.
    pub fn note_summary(&mut self, text: &str) {
        if self.compacting() {
            self.summary = Some(cap(&readable_summary(text), SUMMARY_MAX_BYTES));
        }
    }

    fn effective_ttl(&self) -> Option<Duration> {
        match self.knobs.ttl {
            Some(ttl) => Some(ttl),
            None => self.ttl.filter(|ttl| *ttl >= MIN_NATURAL_TTL),
        }
    }

    fn lead(&self, ttl: Duration) -> Duration {
        LEAD.min(ttl / 2)
    }

    /// When the cache of the last request expires, if it can be known.
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        let ttl = self.effective_ttl()?;
        Some(self.last_request? + chrono::Duration::from_std(ttl).ok()?)
    }

    /// When to compact, while one is still to come this idle spell: the lead
    /// before expiry (or the next retry), for a context over the threshold.
    pub fn due_at(&self) -> Option<DateTime<Utc>> {
        if self.phase != Phase::Waiting {
            return None;
        }
        if self.tokens.unwrap_or(0) < threshold(self.window, &self.knobs) {
            return None;
        }
        let ttl = self.effective_ttl()?;
        let expires = self.expires_at()?;
        let due = expires - chrono::Duration::from_std(self.lead(ttl)).ok()?;
        Some(self.retry_at.map_or(due, |retry| retry.max(due)))
    }

    /// Whether a compaction started at `now` still beats the expiry: a
    /// quarter of the lead must remain.
    pub fn still_warm(&self, now: DateTime<Utc>) -> bool {
        let (Some(ttl), Some(expires)) = (self.effective_ttl(), self.expires_at()) else {
            return false;
        };
        let margin = chrono::Duration::from_std(self.lead(ttl) / 4).unwrap_or_default();
        now < expires - margin
    }

    /// Something blocks the compaction now: look again after [`RETRY`].
    pub fn retry_later(&mut self, now: DateTime<Utc>) {
        self.retry_at = Some(now + chrono::Duration::from_std(RETRY).unwrap_or_default());
    }

    /// Give up on this idle spell.
    pub fn skip(&mut self) {
        self.phase = Phase::Spent;
        self.retry_at = None;
    }

    /// `/compact` went into the mailbox.
    pub fn requested(&mut self) {
        self.phase = Phase::Requested;
        self.started = Some(tokio::time::Instant::now());
        self.pre_tokens = self.tokens;
        self.summary = None;
    }

    /// The compaction's turn started (its steer was confirmed).
    pub fn running(&mut self) {
        if self.phase == Phase::Requested {
            self.phase = Phase::Running;
        }
    }

    /// `/compact` was sent and its turn has not ended.
    pub fn compacting(&self) -> bool {
        matches!(self.phase, Phase::Requested | Phase::Running)
    }

    /// The compaction's turn is streaming.
    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// A compaction in flight holds the idle reaper off, for a while.
    pub fn holds_reaper(&self) -> bool {
        self.compacting() && self.started.is_some_and(|at| at.elapsed() < MAX_RUN)
    }

    /// The compaction ended: the transcript row for its turn, built from what
    /// the turn folded. `error` is the turn's own failure, if it had one.
    pub fn finish(&mut self, folded: &[MessagePart], error: Option<String>) -> MessagePart {
        self.phase = Phase::Spent;
        self.started = None;
        let error = error.or_else(|| {
            folded.iter().find_map(|part| match part {
                MessagePart::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
        });
        let error = error.map(|e| {
            e.trim()
                .trim_start_matches("Context compaction failed:")
                .trim()
                .to_owned()
        });
        MessagePart::Compaction {
            id: "c0".into(),
            summary: if error.is_some() {
                None
            } else {
                self.summary.take()
            },
            pre_tokens: self.pre_tokens.take(),
            post_tokens: if error.is_some() { None } else { self.tokens },
            error: error.map(|e| {
                if e.is_empty() {
                    "the compaction failed".to_owned()
                } else {
                    e
                }
            }),
        }
    }
}

/// The summary as a person reads it: without the `<system-reminder>` blocks
/// Claude Code folds in for the model.
fn readable_summary(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    let mut tidy = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.trim().lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim_end().to_owned()
}

/// `text`, cut to at most `max` bytes on a character boundary.
fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn tracker() -> Tracker {
        let mut t = Tracker::new(Knobs::default());
        t.note_ttl(3600);
        t.note_context(Some(150_000), Some(200_000));
        t.turn_started(at(0));
        t
    }

    #[test]
    fn threshold_is_half_the_window_between_100k_and_200k() {
        let knobs = Knobs::default();
        assert_eq!(threshold(None, &knobs), 100_000);
        assert_eq!(threshold(Some(200_000), &knobs), 100_000);
        assert_eq!(threshold(Some(300_000), &knobs), 150_000);
        assert_eq!(threshold(Some(1_000_000), &knobs), 200_000);
        let knobs = Knobs {
            min_tokens: Some(5),
            ..Knobs::default()
        };
        assert_eq!(threshold(Some(1_000_000), &knobs), 5);
    }

    #[test]
    fn a_large_idle_chat_compacts_two_minutes_before_its_hour_runs_out() {
        let t = tracker();
        assert_eq!(t.expires_at(), Some(at(3600)));
        assert_eq!(t.due_at(), Some(at(3600 - 120)));
        assert!(t.still_warm(at(3600 - 120)));
        // A quarter of the lead (30 s) must remain.
        assert!(t.still_warm(at(3600 - 31)));
        assert!(!t.still_warm(at(3600 - 30)));
        assert!(!t.still_warm(at(3600 + 5)));
    }

    #[test]
    fn the_clock_runs_from_the_last_request() {
        let mut t = tracker();
        t.tool_result(at(600));
        assert_eq!(t.due_at(), Some(at(600 + 3600 - 120)));
    }

    #[test]
    fn small_contexts_and_api_key_caches_never_compact() {
        let mut t = tracker();
        t.note_context(Some(99_000), None);
        assert_eq!(t.due_at(), None);
        // A 1M window needs 200K.
        t.note_context(Some(150_000), Some(1_000_000));
        assert_eq!(t.due_at(), None);
        t.note_context(Some(250_000), None);
        assert!(t.due_at().is_some());
        // An API key's 5-minute cache.
        let mut t = tracker();
        t.note_ttl(300);
        assert_eq!(t.due_at(), None);
        // Nothing reported yet.
        let mut t = Tracker::new(Knobs::default());
        t.note_context(Some(150_000), None);
        t.turn_started(at(0));
        assert_eq!(t.due_at(), None);
    }

    #[test]
    fn the_ttl_override_plans_with_half_its_length_as_the_lead() {
        let mut t = Tracker::new(Knobs {
            ttl: Some(Duration::from_secs(20)),
            ..Knobs::default()
        });
        t.note_ttl(300);
        t.note_context(Some(150_000), None);
        t.turn_started(at(0));
        assert_eq!(t.due_at(), Some(at(10)));
    }

    #[test]
    fn one_idle_spell_compacts_once_and_a_new_turn_starts_over() {
        let mut t = tracker();
        t.requested();
        assert!(t.compacting() && t.holds_reaper());
        assert_eq!(t.due_at(), None);
        t.running();
        t.note_summary("kept: the plan");
        t.note_context(Some(9_000), None);
        let part = t.finish(&[], None);
        assert_eq!(
            part,
            MessagePart::Compaction {
                id: "c0".into(),
                summary: Some("kept: the plan".into()),
                pre_tokens: Some(150_000),
                post_tokens: Some(9_000),
                error: None,
            }
        );
        assert_eq!(t.due_at(), None);
        // Small after compacting: the next spell stays put too.
        t.turn_started(at(4000));
        assert_eq!(t.due_at(), None);
        t.note_context(Some(120_000), None);
        assert_eq!(t.due_at(), Some(at(4000 + 3600 - 120)));
    }

    #[test]
    fn a_blocked_compaction_retries_while_the_window_is_open() {
        let mut t = tracker();
        t.retry_later(at(3480));
        assert_eq!(t.due_at(), Some(at(3510)));
        t.skip();
        assert_eq!(t.due_at(), None);
    }

    #[test]
    fn a_failed_compaction_names_the_reason_and_keeps_no_summary() {
        let mut t = tracker();
        t.requested();
        t.running();
        t.note_summary("partial");
        let part = t.finish(
            &[MessagePart::Error {
                id: "e0".into(),
                message: "Context compaction failed: Prompt is too long".into(),
            }],
            None,
        );
        assert_eq!(
            part,
            MessagePart::Compaction {
                id: "c0".into(),
                summary: None,
                pre_tokens: Some(150_000),
                post_tokens: None,
                error: Some("Prompt is too long".into()),
            }
        );
    }

    #[test]
    fn summaries_drop_the_model_only_reminders() {
        assert_eq!(
            readable_summary(
                "Continued from before.\n\n<system-reminder>\n<tools-used-before-summary>Read</tools-used-before-summary>\n</system-reminder>\nSummary:\n1. Fixed it."
            ),
            "Continued from before.\n\nSummary:\n1. Fixed it."
        );
        assert_eq!(readable_summary("plain"), "plain");
    }

    #[test]
    fn summaries_are_capped_on_a_character_boundary() {
        let text = "é".repeat(SUMMARY_MAX_BYTES);
        let capped = cap(&text, SUMMARY_MAX_BYTES);
        assert!(capped.len() <= SUMMARY_MAX_BYTES + '…'.len_utf8());
        assert!(capped.ends_with('…'));
    }
}
