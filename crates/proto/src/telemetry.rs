//! Anonymous usage telemetry: the once-a-day ping an install sends to
//! `{edge}/telemetry/v1/ping` (docs/PRIVACY.md), and the Settings → General
//! switch that stops it (`GetTelemetrySettings`).
//!
//! The ping is deliberately small and closed: dates, the app version and
//! platform, and counts. No prompts, code, file paths, repo or project
//! names, emails, account or device ids, and no free text of any kind. The
//! install id is a random UUID minted for telemetry alone, never tied to an
//! account. `ping_fields_are_exactly_the_documented_ones` below pins the
//! field list: adding a field means changing that test, docs/PRIVACY.md and
//! the edge's validator (edge/src/telemetry.ts) together.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Payload version (`v`). Bump when the shape changes.
pub const TELEMETRY_VERSION: u32 = 1;

/// Path of the ingest endpoint under the edge base URL.
pub const TELEMETRY_PATH: &str = "/telemetry/v1/ping";

/// Where the in-app notice's "Details" link and the setting's "What's
/// sent?" link point.
pub const PRIVACY_URL: &str = "https://github.com/ShriD5/godspeed-code/blob/main/docs/PRIVACY.md";

/// Harness ids the ping may name. Anything else is counted as `other`, so a
/// harness id can never carry free text.
pub const TELEMETRY_HARNESSES: &[&str] = &[
    "claude-code",
    "codex",
    "cursor",
    "devin",
    "grok",
    "hermes",
    "pi",
    "muse",
    "opencode",
    "antigravity",
    "mock",
    "unknown",
];

/// The once-a-day ping. Field names are the wire names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryPing {
    /// [`TELEMETRY_VERSION`].
    pub v: u32,
    /// Random UUID v4 stored in the data dir (`telemetry.json`). Not the
    /// device id, never tied to an account.
    pub install_id: String,
    /// The UTC day this ping is sent on (one ping per install per day).
    pub day: NaiveDate,
    pub app_version: String,
    /// `macos`, `linux`, `windows` or `other`.
    pub os: String,
    /// `aarch64`, `x86_64` or `other`.
    pub arch: String,
    /// The UTC day this install first ran with telemetry.
    pub first_seen: NaiveDate,
    /// A person used Godspeed on `day` (the desktop app was open or a turn
    /// was started); `false` for a background engine that only ran.
    pub active: bool,
    /// What was used on the most recent earlier day not yet reported (a
    /// day's counts are only complete once it is over). Absent when there is
    /// nothing to report (the first ever ping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TelemetryUsage>,
}

/// One finished UTC day's counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryUsage {
    /// The UTC day these counts cover.
    pub day: NaiveDate,
    /// Chats created on this install that day.
    pub chats_started: u64,
    /// Agent turns that finished that day.
    pub turns_finished: u64,
    /// Approvals and agent questions answered that day.
    pub approvals_answered: u64,
    /// Zoomie (always-on agent) runs started that day.
    pub zoomie_runs: u64,
    /// Finished turns per harness id ([`TELEMETRY_HARNESSES`] or `other`).
    pub harnesses: BTreeMap<String, u64>,
    pub tokens: TelemetryTokens,
    /// Estimated USD for those tokens at list API prices
    /// ([`crate::pricing`]); subscription logins pay nothing per token.
    pub spend_usd: f64,
}

/// Token totals. `input` excludes cache traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// A harness id as the ping may carry it.
pub fn telemetry_harness(id: &str) -> &'static str {
    TELEMETRY_HARNESSES
        .iter()
        .copied()
        .find(|known| *known == id)
        .unwrap_or("other")
}

/// The platform names the ping uses.
pub fn telemetry_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => "other",
    }
}

pub fn telemetry_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => "other",
    }
}

/// Which environment variable turned telemetry off, if any:
/// `GODSPEED_TELEMETRY=0` (or `false`/`off`/`no`) or `DO_NOT_TRACK=1` (any
/// value but empty/`0`/`false`, per consoledonottrack.com).
pub fn telemetry_env_off(get: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    let falsy = |value: &str| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    };
    if get("GODSPEED_TELEMETRY").is_some_and(|value| falsy(&value)) {
        return Some("GODSPEED_TELEMETRY");
    }
    if get("DO_NOT_TRACK").is_some_and(|value| !value.trim().is_empty() && !falsy(&value)) {
        return Some("DO_NOT_TRACK");
    }
    None
}

