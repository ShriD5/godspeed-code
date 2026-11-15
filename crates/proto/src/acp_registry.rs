//! Settings → Providers → "More agents": the public ACP agent registry as
//! one device sees it. Everything here describes untrusted registry data
//! after the engine validated it; nothing in it is ever executed directly.

use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// How a registry agent is installed on the listing device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcpDistributionKind {
    /// A prebuilt archive whose SHA-256 the registry pins.
    Binary,
    /// An exact-version npm package, integrity-checked against npm's sha512.
    Npm,
}

/// One agent row of the "More agents" list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpRegistryAgentInfo {
    pub id: String,
    pub name: String,
    /// The registry's current version.
    pub version: String,
    pub description: String,
    /// The registry's `authors`, joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The install route chosen for this device; `None` with `unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<AcpDistributionKind>,
    /// Download size in bytes (binary archive) or unpacked package size (npm),
    /// when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_size: Option<u64>,
    /// Why this device cannot install the agent (no build for the platform,
    /// no pinned checksum, an unsupported runner, npm missing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// The installed version, when installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// The built-in harness this registry entry duplicates (Godspeed drives
    /// those natively); such rows are listed but not offered for install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_in: Option<HarnessId>,
}

impl AcpRegistryAgentInfo {
    pub fn update_available(&self) -> bool {
        self.installed_version
            .as_deref()
            .is_some_and(|installed| installed != self.version && self.unavailable.is_none())
    }
}

/// The "More agents" list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpRegistrySnapshot {
    pub agents: Vec<AcpRegistryAgentInfo>,
    /// When the registry index was fetched (unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    /// The last refresh failed; `agents` comes from the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
