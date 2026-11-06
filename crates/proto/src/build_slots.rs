//! Shared build slots and sccache (Settings → General → Builds): one
//! GNU-make-style jobserver the engine runs for every agent and terminal it
//! starts, so parallel agents share compile slots instead of each starting
//! one rustc per core. The engine owns the setting (`GetBuildSlotsSettings`).

use serde::{Deserialize, Serialize};

/// The most slots the pool can be given.
pub const MAX_BUILD_SLOTS: u32 = 64;

/// The setting plus what the engine found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSlotsSettings {
    /// Share one pool of build slots across agents. On by default.
    pub share_slots: bool,
    /// Slots in the pool; `0` = automatic ([`Self::default_slots`]).
    #[serde(default)]
    pub slots: u32,
    /// Set `RUSTC_WRAPPER` to sccache in Rust projects when it's installed.
    /// On by default; does nothing until sccache is on PATH.
    pub use_sccache: bool,
    /// The automatic slot count: this machine's cores, the same parallelism
    /// one cargo build uses on its own.
    #[serde(default)]
    pub default_slots: u32,
    /// The platform can run the pool (Unix; not Windows yet).
    #[serde(default)]
    pub supported: bool,
    /// The pool is running.
    #[serde(default)]
    pub active: bool,
    /// The variable in the user's environment or login shell that keeps
    /// the pool out (e.g. `CARGO_BUILD_JOBS`): their choice wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_by: Option<String>,
    /// Where sccache was found, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sccache_path: Option<String>,
    /// The variable that already picks a rustc wrapper (`RUSTC_WRAPPER`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sccache_paused_by: Option<String>,
}

impl BuildSlotsSettings {
    /// The slot count in effect.
    pub fn effective_slots(&self) -> u32 {
        if self.slots == 0 {
            self.default_slots.max(1)
        } else {
            self.slots
        }
    }
}

/// A partial update; `None` leaves a field as is. `slots: Some(0)` returns
/// to the automatic count.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSlotsSettingsUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_slots: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_sccache: Option<bool>,
}
