//! zeron-proto — wire types shared by engine, UI, and RPC.
//!
//! Ported from godspeed's `packages/control/src/wire.ts` + `packages/harness/src/types.ts`.
//! Context occupancy is replicated per chat; billing `Usage` remains a harness passthrough.

pub mod access;
pub mod account_usage;
pub mod agent;
pub mod agents;
pub mod approval_risk;
pub mod agent_messages;
pub mod attachment_mentions;
pub mod attention;
pub mod chat_mentions;
pub mod change_request_assessment;
pub mod chat_import;
pub mod conductor;
pub mod device_paths;
pub mod entities;
pub mod annotation;
pub mod file_mentions;
pub mod git_actions;
pub mod invite_only;
pub mod invocation;
pub mod message_search;
pub mod motion;
pub mod preview;
pub mod preview_agent;
pub mod pricing;
pub mod pull_request_actions;
pub mod project_transfer;
pub mod projectless;
pub mod sidebar_pins;
pub mod telegram;
pub mod telemetry;
pub mod usage_history;
pub mod view;
pub mod voice;
pub mod workspace;
pub mod worktree_env;

pub use access::*;
pub use account_usage::*;
pub use agent::*;
pub use approval_risk::{ApprovalRisk, approval_risk};
pub use conductor::{
    Conductor, ConductorMembership, ConductorNode, ConductorRepo, CreateConductorParams,
    MAX_CONDUCTOR_DEPTH, MAX_NOTES_BYTES, UpdateConductorParams, can_spawn_at, clamp_notes,
    main_agent_instructions, sub_agent_instructions,
};
pub use entities::*;
pub use git_actions::*;
pub use message_search::*;
pub use preview::*;
pub use sidebar_pins::*;
pub use telemetry::*;
pub use usage_history::*;
pub use workspace::*;
pub use worktree_env::{
    WakeWorktreeOutcome, WorktreeCopyRule, WorktreeEnvironment, WorktreeEnvironmentSnapshot,
    WorktreeEnvironmentStatus, WorktreePortBlock, WorktreePorts, WorktreeSetupState,
};

/// Parse "0.2.12" (tolerating a `-suffix`/`+build` tail on the last part)
/// into a comparable triple — the fleet feature-gate primitive (device rows
/// stamp `Device::version` at boot). `None` for anything that doesn't lead
/// with three dotted integers, and gates treat `None` as "too old".
pub fn version_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().splitn(3, '.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch = parts.next()?;
    let patch: u64 = patch
        .split(['-', '+'])
        .next()
        .unwrap_or(patch)
        .parse()
        .ok()?;
    Some((major, minor, patch))
}
