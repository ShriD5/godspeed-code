//! One-time link codes: Settings shows one, a Telegram chat sends
//! `/link CODE`, and that chat is linked. One code is live at a time; it
//! dies on use, after ten minutes, or after a few wrong guesses.

use chrono::{DateTime, TimeDelta, Utc};
use zeron_proto::telegram::TelegramLinkCode;

pub(crate) const CODE_TTL: TimeDelta = TimeDelta::minutes(10);
const CODE_LEN: usize = 6;
/// Wrong codes tolerated before the live one is withdrawn.
const MAX_FAILURES: u32 = 5;
/// No 0/O or 1/I: the code is read off one screen and typed on another.
/// 32 symbols, so a random byte maps onto it without bias.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claim {
    Linked,
    Invalid,
    Expired,
}

struct Pending {
    code: String,
    expires_at: DateTime<Utc>,
    failures: u32,
}

#[derive(Default)]
pub(crate) struct LinkCodes {
    pending: Option<Pending>,
}

fn random_code() -> String {
    uuid::Uuid::new_v4().as_bytes()[..CODE_LEN]
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
        .collect()
}

impl LinkCodes {
    pub fn issue(&mut self, now: DateTime<Utc>) -> TelegramLinkCode {
        let pending = Pending {
            code: random_code(),
            expires_at: now + CODE_TTL,
            failures: 0,
        };
        let issued = TelegramLinkCode {
            code: pending.code.clone(),
            expires_at: pending.expires_at,
        };
        self.pending = Some(pending);
        issued
    }

    pub fn current(&self, now: DateTime<Utc>) -> Option<TelegramLinkCode> {
        self.pending
            .as_ref()
            .filter(|p| p.expires_at > now)
            .map(|p| TelegramLinkCode {
                code: p.code.clone(),
                expires_at: p.expires_at,
            })
    }

    pub fn claim(&mut self, attempt: &str, now: DateTime<Utc>) -> Claim {
        let attempt = attempt.trim().to_ascii_uppercase();
        let Some(pending) = self.pending.as_mut() else {
            return Claim::Invalid;
        };
        if pending.code != attempt {
            pending.failures += 1;
            if pending.failures >= MAX_FAILURES {
                self.pending = None;
            }
            return Claim::Invalid;
        }
        let expired = pending.expires_at <= now;
        self.pending = None;
        if expired {
            Claim::Expired
        } else {
            Claim::Linked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap()
    }

    #[test]
    fn codes_are_six_unambiguous_symbols() {
        let mut codes = LinkCodes::default();
        let code = codes.issue(t0()).code;
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
    }

    #[test]
    fn a_code_links_once_case_insensitively() {
        let mut codes = LinkCodes::default();
        let code = codes.issue(t0()).code;
        assert_eq!(
            codes.claim(&code.to_lowercase(), t0() + TimeDelta::minutes(9)),
            Claim::Linked
        );
        assert_eq!(codes.claim(&code, t0()), Claim::Invalid);
        assert!(codes.current(t0()).is_none());
    }

    #[test]
    fn expired_codes_are_refused_and_withdrawn() {
        let mut codes = LinkCodes::default();
        let code = codes.issue(t0()).code;
        let late = t0() + CODE_TTL;
        assert!(codes.current(late).is_none());
        assert_eq!(codes.claim(&code, late), Claim::Expired);
        assert_eq!(codes.claim(&code, t0()), Claim::Invalid);
    }

    #[test]
    fn repeated_wrong_guesses_burn_the_code() {
        let mut codes = LinkCodes::default();
        let code = codes.issue(t0()).code;
        for _ in 0..MAX_FAILURES {
            assert_eq!(codes.claim("WRONG1", t0()), Claim::Invalid);
        }
        assert_eq!(codes.claim(&code, t0()), Claim::Invalid);
    }

    #[test]
    fn a_new_code_replaces_the_old() {
        let mut codes = LinkCodes::default();
        let old = codes.issue(t0()).code;
        let new = codes.issue(t0()).code;
        if old != new {
            assert_eq!(codes.claim(&old, t0()), Claim::Invalid);
        }
        assert_eq!(codes.claim(&new, t0()), Claim::Linked);
    }
}