/// `GetTelemetrySettings` / `SetTelemetrySettings` reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySettings {
    /// The Settings switch (default on).
    pub enabled: bool,
    /// The environment variable that turns telemetry off regardless of the
    /// switch (`GODSPEED_TELEMETRY` or `DO_NOT_TRACK`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_by_env: Option<String>,
    /// The one-time first-launch notice hasn't been shown yet.
    #[serde(default)]
    pub notice_pending: bool,
}

impl TelemetrySettings {
    /// Pings are actually sent.
    pub fn sending(&self) -> bool {
        self.enabled && self.disabled_by_env.is_none()
    }
}

/// `SetTelemetrySettings` params; absent fields stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySettingsUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Record that the first-launch notice was shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice_shown: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    fn sample() -> TelemetryPing {
        TelemetryPing {
            v: TELEMETRY_VERSION,
            install_id: "4f9d7c1e-2b6a-4c1d-9e3f-0a1b2c3d4e5f".into(),
            day: day(27),
            app_version: "0.5.8".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            first_seen: day(20),
            active: true,
            usage: Some(TelemetryUsage {
                day: day(26),
                chats_started: 3,
                turns_finished: 12,
                approvals_answered: 4,
                zoomie_runs: 1,
                harnesses: BTreeMap::from([("claude-code".into(), 9), ("codex".into(), 3)]),
                tokens: TelemetryTokens {
                    input: 1,
                    output: 2,
                    cache_read: 3,
                    cache_write: 4,
                },
                spend_usd: 1.25,
            }),
        }
    }

    fn keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The privacy contract. If this fails you added (or renamed) a field:
    /// update docs/PRIVACY.md and edge/src/telemetry.ts in the same change,
    /// and never add anything that could carry prompts, code, paths, names,
    /// emails, account/device ids or free text.
    #[test]
    fn ping_fields_are_exactly_the_documented_ones() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            keys(&json),
            [
                "active",
                "app_version",
                "arch",
                "day",
                "first_seen",
                "install_id",
                "os",
                "usage",
                "v"
            ]
        );
        assert_eq!(
            keys(&json["usage"]),
            [
                "approvals_answered",
                "chats_started",
                "day",
                "harnesses",
                "spend_usd",
                "tokens",
                "turns_finished",
                "zoomie_runs"
            ]
        );
        assert_eq!(
            keys(&json["usage"]["tokens"]),
            ["cache_read", "cache_write", "input", "output"]
        );
        assert_eq!(json["day"], "2026-10-27");
        assert_eq!(json["usage"]["harnesses"]["claude-code"], 9);
    }

    #[test]
    fn pings_round_trip_and_reject_unknown_fields() {
        let ping = sample();
        let text = serde_json::to_string(&ping).unwrap();
        assert_eq!(serde_json::from_str::<TelemetryPing>(&text).unwrap(), ping);
        let mut extra = serde_json::to_value(&ping).unwrap();
        extra["email"] = "x@example.com".into();
        assert!(serde_json::from_value::<TelemetryPing>(extra).is_err());
        let first = TelemetryPing {
            usage: None,
            ..sample()
        };
        assert!(
            !serde_json::to_value(first)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("usage")
        );
    }

    #[test]
    fn harness_ids_are_a_closed_set() {
        assert_eq!(telemetry_harness("codex"), "codex");
        assert_eq!(telemetry_harness("my secret project"), "other");
    }

    #[test]
    fn env_switches_turn_it_off() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(telemetry_env_off(env(&[])), None);
        assert_eq!(
            telemetry_env_off(env(&[("GODSPEED_TELEMETRY", "0")])),
            Some("GODSPEED_TELEMETRY")
        );
        assert_eq!(
            telemetry_env_off(env(&[("GODSPEED_TELEMETRY", "off")])),
            Some("GODSPEED_TELEMETRY")
        );
        assert_eq!(telemetry_env_off(env(&[("GODSPEED_TELEMETRY", "1")])), None);
        assert_eq!(
            telemetry_env_off(env(&[("DO_NOT_TRACK", "1")])),
            Some("DO_NOT_TRACK")
        );
        assert_eq!(telemetry_env_off(env(&[("DO_NOT_TRACK", "0")])), None);
        assert_eq!(telemetry_env_off(env(&[("DO_NOT_TRACK", "")])), None);
    }
}
