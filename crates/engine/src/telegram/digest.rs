//! Burst control for approval and question cards. A few at a time each get
//! their own card, instantly; when more than [`BURST`] would land in a
//! [`WINDOW`], the waiting ones go out as one digest message with a row of
//! buttons per item instead. Cards for the user's own Telegram threads are
//! never held: that conversation is happening right now.

use chrono::{DateTime, TimeDelta, Utc};

/// Cards a chat may get within [`WINDOW`] before they fold into a digest.
pub(super) const BURST: usize = 3;
pub(super) const WINDOW: TimeDelta = TimeDelta::seconds(60);
/// Over the burst, a lone card waits this long for company before it goes
/// out on its own.
pub(super) const GATHER: TimeDelta = TimeDelta::seconds(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Plan {
    /// Nothing to send yet.
    Hold,
    /// One card per waiting item.
    Cards,
    /// All waiting items in one digest.
    Digest,
}

/// What to do with the cards waiting for one Telegram chat, given when its
/// recent cards and digests went out and when each waiting item arrived.
pub(super) fn plan(sent: &[DateTime<Utc>], waiting: &[DateTime<Utc>], now: DateTime<Utc>) -> Plan {
    let Some(oldest) = waiting.iter().min() else {
        return Plan::Hold;
    };
    let recent = sent.iter().filter(|at| now - **at < WINDOW).count();
    if recent + waiting.len() <= BURST {
        Plan::Cards
    } else if waiting.len() > 1 {
        Plan::Digest
    } else if now - *oldest >= GATHER {
        Plan::Cards
    } else {
        Plan::Hold
    }
}

/// Forget sends older than the window.
pub(super) fn prune(sent: &mut Vec<DateTime<Utc>>, now: DateTime<Utc>) {
    sent.retain(|at| now - *at < WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 9, 0, 0).unwrap() + TimeDelta::seconds(secs)
    }

    #[test]
    fn a_few_go_out_one_by_one_instantly() {
        assert_eq!(plan(&[], &[], at(0)), Plan::Hold);
        assert_eq!(plan(&[], &[at(0)], at(0)), Plan::Cards);
        assert_eq!(plan(&[at(0)], &[at(1), at(1)], at(1)), Plan::Cards);
        // Sends older than a minute don't count.
        assert_eq!(plan(&[at(0), at(1), at(2)], &[at(70)], at(70)), Plan::Cards);
    }

    #[test]
    fn more_than_three_in_a_minute_fold_into_a_digest() {
        assert_eq!(plan(&[], &[at(0); 4], at(0)), Plan::Digest);
        assert_eq!(plan(&[at(0), at(5)], &[at(9), at(9)], at(9)), Plan::Digest);
        let three = [at(0), at(1), at(2)];
        assert_eq!(plan(&three, &[at(3), at(4)], at(4)), Plan::Digest);
    }

    #[test]
    fn a_lone_card_over_the_burst_waits_for_company_then_goes_alone() {
        let three = [at(0), at(1), at(2)];
        assert_eq!(plan(&three, &[at(3)], at(3)), Plan::Hold);
        assert_eq!(plan(&three, &[at(3)], at(12)), Plan::Hold);
        assert_eq!(plan(&three, &[at(3)], at(13)), Plan::Cards);
    }

    #[test]
    fn prune_keeps_the_window() {
        let mut sent = vec![at(0), at(30), at(61)];
        prune(&mut sent, at(75));
        assert_eq!(sent, [at(30), at(61)]);
    }
}
