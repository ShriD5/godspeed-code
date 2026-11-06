//! Zoomies from the main window: the composer's Zoomie mode (plain-English
//! schedules, local names, draft assembly), "Run this on a schedule…" for an
//! existing chat, and the sidebar section with its speedster rows. The
//! engine side lives in `zeron_engine::agents`; Settings → Zoomies keeps the
//! full editor and inbox.

pub mod draft;
pub mod feed;
pub mod mascot;
pub mod naming;
pub mod rows;
pub mod schedule;

pub use draft::{ComposerZoomie, ProjectRef, ZoomiePrefill, assemble_draft, prefill_from_chat};
pub use feed::ZoomiesFeed;
pub use naming::derive_name;
pub use schedule::{ParsedSchedule, Preset, parse_schedule};

/// This machine's IANA zone (`$TZ`, else `/etc/localtime` →
/// `…/zoneinfo/Area/City`), the zone plain-English schedules run in.
pub fn local_timezone() -> String {
    std::env::var("TZ")
        .ok()
        .filter(|tz| tz.contains('/'))
        .or_else(|| {
            let target = std::fs::read_link("/etc/localtime").ok()?;
            let target = target.to_string_lossy();
            target
                .split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_string())
        })
        .unwrap_or_else(|| "UTC".into())
}
