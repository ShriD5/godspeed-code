//! Codex harness: spawns the installed `codex` CLI as `codex app-server` and
//! speaks JSON-RPC 2.0 over stdio — the same interface the Codex IDE extension
//! uses. Resurrected from the pre-ACP driver and modernized.
//!
//! VERSION PIN: the app-server API is EXPERIMENTAL (`capabilities.
//! experimentalApi`); this driver is validated against codex-cli 0.153.4 —
//! imageGeneration additionally follows the 0.154.0 schema (savedPath only).
//! Revalidate the method/notification surface when bumping past it.
//!
//! - `initialize` handshake (clientInfo + `capabilities.experimentalApi`) then
//!   the `initialized` notification; unknown notification methods tolerated.
//! - `thread/start` (or `thread/resume` with a fresh-start fallback) →
//!   `SessionStarted`; `turn/start` carries the prompt, model, effort,
//!   `sandboxPolicy`, and approval policy.
//! - Notifications map to [`AgentEvent`]s: agentMessage/reasoning deltas (both
//!   `delta`/`textDelta` spellings), item lifecycles → typed ToolCall/ToolResult,
//!   `thread/tokenUsage/updated` → Usage, turn/completed|failed|aborted → Done.
//! - Access modes map onto `approvalPolicy` + sandbox (validated against
//!   `codex app-server generate-json-schema`, 0.155): supervised =
//!   `untrusted` + read-only, auto-accept edits = `untrusted` +
//!   workspace-write with in-workspace file changes accepted here (codex's
//!   `untrusted` asks before every patch), auto = `on-request` +
//!   workspace-write (codex's own "Auto" preset), full access = `never` +
//!   danger-full-access. Both ride every `turn/start`, so a mode change
//!   applies from the next turn in place; extra writable roots ride
//!   `sandboxPolicy.writableRoots`. `item/commandExecution/requestApproval`,
//!   `item/fileChange/requestApproval` (its diff taken from the fileChange
//!   item) and `item/permissions/requestApproval` become approvals answered
//!   accept / acceptForSession / decline / cancel.
//! - Subagents are full child app-server threads. Parent spawn items establish
//!   their stable ownership; content arriving before the spawn is buffered.
//!   A registered child's notifications route through an EXPLICIT table
//!   ([`normalize::route_child_notification`]) — content, errors and child turns
//!   become tagged [`AgentEvent::Subagent`] events; unrelated child bookkeeping is
//!   consumed so it can never settle the parent turn, and unknown methods
//!   fall through to the parent path (fail open, never silent loss).
//! - Steering: `turn/steer { expectedTurnId, clientUserMessageId }` into the
//!   live turn, confirmed when its userMessage item (carrying that id) joins
//!   the turn; a rejected steer (the turn-completed race) is queued and
//!   delivered as the next `turn/start` on the same thread. The session is
//!   persistent across turns while the steering mailbox lives; a steer's
//!   changed model/effort/tier rides the next `turn/start`.
//! - Only the turn in flight settles: a stale or duplicate completion is
//!   ignored. Commands still running past their turn report as background
//!   work, so the host never reaps the app-server from under them.
//! - Turn stop ([`crate::TurnControl::stop_turn`]): `turn/interrupt` ends the
//!   turn `Interrupted`; the app-server and its threads (child agents
//!   included) stay up for the next `turn/start`.
//! - Interrupt: cancelling [`RunControls::interrupt`] tears the runtime down —
//!   `turn/interrupt`, escalating to SIGTERM → SIGKILL if the child is
//!   unresponsive; the stream always ends with `Done { status: Interrupted }`.

pub(crate) mod catalog;
mod normalize;
pub mod realtime;
mod subagents;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use zeron_proto::{
    AccessMode, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalRequest, DoneStatus,
    HarnessId, Model, ModelOption, ModelOptionChoice, ReasoningLevel, RunRequest, SandboxLevel,
    SlashCommand, SteeringMode, UserInputAnswer, UserInputQuestion,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::process::{Child, Command, Stdio};
use crate::{Harness, HarnessError, RunControls};
use catalog::{REASONING_LEVELS, sandbox_mode, sandbox_policy_value, static_models, to_effort};
use normalize::{
    ChildRoute, Phase, ReasoningStream, delta_text, item_id, item_type, notification_thread_id,
    route_child_notification, turn_error_message, turn_id, usage_event,
};

/// Locate the device's installed Codex CLI: `CODEX_EXECUTABLE`, then our own
/// PATH, then the login-shell PATH snapshot (the user's shell init shapes
/// PATH in ways a GUI/service launch never sees — see [`crate::shell_env`]),
/// then known install locations as a last resort. Resolved per call — cheap
/// after the snapshot is cached.
pub fn resolve_codex_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CODEX_EXECUTABLE").filter(|p| !p.is_empty()) {
        return crate::executable::validate_native_override(&PathBuf::from(p)).ok();
    }
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".local").join("bin").join("codex"));
        extra.push(home.join(".codex").join("bin").join("codex"));
        extra.push(home.join(".npm-global").join("bin").join("codex"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/codex"));
    extra.push(PathBuf::from("/usr/local/bin/codex"));
    crate::executable::find_on_paths("codex", extra)
}

/// `canonicalize` yields `\\?\`-prefixed verbatim paths on Windows, which
/// cmd.exe cannot launch batch shims through; keep the plain drive/UNC form.
fn plain_executable(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rest) = path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
            return match rest.strip_prefix(r"UNC\") {
                Some(share) => PathBuf::from(format!(r"\\{share}")),
                None => PathBuf::from(rest),
            };
        }
    }
    path
}

/// Dotted `thread/start` config overrides that add an injected MCP server
/// to the user's `mcp_servers` table.
fn codex_mcp_overrides(mcp: &zeron_proto::McpServer) -> Vec<(String, Value)> {
    let key = |field: &str| format!("mcp_servers.{}.{field}", mcp.name);
    vec![
        (key("command"), mcp.command.clone().into()),
        (key("args"), json!(mcp.args)),
        (key("env"), json!(mcp.env)),
    ]
}

/// A ready-to-spawn `codex login` command for the engine's account flow.
///
/// Shares the harness's full resolution (`CODEX_EXECUTABLE`, PATH, login-shell
/// snapshot, install locations — including the Windows npm payload layout) and
/// its child-PATH composition, so "Add account" launches exactly the binary
/// the harness itself would run. `CODEX_HOME` isolates the login from the live
/// `~/.codex` session; the caller owns stdio wiring and cancellation.
pub fn login_command(codex_home: &std::path::Path) -> Result<Command, HarnessError> {
    let exe = CodexHarness::new().resolve_executable()?;
    let mut cmd = Command::new(&exe);
    crate::compose_child_path(&mut cmd, &exe);
    cmd.arg("login").env("CODEX_HOME", codex_home);
    Ok(cmd)
}

/// The Codex harness. Construct with [`CodexHarness::new`]; tests point it at a
/// fake app server with [`CodexHarness::with_executable`].
pub struct CodexHarness {
    models_cache: crate::catalog::Catalog,
    executable: Option<PathBuf>,
    /// Grace between `turn/interrupt` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
}

impl Default for CodexHarness {
    fn default() -> Self {
        Self {
            models_cache: crate::catalog::Catalog::default(),
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl CodexHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return crate::executable::validate_native_override(p);
        }
        if let Some(p) = std::env::var_os("CODEX_EXECUTABLE")
            && !p.is_empty()
        {
            return crate::executable::validate_native_override(&PathBuf::from(p));
        }
        resolve_codex_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "codex (searched PATH, the login shell's PATH, ~/.local/bin, \
                 ~/.codex/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, \
                 and fnm/nvm/volta/pnpm/bun install dirs; Windows also checks USERPROFILE \
                 and explicit NVM_SYMLINK/VOLTA_HOME/PNPM_HOME; set CODEX_EXECUTABLE \
                 to override)"
                    .into(),
            )
        })
    }

    /// Short-lived discovery probe: a `codex app-server` handshake followed by
    /// `skills/list` — the only invocable-listing method the 0.146.x wire has
    /// (custom `~/.codex/prompts` are NOT exposed; the TUI-only built-ins
    /// aren't either). Preserve each skill's path and project context;
    /// skills are separate from the slash-command catalog.
    async fn discover_skills(&self, cwd: Option<&std::path::Path>) -> Result<Value, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        cmd.arg("app-server");
        cmd.current_dir(cwd.map_or_else(crate::executable::neutral_cwd, |c| c.to_path_buf()));
        crate::compose_child_path(&mut cmd, &exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(crate::executable::binary_hint(&exe))
            } else {
                HarnessError::Io(e)
            }
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("codex child has no stdio".into()));
        };
        // The receiver must stay alive for the client's reader loop; agent →
        // client traffic during the probe is ignored.
        let (client, _incoming) = RpcClient::new(stdin, stdout);
        let discovery = async {
            client
                .request(
                    "initialize",
                    json!({
                        "clientInfo": {
                            "name": "godspeed-native",
                            "title": "Godspeed",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "capabilities": { "experimentalApi": true },
                    }),
                )
                .await?;
            client.notify("initialized", None);
            let params = cwd
                .map(|cwd| json!({ "cwds": [cwd], "forceReload": true }))
                .unwrap_or_else(|| json!({}));
            let skills = client.request("skills/list", params).await?;
            Ok::<Value, HarnessError>(skills)
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("command discovery timed out".into())),
        }
    }

    /// Short-lived live catalog probe. `model/list` is paginated and already
    /// applies the signed-in account's rollout/visibility policy, so hidden or
    /// unavailable models (including staged Astra rollouts) never leak into a
    /// successful picker response.
    async fn discover_models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        cmd.arg("app-server").current_dir(crate::executable::neutral_cwd());
        crate::compose_child_path(&mut cmd, &exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(crate::executable::binary_hint(&exe))
            } else {
                HarnessError::Io(e)
            }
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("codex child has no stdio".into()));
        };
        let (client, _incoming) = RpcClient::new(stdin, stdout);
        let discovery = async {
            client
                .request(
                    "initialize",
                    json!({
                        "clientInfo": {
                            "name": "godspeed-native",
                            "title": "Godspeed",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "capabilities": { "experimentalApi": true },
                    }),
                )
                .await?;
            client.notify("initialized", None);

            let mut models = Vec::new();
            let mut model_ids = HashSet::new();
            let mut seen_cursors = HashSet::new();
            let mut cursor: Option<String> = None;
            let mut default_model_id: Option<String> = None;
            loop {
                let mut params = json!({ "limit": 20, "includeHidden": false });
                if let Some(cursor) = cursor.as_deref() {
                    params["cursor"] = Value::String(cursor.to_owned());
                }
                let page = client.request("model/list", params).await?;
                if legacy_model_page(&page) {
                    tracing::warn!(binary_path = %exe.display(), binary_version = ?crate::executable::binary_version(&exe), "Model discovery response lacks hidden flags; CLI may be outdated");
                }
                let (page_models, next_cursor) = parse_model_list_page(&page);
                for (model, is_default) in page_models {
                    if model_ids.insert(model.id.clone()) {
                        if is_default && default_model_id.is_none() {
                            default_model_id = Some(model.id.clone());
                        }
                        models.push(model);
                    }
                }
                let Some(next) = next_cursor.filter(|next| !next.is_empty()) else {
                    break;
                };
                if !seen_cursors.insert(next.clone()) {
                    break;
                }
                cursor = Some(next);
            }

            if let Some(default_id) = default_model_id
                && let Some(index) = models.iter().position(|model| model.id == default_id)
                && index != 0
            {
                let default_model = models.remove(index);
                models.insert(0, default_model);
            }
            if models.is_empty() {
                return Err(crate::CatalogFailure {
                    code: crate::CatalogFailureCode::Failed,
                    message: "Codex returned an empty model catalog".into(),
                }
                .into());
            }
            Ok::<Vec<Model>, HarnessError>(models)
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("model discovery timed out".into())),
        }
    }
}

/// The `serviceTier` a request runs with; "default" (Standard) is omitted.
fn service_tier(request: &RunRequest) -> Option<String> {
    request
        .model_options
        .get("serviceTier")
        .and_then(Value::as_str)
        .filter(|t| *t != "default")
        .map(str::to_owned)
}

fn reasoning_level(value: &str) -> Option<ReasoningLevel> {
    Some(match value {
        "minimal" => ReasoningLevel::Minimal,
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::XHigh,
        "max" => ReasoningLevel::Max,
        "ultra" => ReasoningLevel::Ultra,
        "ultracode" => ReasoningLevel::Ultracode,
        "ultrathink" => ReasoningLevel::Ultrathink,
        _ => return None,
    })
}

/// Codex accepts both names, but Godspeed has historically persisted `fast`.
/// Normalize the app server's `priority` id so live and fallback catalogs do
/// not produce two different settings for the same tier.
fn normalized_service_tier(value: &str) -> &str {
    match value {
        "priority" => "fast",
        other => other,
    }
}

fn service_tier_label(value: &str) -> String {
    match value {
        "fast" | "priority" => "Fast".into(),
        "flex" => "Flex".into(),
        "ultrafast" => "Ultra Fast".into(),
        other => other.to_owned(),
    }
}

fn model_service_tier(item: &Value) -> Option<ModelOption> {
    let mut choices = vec![ModelOptionChoice {
        id: "default".into(),
        label: "Standard".into(),
    }];
    let mut seen = HashSet::from(["default".to_owned()]);
    for tier in item
        .get("serviceTiers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(wire_id) = tier.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = normalized_service_tier(wire_id).to_owned();
        if !seen.insert(id.clone()) {
            continue;
        }
        let label = tier
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| service_tier_label(wire_id));
        choices.push(ModelOptionChoice { id, label });
    }
    for tier in item
        .get("additionalSpeedTiers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(wire_id) = tier.as_str() else {
            continue;
        };
        let id = normalized_service_tier(wire_id).to_owned();
        if seen.insert(id.clone()) {
            choices.push(ModelOptionChoice {
                id,
                label: service_tier_label(wire_id),
            });
        }
    }
    if choices.len() == 1 {
        return None;
    }
    let default_choice = item
        .get("defaultServiceTier")
        .and_then(Value::as_str)
        .map(normalized_service_tier)
        .filter(|id| seen.contains(*id))
        .unwrap_or("default")
        .to_owned();
    Some(ModelOption {
        id: "serviceTier".into(),
        label: "Service Tier".into(),
        choices,
        default_choice,
    })
}

fn legacy_model_page(page: &Value) -> bool {
    page.get("data")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            !items.is_empty() && items.iter().all(|item| item.get("hidden").is_none())
        })
}

/// Parse one `model/list` page. Unknown future reasoning levels are ignored
/// independently instead of invalidating the complete catalog.
fn parse_model_list_page(result: &Value) -> (Vec<(Model, bool)>, Option<String>) {
    let mut models = Vec::new();
    for item in result
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if item.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(id) = item
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| item.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let label = item
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(id)
            .to_owned();
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(str::to_owned);
        let description = match item
            .get("upgrade")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            Some(upgrade) => Some(format!(
                "{}(upgrade: {upgrade})",
                description.map(|d| format!("{d} ")).unwrap_or_default()
            )),
            None => description,
        };
        let reasoning_levels = item
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|effort| {
                effort
                    .get("reasoningEffort")
                    .and_then(Value::as_str)
                    .or_else(|| effort.as_str())
                    .and_then(reasoning_level)
            })
            .collect();
        let options = model_service_tier(item).into_iter().collect();
        models.push((
            Model {
                id: id.to_owned(),
                label,
                description,
                reasoning_levels,
                options,
            },
            item.get("isDefault").and_then(Value::as_bool) == Some(true),
        ));
    }
    let next_cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (models, next_cursor)
}

/// `skills/list` result → typed skills. Keep distinct paths for duplicate names.
/// Identical name/path pairs are deduplicated across cwd groups. The interface's
/// shortDescription is picker-sized; the model-facing description is a fallback.
fn parse_skills(result: &Value) -> Vec<zeron_proto::invocation::Skill> {
    let mut seen = HashSet::new();
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group
                .get("skills")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|skill| {
            let name = skill.get("name")?.as_str()?;
            let path = skill.get("path")?.as_str()?;
            if !zeron_proto::invocation::valid_invocation_name(name)
                || !zeron_proto::invocation::valid_skill_path(path)
                || !seen.insert((name.to_owned(), path.to_owned()))
            {
                return None;
            }
            Some(zeron_proto::invocation::Skill {
                command: None,
                name: name.to_owned(),
                path: path.to_owned(),
                description: skill
                    .pointer("/interface/shortDescription")
                    .and_then(Value::as_str)
                    .or_else(|| skill.get("description").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned(),
                enabled: skill
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            })
        })
        .collect()
}

#[async_trait]
impl Harness for CodexHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }
    fn display_name(&self) -> &str {
        // "Codex" (not "Codex CLI") — comet composer/defaults.ts
        // HARNESS_LABEL; must also match the registry's lazy descriptor so
        // the catalog entry doesn't change after the first resolve.
        "Codex"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// Native `turn/steer` injects into the active turn; a steer that misses
    /// the turn falls back to a follow-up `turn/start` on the same thread.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_executable().ok()
    }
    /// Done is the CLI's own terminal frame, for wake turns too.
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    /// `turn/interrupt` ends the turn; the app-server and its thread — child
    /// agent threads included — stay up for the next `turn/start`.
    fn stops_turn_in_place(&self) -> bool {
        true
    }
    /// What the app-server launch and its turns read: `serviceTier` is the
    /// only model option used, and the access mode rides every turn.
    fn same_runtime(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.cwd == next.cwd
            && live.model == next.model
            && to_effort(live.reasoning) == to_effort(next.reasoning)
            && service_tier(live) == service_tier(next)
            && live.access_mode() == next.access_mode()
            && live.writable_roots() == next.writable_roots()
    }
    /// `turn/start` carries model, effort and service tier, and each sticks
    /// for later turns — so a change applies from the next turn without a
    /// new app-server. Clearing one back to the default cannot be expressed
    /// that way (an omitted parameter keeps the previous value).
    fn reconfigures_in_place(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.cwd == next.cwd
            && (next.model.is_some() || live.model.is_none())
            && (to_effort(next.reasoning).is_some() || to_effort(live.reasoning).is_none())
            && (service_tier(next).is_some() || service_tier(live).is_none())
    }

    /// The signed-in account's visible `model/list` is authoritative. A
    /// curated snapshot keeps the picker operational when the experimental
    /// discovery call is unavailable and no last-good catalog exists. Explicit
    /// picker refreshes bypass cooldowns while overlapping callers coalesce.
    fn model_context(&self) -> Result<Option<crate::ModelContext>, HarnessError> {
        crate::model_context::context(self.id(), &self.resolve_executable()?, &[]).map(Some)
    }
    fn fallback_models(&self) -> Vec<Model> {
        static_models()
    }
    async fn model_catalog(&self, force: bool) -> Result<crate::ModelCatalog, HarnessError> {
        self.model_context()?.unwrap().log();
        self.models_cache
            .get_with(
                force,
                || self.model_context().map(|c| c.unwrap().key()),
                || self.discover_models(),
            )
            .await
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        match self.model_catalog(false).await {
            Ok(catalog) => Ok(catalog.models),
            Err(error) if !crate::CatalogFailure::classify(&error).allows_stale() => Err(error),
            Err(error) => {
                tracing::warn!(%error, source = "static", "Model discovery failed");
                Ok(self.fallback_models())
            }
        }
    }

    async fn skills(
        &self,
        cwd: &std::path::Path,
    ) -> Result<Option<Vec<zeron_proto::invocation::Skill>>, HarnessError> {
        self.discover_skills(Some(cwd))
            .await
            .map(|value| Some(parse_skills(&value)))
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        Ok(vec![
            SlashCommand {
                name: "compact".into(),
                description: "Compact this conversation's context".into(),
                input_hint: None,
            },
            SlashCommand {
                name: "review".into(),
                description: "Review uncommitted changes, or supply review instructions".into(),
                input_hint: Some("optional instructions".into()),
            },
        ])
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.run_with_mode(request, controls, false, false).await
    }

    async fn start_idle(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.run_with_mode(request, controls, false, true).await
    }

    async fn run_title(
        &self,
        mut request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        request.resume = None;
        request.worktree = None;
        request.attachments.clear();
        request.mcp = None;
        request.model_options.clear();
        request.auto_approve = false;
        self.run_with_mode(request, controls, true, false).await
    }
}

impl CodexHarness {
    async fn run_with_mode(
        &self,
        mut request: RunRequest,
        controls: RunControls,
        title_only: bool,
        idle: bool,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let native = command_request(&request.prompt, "")?;
        if native
            .as_ref()
            .is_some_and(|(method, _)| *method == "thread/compact/start")
            && request.resume.is_none()
        {
            return Err(HarnessError::Protocol(
                "/compact needs an existing Codex conversation".into(),
            ));
        }
        if native.is_some() && !request.attachments.is_empty() {
            return Err(HarnessError::Protocol(
                "Codex commands cannot include attachments; send them in a separate prompt".into(),
            ));
        }
        // Pin the physical release for this process and its voice helper: an
        // installer may move the current symlink while this runtime stays warm.
        let exe = plain_executable(
            self.resolve_executable()?
                .canonicalize()
                .map_err(HarnessError::Io)?,
        );
        // The sandbox follows the access mode (see `access_policy`); a title
        // run is always read-only. Codex ≤0.144.x mis-mounted a linked
        // worktree on a slash-named branch under workspace-write — fixed in
        // the releases this driver is validated against.
        if title_only {
            request.sandbox = zeron_proto::SandboxLevel::ReadOnly;
        }
        let mut cmd = Command::new(&exe);
        cmd.arg("app-server");
        crate::compose_child_path(&mut cmd, &exe);
        crate::apply_run_env(&mut cmd, &request.env);
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(crate::executable::binary_hint(&exe))
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "zeron_harness::codex", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            title_only,
            idle,
            executable: exe,
            registration: crate::process::registry::register(child.id(), "codex"),
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    executable: PathBuf,
    title_only: bool,
    idle: bool,
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
    /// The agent's crash record, held until the child is reaped.
    registration: Option<crate::process::registry::Registration>,
}

/// Turn-routing state (port of codex.ts's activeTurnId/completedTurnIds): the
/// `turn/start` response and the turn lifecycle notifications are separate
/// app-server messages that may arrive in either order — never revive a turn
/// that `turn/completed` already declared finished.
#[derive(Default)]
struct TurnRouter {
    active: Option<String>,
    /// The app-server announced `active` (`turn/started`). Until then it may
    /// not have registered the turn: `turn/interrupt` answers "no active
    /// turn to interrupt" (live, 0.159.3, an interrupt right after the
    /// `turn/start` response) and the turn would run to its end.
    announced: bool,
    completed: VecDeque<String>,
}

impl TurnRouter {
    fn is_completed(&self, id: &str) -> bool {
        self.completed.iter().any(|c| c == id)
    }

    fn note_started(&mut self, id: String) {
        if id.is_empty() || self.is_completed(&id) {
            return;
        }
        // A replacement `turn/started` is authoritative evidence that a stale
        // active turn is over, even if its completion notification was lost.
        if let Some(prev) = self.active.take()
            && prev != id
        {
            self.remember_completed(prev);
        }
        self.active = Some(id);
        self.announced = true;
    }

    fn note_completed(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        self.remember_completed(id.to_owned());
        if self.active.as_deref() == Some(id) {
            self.active = None;
            self.announced = false;
        }
    }

    /// Adopt a turn id from a `turn/start` RESPONSE (the notification is
    /// allowed to beat it). A turn already running keeps its id: a follow-up
    /// `turn/start` folds into it and answers with that same id anyway, and
    /// an announced turn is authoritative.
    fn adopt_started(&mut self, id: String) {
        if self.active.is_some() || id.is_empty() || self.is_completed(&id) {
            return;
        }
        self.active = Some(id);
        self.announced = false;
    }

    /// Whether a `turn/completed` (or failed/aborted) for `id` ends the turn
    /// in flight. A completion for a turn already settled, or for another
    /// turn while ours runs, is stale: settling on it marked the running
    /// turn done mid-way.
    fn settles(&self, id: &str) -> bool {
        id.is_empty() || (!self.is_completed(id) && self.active.as_deref().is_none_or(|a| a == id))
    }

    fn remember_completed(&mut self, id: String) {
        self.completed.push_back(id);
        // Bounded so a months-long persistent session can't grow it forever.
        while self.completed.len() > 32 {
            self.completed.pop_front();
        }
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// Preserve the selected path in the app-server's native skill input. Text
/// stays first for command routing; repeated selections do not load a skill twice.
fn prompt_input(text: &str) -> Value {
    use zeron_proto::invocation::{Invocation, invocation_links, invocation_prompt};
    let mut input = vec![json!({"type": "text", "text": invocation_prompt(text)})];
    let mut seen = std::collections::HashSet::new();
    for (_, invocation) in invocation_links(text) {
        if let Invocation::Skill { name, path, .. } = invocation {
            if !zeron_proto::invocation::native_skill_identity(&path)
                && seen.insert((name.clone(), path.clone()))
            {
                input.push(json!({"type": "skill", "name": name, "path": path}));
            }
        }
    }
    Value::Array(input)
}

/// Map supported leading commands to native app-server operations.
fn command_request(
    text: &str,
    thread_id: &str,
) -> Result<Option<(&'static str, Value)>, HarnessError> {
    let decoded = zeron_proto::invocation::invocation_prompt(text);
    let Some((name, args)) = zeron_proto::invocation::leading_command(&decoded) else {
        return Ok(None);
    };
    if matches!(name, "compact" | "review")
        && zeron_proto::invocation::invocation_links(text)
            .iter()
            .any(|(_, invocation)| {
                matches!(
                    invocation,
                    zeron_proto::invocation::Invocation::Skill { .. }
                )
            })
    {
        return Err(HarnessError::Protocol(
            "Codex commands cannot include skill selections; send them in a separate prompt".into(),
        ));
    }
    match name {
        "compact" if args.is_empty() => Ok(Some((
            "thread/compact/start",
            json!({"threadId": thread_id}),
        ))),
        "compact" => Err(HarnessError::Protocol(
            "/compact takes no arguments; send other text separately".into(),
        )),
        "review" => Ok(Some((
            "review/start",
            json!({
                "threadId": thread_id, "delivery": "inline",
                "target": if args.is_empty() { json!({"type":"uncommittedChanges"}) }
                    else { json!({"type":"custom", "instructions":args}) },
            }),
        ))),
        // Known client commands need explicit UI mappings. Do not silently
        // send those to the model; unknown slash tokens and paths stay literal.
        "model" | "permissions" | "approvals" | "new" | "clear" | "resume" | "fork" | "status"
        | "diff" | "mention" | "mcp" | "skills" | "plan" | "fast" | "logout" | "quit" | "exit"
        | "init" | "rename" | "feedback" | "ps" | "stop" | "clean" | "archive" | "delete" => {
            Err(HarnessError::Protocol(format!(
                "/{name} is not mapped in Godspeed's Codex integration. Available commands: /compact and /review."
            )))
        }
        _ => Ok(None),
    }
}

async fn start_turn(client: &RpcClient, params: Value) -> Result<String, HarnessError> {
    let text = params
        .pointer("/input/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let thread_id = params["threadId"].as_str().unwrap_or_default();
    let native = command_request(text, thread_id)?;
    if native.is_some()
        && params["input"]
            .as_array()
            .is_some_and(|input| input.len() > 1)
    {
        return Err(HarnessError::Protocol(
            "Codex commands cannot include skill selections; send them in a separate prompt".into(),
        ));
    }
    let (method, params) = native.unwrap_or(("turn/start", params));
    let started = client.request(method, params).await?;
    Ok(started["turn"]["id"].as_str().unwrap_or("").to_owned())
}

/// The per-run event loop: one task multiplexing app-server messages, the
/// steering mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        executable,
        title_only,
        idle,
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        stderr_tail,
        registration: _registration,
    } = session;
    let RunControls {
        realtime,
        execution_lease: _execution_lease,
        request_input,
        mut steering,
        interrupt,
        turn,
    } = controls;
    let request_input = Arc::new(request_input);

    // ---- wire params ------------------------------------------------------
    // The access mode picks the approval policy and sandbox; both ride the
    // thread and every turn (`turn/start` applies them to "this turn and
    // subsequent turns"), so a mode change lands on the next turn in place.
    let start_tier = service_tier(&request);
    let (approval_policy, start_sandbox) = access_policy(&request, title_only);

    let start_params = {
        let mut p = serde_json::Map::new();
        if title_only {
            p.insert("baseInstructions".into(), crate::TITLE_INSTRUCTIONS.into());
            p.insert(
                "developerInstructions".into(),
                crate::TITLE_INSTRUCTIONS.into(),
            );
            p.insert("ephemeral".into(), true.into());
            p.insert(
                "config".into(),
                json!({
                    "project_doc_max_bytes": 0,
                    "web_search": "disabled",
                    "features.shell_tool": false,
                    "features.apply_patch_freeform": false,
                    "features.multi_agent": false,
                    "features.apps": false,
                    "features.multi_agent_v2": false,
                    "agents.enabled": false,
                    "features.browser_use": false,
                    "features.computer_use": false,
                    "features.js_repl": false,
                    "features.image_generation": false,
                    "features.memories": false
                }),
            );
        }
        p.insert("cwd".into(), Value::String(request.cwd.clone()));
        if let Some(mcp) = request.mcp.as_ref().filter(|_| !title_only) {
            // Godspeed's own MCP server as dotted config overrides on top of the
            // user's `mcp_servers` table (the same layer the title run uses
            // to switch servers off).
            let overrides = p
                .entry("config")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("thread/start config overrides are an object");
            overrides.extend(codex_mcp_overrides(mcp));
        }
        p.insert("approvalPolicy".into(), approval_policy.into());
        p.insert("sandbox".into(), sandbox_mode(start_sandbox).into());
        if let Some(model) = &request.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(tier) = &start_tier {
            p.insert("serviceTier".into(), Value::String(tier.clone()));
        }
        p
    };

    // ---- handshake + thread + first turn (interruptible) ------------------
    let setup = async {
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "godspeed-native",
                        "title": "Godspeed",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await?;
        client.notify("initialized", None);

        if realtime.is_some() {
            // Codex's first account/read announces the initial auth snapshot
            // via account/updated. Load it before attaching the voice router,
            // which must keep aborting media on subsequent identity updates.
            // A failed warmup must not block text; voice probes still verify auth.
            let _ = client
                .request("account/read", json!({"refreshToken": false}))
                .await;
        }

        let mut start_params = start_params.clone();
        // The user's effective config: per-turn overrides must merge with
        // it, not replace it (see [`merge_user_turn_config`]).
        let user_config = client
            .request("config/read", json!({"includeLayers": false}))
            .await;
        if title_only {
            // Disable each configured MCP server explicitly: an empty table
            // would merge with user configuration and leave servers enabled.
            let config = user_config.as_ref().map_err(|e| HarnessError::Protocol(e.to_string()))?;
            if let Some(servers) = config["config"]["mcp_servers"].as_object() {
                let overrides = start_params
                    .get_mut("config")
                    .and_then(Value::as_object_mut)
                    .unwrap();
                for name in servers.keys() {
                    overrides.insert(format!("mcp_servers.{name}.enabled"), false.into());
                }
            }
        }
        let thread = if let Some(resume) = &request.resume {
            let mut p = start_params.clone();
            p.insert("threadId".into(), Value::String(resume.clone()));
            match client.request("thread/resume", Value::Object(p)).await {
                Ok(thread) => thread,
                // A missing/foreign rollout falls back to a fresh thread.
                Err(e) => {
                    if command_request(&request.prompt, resume)?.is_some() {
                        return Err(e);
                    }
                    tracing::debug!(
                        target: "zeron_harness::codex",
                        "thread/resume failed (starting fresh): {e}"
                    );
                    client
                        .request("thread/start", Value::Object(start_params.clone()))
                        .await?
                }
            }
        } else {
            client
                .request("thread/start", Value::Object(start_params.clone()))
                .await?
        };
        let thread_id = thread["thread"]["id"].as_str().unwrap_or("").to_owned();
        let mut children = subagents::Subagents::new(thread_id.clone());
        children.restore(&thread["thread"]);
        let voice_context = realtime::ThreadContext {
            cwd: thread["cwd"].as_str().unwrap_or(&request.cwd).to_owned(),
            model_provider: thread["modelProvider"].as_str().map(str::to_owned),
        };
        let user_config = user_config.map(|mut c| c["config"].take()).unwrap_or(Value::Null);
        Ok::<_, HarnessError>((thread_id, children, voice_context, user_config))
    };
    let (thread_id, mut children, voice_context, user_config) = tokio::select! {
        res = setup => match res {
            Ok(thread_id) => thread_id,
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    // Model, effort and service tier ride every `turn/start` (each sticks
    // for later turns): a steer's in-place `config` takes effect from the
    // next turn the runtime starts.
    let mut live = request.clone();
    let turn_params = |live: &RunRequest, text: &str| -> Value {
        let mut p = serde_json::Map::new();
        p.insert("threadId".into(), Value::String(thread_id.clone()));
        p.insert("input".into(), prompt_input(text));
        let (approval_policy, sandbox) = access_policy(live, title_only);
        p.insert("approvalPolicy".into(), approval_policy.into());
        p.insert(
            "sandboxPolicy".into(),
            sandbox_policy_value(sandbox, live.writable_roots()),
        );
        // Reasoning summaries stream (`item/reasoning/summaryTextDelta`) only
        // when asked for — without this codex "thinks" in silence for minutes:
        // nothing renders and the UI's 45s staleness gate flips Working off
        // (user report: "not streaming, doesn't say it's working").
        p.insert("summary".into(), "auto".into());
        merge_user_turn_config(&mut p, &user_config);
        if let Some(model) = &live.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(effort) = to_effort(live.reasoning) {
            p.insert("effort".into(), effort.into());
        }
        if let Some(tier) = service_tier(live) {
            p.insert("serviceTier".into(), Value::String(tier));
        }
        Value::Object(p)
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Codex,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: thread_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    let mut router = TurnRouter::default();
    let _voice_bridge = realtime.map(|controls| {
        realtime::attach(
            client.clone(),
            thread_id.clone(),
            executable,
            voice_context,
            controls,
        )
    });
    if !idle {
        match start_turn(&client, turn_params(&live, &request.prompt)).await {
            Ok(id) => router.adopt_started(id),
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: Some(thread_id.clone()),
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        }
    }

    // ---- main loop --------------------------------------------------------
    // Deltas seen per agent-message item, so a model that never streams
    // (item/completed only) still emits its text exactly once.
    let mut streamed_text: HashSet<String> = HashSet::new();
    let mut reasoning_streams: HashMap<String, ReasoningStream> = HashMap::new();
    // Token usage is held until the turn ends, emitted just before Done.
    let mut pending_usage: Option<AgentEvent> = None;
    // The thread's running token total just before the current turn's first
    // response — what `usage_event` subtracts. Cleared at turn boundaries.
    let mut turn_usage_base: Option<normalize::TokenCounts> = None;
    // Latest `account/rateLimits/updated` snapshot: where a usage-limited
    // turn's reset comes from.
    let mut rate_limits: Option<Value> = None;
    // A final (not retried) usage-limit `error` notification this turn.
    let mut limit_error: Option<String> = None;
    // Steers whose `turn/steer` lost the turn-completed race; delivered as the
    // next `turn/start` when the expected turn's end notification arrives.
    // Each carries the configuration it was sent with, adopted when its own
    // `turn/start` goes out.
    let mut queued_steers: VecDeque<(String, Option<Box<RunRequest>>)> = VecDeque::new();
    // Steers the app-server accepted into the running turn, by the
    // `clientUserMessageId` they were sent with. Each is confirmed
    // (`Steered`) when its userMessage item joins the turn — not when
    // `turn/steer` returns, which split the reply still streaming ahead of
    // it into the steer's segment.
    let mut accepted_steers: VecDeque<String> = VecDeque::new();
    // A turn stop waiting for the app-server to announce the turn.
    let mut stop_when_announced = false;
    // Commands the parent thread started and the app-server has not reported
    // finished, with their process id and the turn that started them. One
    // still open after its turn is background work (a dev server, a
    // watcher): its `item/completed` arrives when the process exits, turns
    // later (live, 0.159.3). The host's idle reaper must not retire the
    // app-server — and the process with it — while it runs.
    let mut open_commands: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    // An interrupted turn's foreground command is killed WITHOUT an
    // `item/completed`: once its process has had a moment to go, drop the
    // turn's commands whose process is gone (background ones live on).
    let mut prune_at: Option<(tokio::time::Instant, String)> = None;
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // A turn stop (`TurnControl::stop_turn`) in flight: the turn's end reads
    // as Interrupted and the app-server stays up for the next turn.
    let mut stopping = false;
    // A Done has been emitted for the turn currently/last in flight.
    let mut done_current = idle;
    let mut current_native = command_request(&request.prompt, &thread_id)
        .ok()
        .flatten()
        .is_some();
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // The runtime's process tree when its teardown began (see
    // [`crate::shutdown_agent`]).
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut torn_down: Vec<i32> = Vec::new();

    // In-flight fileChange items by id: their `changes` (path + diff) are
    // what a `item/fileChange/requestApproval` — which carries none — asks
    // about.
    let mut file_changes: HashMap<String, Value> = HashMap::new();

    'main: loop {
        tokio::select! {
            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                note_file_changes(&mut file_changes, &method, &params);
                // Foreign-thread traffic FIRST: a child thread's turn/thread
                // bookkeeping must never reach the parent turn router below
                // (a child's turn/completed would settle the PARENT turn).
                if let Some(nthread) = notification_thread_id(&method, &params)
                    && !nthread.is_empty()
                    && nthread != thread_id
                {
                    match route_child_notification(&method) {
                        ChildRoute::Parent => {
                            // Unknown/parent-owned: fall through so a codex
                            // update degrades to "the parent sees it",
                            // never silent loss.
                        }
                        ChildRoute::Consumed => continue,
                        ChildRoute::Subagent => {
                            for event in children.notification(&nthread, &method, &params) {
                                if !send(&event_tx, event).await {
                                    break 'main;
                                }
                            }
                            continue;
                        }
                    }
                }
                match method.as_str() {
                    "turn/started" => {
                        let id = turn_id(&params);
                        // Native voice handoffs start a turn without going through turn/start.
                        // Publish the boundary before its text/tool deltas reach the parked engine.
                        if done_current && !id.is_empty() && !router.is_completed(&id) {
                            done_current = false;
                            let (prev, next) = rotate(&mut assistant_message_id);
                            if !send(&event_tx, AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            }).await {
                                break 'main;
                            }
                        }
                        router.note_started(id);
                        if std::mem::take(&mut stop_when_announced)
                            && let Some(active) = router.active.clone()
                        {
                            send_turn_interrupt(&client, &thread_id, active);
                        }
                    }

                    "item/agentMessage/delta" => {
                        streamed_text.insert(item_id(&params));
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::TextDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta"
                    | "item/reasoning/summaryPartAdded" => {
                        for event in reasoning_streams.entry(thread_id.clone()).or_default()
                            .map(&method, &params)
                        {
                            if !send(&event_tx, event).await {
                                break 'main;
                            }
                        }
                    }

                    "item/started" | "item/completed" => {
                        let phase = if method == "item/started" {
                            Phase::Started
                        } else {
                            Phase::Completed
                        };
                        let item = params.get("item").unwrap_or(&Value::Null);
                        if matches!(item_type(item), "commandExecution" | "command_execution") {
                            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                            let changed = if phase == Phase::Started {
                                let pid = item.get("processId").and_then(Value::as_str).map(str::to_owned);
                                open_commands.insert(id.to_owned(), (pid, router.active.clone())).is_none()
                            } else {
                                open_commands.remove(id).is_some()
                            };
                            if changed {
                                turn.set_background(open_commands.len());
                            }
                        }
                        if matches!(item_type(item), "userMessage" | "user_message")
                            && let Some(at) = item
                                .get("clientId")
                                .and_then(Value::as_str)
                                .and_then(|id| accepted_steers.iter().position(|s| s == id))
                        {
                            for _ in 0..=at {
                                accepted_steers.pop_front();
                                let (prev, next) = rotate(&mut assistant_message_id);
                                if !send(&event_tx, AgentEvent::Steered {
                                    assistant_message_id: Some(prev),
                                    next_assistant_message_id: Some(next),
                                }).await {
                                    break 'main;
                                }
                            }
                        }
                        if phase == Phase::Completed {
                            let output = match item_type(item) {
                                "exitedReviewMode" => item.get("review").and_then(Value::as_str),
                                "contextCompaction" => Some("Context compacted."),
                                _ => None,
                            };
                            if let Some(text) = output
                                && !send(&event_tx, AgentEvent::TextDelta { text: text.into() }).await
                            { break 'main; }
                        }
                        if matches!(item_type(item), "agentMessage" | "agent_message") {
                            if phase == Phase::Completed {
                                // Fallback for non-streamed messages only.
                                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !streamed_text.contains(id)
                                    && !text.is_empty()
                                    && !send(&event_tx, AgentEvent::TextDelta { text: text.into() }).await
                                {
                                    break 'main;
                                }
                                // Codex emits several assistant messages per
                                // turn (commentary, final answer); their
                                // deltas carry no separator, so consecutive
                                // messages rendered concatenated
                                // ("…waiting.Beta's 90-second…" — live
                                // finding). Close each message as a
                                // paragraph.
                                if !send(
                                    &event_tx,
                                    AgentEvent::TextDelta {
                                        text: "\n\n".into(),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                                // Deltas are token chunks, not steering
                                // boundaries: the completed item is the
                                // provider-authoritative end of the text part.
                                let (prev, _next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::AssistantMessageCompleted {
                                        assistant_message_id: prev,
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        } else {
                            for ev in children.parent_item(phase, item) {
                                if !send(&event_tx, ev).await {
                                    break 'main;
                                }
                            }
                        }
                    }

                    "thread/tokenUsage/updated" => {
                        if let Some(usage) = normalize::context_usage_event(&params)
                            && !send(&event_tx, usage).await { break 'main; }
                        if let Some(usage) = usage_event(&params, &mut turn_usage_base) {
                            pending_usage = Some(usage);
                        }
                    }

                    // Codex's `update_plan` tool: the whole checklist, replaced
                    // on every call, with an in-progress step.
                    "turn/plan/updated" => {
                        for ev in normalize::plan_update_events(&params) {
                            if !send(&event_tx, ev).await { break 'main; }
                        }
                    }

                    "turn/completed" => {
                        let id = turn_id(&params);
                        if !router.settles(&id) {
                            tracing::debug!(
                                target: "zeron_harness::codex",
                                turn = %id,
                                "stale turn/completed ignored (not the turn in flight)"
                            );
                            router.note_completed(&id);
                            continue 'main;
                        }
                        router.note_completed(&id);
                        stop_when_announced = false;
                        // Accepted steers whose item never surfaced still
                        // joined this turn: confirm them before its end.
                        while accepted_steers.pop_front().is_some() {
                            let (prev, next) = rotate(&mut assistant_message_id);
                            if !send(&event_tx, AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            }).await {
                                break 'main;
                            }
                        }
                        // Item ids never span turns; without this the set grew
                        // one entry per message for a persistent session's life.
                        streamed_text.clear();
                        turn_usage_base = None;
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        let error = turn_error_message(&params).or_else(|| {
                            (params
                                .pointer("/turn/status")
                                .and_then(Value::as_str)
                                == Some("failed"))
                            .then(|| "Codex turn failed".to_owned())
                        });
                        let status = if interrupted || stopping {
                            DoneStatus::Interrupted
                        } else if error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        let limited_earlier = limit_error.take();
                        if status == DoneStatus::Errored
                            && let Some(limit) = normalize::turn_limit_event(
                                params.pointer("/turn/error").filter(|e| !e.is_null()),
                                limited_earlier.as_deref(),
                                rate_limits.as_ref(),
                                chrono::Utc::now(),
                            )
                            && !send(&event_tx, limit).await
                        {
                            break 'main;
                        }
                        if status == DoneStatus::Interrupted
                            || params.pointer("/turn/status").and_then(Value::as_str) == Some("interrupted")
                        {
                            prune_at = Some((tokio::time::Instant::now() + COMMAND_EXIT_GRACE, id.clone()));
                        }
                        stopping = false;
                        done_current = true;
                        if !send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                            break 'main;
                        }
                        // Persistent session: a steer that lost the race with
                        // this turn's end becomes the next turn now; otherwise
                        // stay alive for the mailbox — the caller owns teardown.
                        current_native = false;
                        if let Some((text, config)) = queued_steers.pop_front() {
                            if let Some(next) = config {
                                live = *next;
                            }
                            current_native = command_request(&text, &thread_id).ok().flatten().is_some();
                            if !steer_as_new_turn(
                                &client,
                                turn_params(&live, &text),
                                &mut router,
                                &event_tx,
                                &mut assistant_message_id,
                                &mut done_current,
                            )
                            .await
                            {
                                break 'main;
                            }
                        } else if !steering_open {
                            break 'main;
                        }
                    }

                    "turn/failed" => {
                        let id = turn_id(&params);
                        if !router.settles(&id) {
                            router.note_completed(&id);
                            continue 'main;
                        }
                        router.note_completed(&id);
                        stop_when_announced = false;
                        accepted_steers.clear();
                        turn_usage_base = None;
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        done_current = true;
                        if std::mem::take(&mut stopping) && !interrupted {
                            // The stopped turn's end: the thread lives on.
                            if !send(&event_tx, AgentEvent::Done {
                                status: DoneStatus::Interrupted,
                                result: None,
                                error: None,
                                session_id: Some(thread_id.clone()),
                            }).await {
                                break 'main;
                            }
                            continue 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let limited_earlier = limit_error.take();
                        if !interrupted
                            && let Some(limit) = normalize::turn_limit_event(
                                params.pointer("/turn/error").filter(|e| !e.is_null()),
                                limited_earlier.as_deref(),
                                rate_limits.as_ref(),
                                chrono::Utc::now(),
                            )
                            && !send(&event_tx, limit).await
                        {
                            break 'main;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: if interrupted {
                                    DoneStatus::Interrupted
                                } else {
                                    DoneStatus::Errored
                                },
                                result: None,
                                error: Some(
                                    turn_error_message(&params)
                                        .unwrap_or_else(|| "Codex turn failed".into()),
                                ),
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "turn/aborted" => {
                        let id = turn_id(&params);
                        if !router.settles(&id) {
                            router.note_completed(&id);
                            continue 'main;
                        }
                        router.note_completed(&id);
                        stop_when_announced = false;
                        accepted_steers.clear();
                        done_current = true;
                        prune_at = Some((tokio::time::Instant::now() + COMMAND_EXIT_GRACE, id.clone()));
                        if std::mem::take(&mut stopping) && !interrupted {
                            if !send(&event_tx, AgentEvent::Done {
                                status: DoneStatus::Interrupted,
                                result: None,
                                error: None,
                                session_id: Some(thread_id.clone()),
                            }).await {
                                break 'main;
                            }
                            continue 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: DoneStatus::Interrupted,
                                result: None,
                                error: None,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "error" => {
                        // 0.146.x nests it (`params.error.message`); older
                        // builds were flat (`params.message`) — accept both.
                        let message = params
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .or_else(|| params.get("message").and_then(Value::as_str))
                            .unwrap_or("Codex error")
                            .to_owned();
                        // A retried error is transient; only a final one
                        // can leave the turn usage-limited.
                        let error = params.get("error").filter(|e| e.is_object()).unwrap_or(&params);
                        if params.get("willRetry").and_then(Value::as_bool) != Some(true)
                            && normalize::error_is_limit(error, chrono::Utc::now())
                        {
                            limit_error = Some(message.clone());
                        }
                        if !send(&event_tx, AgentEvent::Error { message }).await {
                            break 'main;
                        }
                    }

                    "account/rateLimits/updated" => {
                        rate_limits = Some(params.clone());
                    }

                    // thread/status, mcpServer startup, account noise, … —
                    // unknown notification methods are tolerated by design.
                    _ => {}
                }
                }

                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(
                        &client,
                        id,
                        &method,
                        &params,
                        &live,
                        &file_changes,
                        &request_input,
                    );
                }

                // stdout EOF or reader gone: the app server exited.
                Some(Incoming::Eof) | None => break 'main,
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    // Native operations run at a turn boundary, never as text
                    // injected into an already running model turn. So does a
                    // changed configuration: `turn/steer` carries no model,
                    // effort or tier, and only a `turn/start` applies them.
                    // Later messages must stay behind queued ones: Steered
                    // acknowledgments retire the engine's accepted-message
                    // ledger in FIFO order.
                    if !done_current
                        && (!queued_steers.is_empty()
                            || current_native
                            || msg.config.is_some()
                            || !matches!(command_request(&text, &thread_id), Ok(None)))
                    {
                        queued_steers.push_back((text, msg.config));
                        continue 'main;
                    }
                    if let Some(next) = msg.config {
                        live = *next;
                    }
                    if let Some(expected) = router.active.clone() {
                        let client_id = new_message_id();
                        let steer_params = json!({
                            "threadId": thread_id,
                            "expectedTurnId": expected,
                            "clientUserMessageId": client_id,
                            "input": prompt_input(&text),
                        });
                        match client.request("turn/steer", steer_params).await {
                            Ok(_) => accepted_steers.push_back(client_id),
                            // A failed `turn/steer` does NOT mean the text is
                            // bad: most commonly the active turn finished
                            // between the UI send and this request. Queue it
                            // for redelivery as the next `turn/start` when the
                            // expected turn's end arrives (also the safe
                            // fallback for older Codex without steering).
                            Err(e) => {
                                tracing::debug!(
                                    target: "zeron_harness::codex",
                                    "turn/steer rejected (queued as next turn): {e}"
                                );
                                if router.active.as_deref() == Some(expected.as_str())
                                    && !router.is_completed(&expected)
                                {
                                    queued_steers.push_back((text, None));
                                } else {
                                    current_native = command_request(&text, &thread_id).ok().flatten().is_some();
                                    if !steer_as_new_turn(
                                        &client, turn_params(&live, &text), &mut router, &event_tx,
                                        &mut assistant_message_id, &mut done_current,
                                    ).await { break 'main; }
                                }
                            }
                        }
                    } else {
                        current_native = command_request(&text, &thread_id).ok().flatten().is_some();
                        if !steer_as_new_turn(
                            &client, turn_params(&live, &text), &mut router, &event_tx,
                            &mut assistant_message_id, &mut done_current,
                        ).await { break 'main; }
                    }
                }
                None => {
                    // Mailbox closed (the caller's graceful idle-reap): finish
                    // once nothing is in flight — mirrors codex.ts's steer loop
                    // `finish()` on a null take.
                    steering_open = false;
                    if done_current && router.active.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            // End the in-flight turn, not the app-server: its threads (child
            // agents included) keep running. Steers still waiting for this
            // turn's end go with it — a stopped turn never continues.
            _ = turn.stop_requested(), if !interrupted => {
                queued_steers.clear();
                match router.active.clone() {
                    Some(active) if !done_current => {
                        stopping = true;
                        if router.announced {
                            send_turn_interrupt(&client, &thread_id, active);
                        } else {
                            stop_when_announced = true;
                        }
                    }
                    // Between turns: nothing to interrupt. Settle the stop so
                    // the host sees the turn over.
                    _ => {
                        done_current = true;
                        if !send(&event_tx, AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            result: None,
                            error: None,
                            session_id: Some(thread_id.clone()),
                        }).await {
                            break 'main;
                        }
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                // The runtime is going: so is everything it started.
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    torn_down = crate::process::descendants(pid).await;
                }
                if let Some(turn) = router.active.clone() {
                    let client = client.clone();
                    let thread = thread_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client
                            .request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }))
                            .await
                        {
                            tracing::debug!(
                                target: "zeron_harness::codex",
                                "turn/interrupt failed (escalation will reap): {e}"
                            );
                        }
                    });
                    // Escalate if the app server doesn't wind down (turn/aborted)
                    // within the grace periods: SIGTERM, then SIGKILL.
                    if let Some(pid) = crate::process::signal_target(&child) {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(&pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(&pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to interrupt — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            _ = tokio::time::sleep_until(
                prune_at.as_ref().map_or_else(tokio::time::Instant::now, |(at, _)| *at)
            ), if prune_at.is_some() => {
                let (_, stopped) = prune_at.take().expect("guarded by if");
                open_commands.retain(|_, (pid, started_in)| {
                    started_in.as_deref() != Some(stopped.as_str())
                        || pid.as_deref().is_some_and(process_alive)
                });
                turn.set_background(open_commands.len());
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child KILLED mid-turn (OS memory pressure, `killall codex`)
            // must not read as a silent success — codex.ts's signal-death
            // handling, reduced to the turn-in-flight case.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(client.close_reason().unwrap_or_else(|| {
                        crate::crash_message("codex app-server", status, &stderr_tail)
                    })),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        }
    }

    crate::shutdown_agent(&mut child, torn_down, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Let the user's Codex config win over the `turn/start` defaults. Both
/// `summary` and `sandboxPolicy` override the thread "for this turn and
/// subsequent turns" (app-server schema, 0.155), so sending ours replaced
/// what the user set: `model_reasoning_summary = "none"` (providers such as
/// Bedrock reject `detailed`/`concise` on every turn) and
/// `[sandbox_workspace_write] network_access` and friends (a missing field
/// reads as false). The user's summary choice is left to the config; their
/// workspace-write settings fill the policy, extra roots joined with ours.
fn merge_user_turn_config(params: &mut serde_json::Map<String, Value>, config: &Value) {
    if config
        .get("model_reasoning_summary")
        .is_some_and(|summary| !summary.is_null())
    {
        params.remove("summary");
    }
    let Some(user) = config.get("sandbox_workspace_write").filter(|v| v.is_object()) else {
        return;
    };
    let Some(policy) = params
        .get_mut("sandboxPolicy")
        .and_then(Value::as_object_mut)
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("workspaceWrite"))
    else {
        return;
    };
    for (theirs, ours) in [
        ("network_access", "networkAccess"),
        ("exclude_slash_tmp", "excludeSlashTmp"),
        ("exclude_tmpdir_env_var", "excludeTmpdirEnvVar"),
    ] {
        if let Some(value) = user.get(theirs).and_then(Value::as_bool) {
            policy.insert(ours.into(), value.into());
        }
    }
    let mut roots: Vec<Value> = policy
        .get("writableRoots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for root in user
        .get("writable_roots")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if root.is_string() && !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    if !roots.is_empty() {
        policy.insert("writableRoots".into(), Value::Array(roots));
    }
}

/// How long an interrupted turn's commands get to exit before a process
/// still running counts as background work.
const COMMAND_EXIT_GRACE: Duration = Duration::from_secs(3);

/// Whether a process the app-server reported is still running.
fn process_alive(pid: &str) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only probes for existence; nothing is delivered.
        pid.parse::<i32>()
            .is_ok_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
    }
    #[cfg(not(unix))]
    {
        // Unknown: count it gone, which leaves the idle reaper as it was.
        let _ = pid;
        false
    }
}

/// Ask the app-server to end `turn` (fire-and-forget: its end arrives as the
/// turn's own completion notification).
fn send_turn_interrupt(client: &RpcClient, thread_id: &str, turn: String) {
    let client = client.clone();
    let thread = thread_id.to_owned();
    tokio::spawn(async move {
        if let Err(e) = client
            .request(
                "turn/interrupt",
                json!({ "threadId": thread, "turnId": turn }),
            )
            .await
        {
            tracing::warn!(target: "zeron_harness::codex", "turn/interrupt failed: {e}");
        }
    });
}

/// Deliver a steer as a fresh `turn/start` on the same thread (the fallback
/// leg of the steer race, and the between-turns delivery path). Returns false
/// when the loop should end (turn/start failed or the consumer hung up).
async fn steer_as_new_turn(
    client: &RpcClient,
    params: Value,
    router: &mut TurnRouter,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    assistant_message_id: &mut String,
    done_current: &mut bool,
) -> bool {
    match start_turn(client, params).await {
        Ok(id) => {
            router.adopt_started(id);
            *done_current = false;
            let (prev, next) = rotate(assistant_message_id);
            send(
                event_tx,
                AgentEvent::Steered {
                    assistant_message_id: Some(prev),
                    next_assistant_message_id: Some(next),
                },
            )
            .await
        }
        Err(e) => {
            let _ = send(
                event_tx,
                AgentEvent::Error {
                    message: format!("Steering failed: {e}"),
                },
            )
            .await;
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Approvals (approval-as-input parity with godspeed's UX)
// ---------------------------------------------------------------------------

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// The wire approval policy and sandbox for a run's access mode. Title runs
/// stay read-only and never ask.
fn access_policy(request: &RunRequest, title_only: bool) -> (&'static str, SandboxLevel) {
    if title_only {
        return ("never", SandboxLevel::ReadOnly);
    }
    match request.access_mode() {
        AccessMode::Supervised => ("untrusted", SandboxLevel::ReadOnly),
        AccessMode::AutoAcceptEdits => ("untrusted", SandboxLevel::WorkspaceWrite),
        AccessMode::Auto => ("on-request", SandboxLevel::WorkspaceWrite),
        AccessMode::FullAccess => ("never", SandboxLevel::DangerFullAccess),
    }
}

/// Track fileChange items (from `item/started`, `item/fileChange/patchUpdated`
/// and `item/completed`) so their approval can show what changes.
fn note_file_changes(cache: &mut HashMap<String, Value>, method: &str, params: &Value) {
    match method {
        "item/started" | "item/completed" => {
            let item = params.get("item").unwrap_or(&Value::Null);
            if !matches!(item_type(item), "fileChange" | "file_change") {
                return;
            }
            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            if method == "item/completed" {
                cache.remove(id);
            } else if let Some(changes) = item.get("changes") {
                cache.insert(id.to_owned(), changes.clone());
            }
        }
        "item/fileChange/patchUpdated" => {
            if let (Some(id), Some(changes)) = (
                params.get("itemId").and_then(Value::as_str),
                params.get("changes"),
            ) {
                cache.insert(id.to_owned(), changes.clone());
            }
        }
        _ => {}
    }
}

/// Serve one server→client request. Approvals the access mode lets through
/// (full access; in-workspace file changes under auto-accept edits and
/// auto) are accepted here; the rest wait on the user through the input
/// bridge (in a subtask so the message loop keeps flowing). Anything else is
/// rejected as unsupported so the server never wedges awaiting a reply.
fn handle_server_request(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
    live: &RunRequest,
    file_changes: &HashMap<String, Value>,
    request_input: &Arc<RequestInputFn>,
) {
    // A tool's user-input request (EXPERIMENTAL, codex 0.146.x) is a CONTENT
    // question, never auto-approvable — route it to the input bridge and
    // answer keyed by question id, `{ answers: { <id>: { answers: [..] } } }`.
    if method == "item/tool/requestUserInput" {
        let questions = user_input_questions(params);
        if questions.is_empty() {
            client.respond(&id, json!({ "answers": {} }));
            return;
        }
        let client = client.clone();
        let request_input = Arc::clone(request_input);
        tokio::spawn(async move {
            let asked: Vec<UserInputQuestion> = questions.iter().map(|(_, q)| q.clone()).collect();
            let answers = (request_input)(asked).await.unwrap_or_default();
            let mut by_id = serde_json::Map::new();
            for (wire_id, q) in &questions {
                let labels: Vec<Value> = answers
                    .iter()
                    .find(|a| a.question_id == q.id)
                    .map(|a| a.labels.iter().cloned().map(Value::String).collect())
                    .unwrap_or_default();
                by_id.insert(wire_id.clone(), json!({ "answers": labels }));
            }
            client.respond(&id, json!({ "answers": by_id }));
        });
        return;
    }
    let Some(approval) = approval_request(method, params, file_changes, &live.cwd) else {
        tracing::debug!(
            target: "zeron_harness::codex",
            "unhandled server request: {method}"
        );
        client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
        return;
    };
    let in_workspace = crate::approval::paths_within(
        approval.paths.iter().map(String::as_str),
        &live.cwd,
        live.writable_roots(),
    );
    if crate::approval::auto_decision(live.access_mode(), approval.kind, in_workspace) {
        client.respond(&id, approval_response(method, params, ApprovalDecision::Approve));
        return;
    }
    let client = client.clone();
    let request_input = Arc::clone(request_input);
    let method = method.to_owned();
    let params = params.clone();
    tokio::spawn(async move {
        // The engine's input bridge owns the `InputRequested`/`InputResolved`
        // lifecycle (it mints the request id the resolver is parked under);
        // emitting our own copy here doubled the doc's input part with an id
        // `respond_input` could never match. A dropped bridge declines.
        let decision = crate::approval::ask(request_input.as_ref(), approval).await;
        client.respond(&id, approval_response(&method, &params, decision));
    });
}

/// Parse `item/tool/requestUserInput` questions into (wire id, question)
/// pairs, tolerant of field spellings; answers key by the WIRE id.
fn user_input_questions(params: &Value) -> Vec<(String, UserInputQuestion)> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(ix, q)| {
            let field = |keys: [&str; 3]| {
                keys.iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_str))
                    .unwrap_or("")
                    .to_owned()
            };
            let wire_id = {
                let id = field(["id", "questionId", "question_id"]);
                if id.is_empty() { format!("q{ix}") } else { id }
            };
            let question = UserInputQuestion {
                id: new_message_id(),
                header: {
                    let h = field(["header", "title", "label"]);
                    if h.is_empty() {
                        "Codex question".into()
                    } else {
                        h
                    }
                },
                question: field(["question", "prompt", "text"]),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|op| match op {
                        Value::String(s) => s.clone(),
                        other => other
                            .get("label")
                            .or_else(|| other.get("value"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    })
                    .collect(),
                prefill: None,
                multiline: false,
                multi_select: ["multiSelect", "multi_select"]
                    .iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_bool))
                    .unwrap_or(false),
                    approval: None,
            };
            (wire_id, question)
        })
        .collect()
}

/// Describe a codex approval request, or `None` for a method that is not one.
fn approval_request(
    method: &str,
    params: &Value,
    file_changes: &HashMap<String, Value>,
    cwd: &str,
) -> Option<ApprovalRequest> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut approval = match method {
        "item/commandExecution/requestApproval" => {
            let command = match params.get("command") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Array(parts)) => Some(
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            };
            let network = params
                .get("networkApprovalContext")
                .filter(|c| !c.is_null());
            let mut approval = match network {
                Some(context) => ApprovalRequest::new(
                    ApprovalKind::Network,
                    format!(
                        "Allow network access to {}",
                        context["host"].as_str().unwrap_or("a host")
                    ),
                ),
                None => ApprovalRequest::new(ApprovalKind::Command, "Run a command"),
            };
            approval.command = command;
            approval.cwd = text("cwd").or_else(|| (!cwd.is_empty()).then(|| cwd.to_owned()));
            approval
        }
        "item/fileChange/requestApproval" => {
            let changes = text("itemId")
                .and_then(|item| file_changes.get(&item))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let paths: Vec<String> = changes
                .iter()
                .filter_map(|c| c.get("path").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            let title = match paths.as_slice() {
                [] => "Edit files".to_owned(),
                [one] => format!("Edit {one}"),
                [first, rest @ ..] => format!("Edit {first} and {} more", rest.len()),
            };
            let mut approval = ApprovalRequest::new(ApprovalKind::FileChange, title);
            let diff: String = changes
                .iter()
                .filter_map(|c| {
                    let diff = c.get("diff").and_then(Value::as_str)?;
                    let path = c.get("path").and_then(Value::as_str).unwrap_or_default();
                    Some(if diff.starts_with("---") || diff.starts_with("diff ") {
                        diff.to_owned()
                    } else {
                        format!("--- a/{path}\n+++ b/{path}\n{diff}")
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            approval.diff = (!diff.is_empty()).then_some(diff);
            approval.paths = paths;
            if let Some(root) = text("grantRoot") {
                approval.paths.push(root);
            }
            approval.cwd = (!cwd.is_empty()).then(|| cwd.to_owned());
            approval
        }
        "item/permissions/requestApproval" => {
            let permissions = params.get("permissions").unwrap_or(&Value::Null);
            let network = permissions
                .get("network")
                .is_some_and(|n| !n.is_null());
            let file_system = permissions.get("fileSystem").filter(|f| !f.is_null());
            let mut approval = if network && file_system.is_none() {
                ApprovalRequest::new(ApprovalKind::Network, "Allow network access")
            } else {
                ApprovalRequest::new(ApprovalKind::Other, "Grant extra permissions")
            };
            if let Some(fs) = file_system {
                for key in ["write", "read"] {
                    approval.paths.extend(
                        fs.get(key)
                            .and_then(Value::as_array)
                            .map(Vec::as_slice)
                            .unwrap_or_default()
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                }
            }
            approval.cwd = text("cwd");
            approval
        }
        _ => return None,
    };
    approval.reason = text("reason");
    Some(approval)
}

/// The response body for a decision, per method.
fn approval_response(method: &str, params: &Value, decision: ApprovalDecision) -> Value {
    if method == "item/permissions/requestApproval" {
        let scope = if decision == ApprovalDecision::ApproveForSession {
            "session"
        } else {
            "turn"
        };
        let granted = if decision.approves() {
            params.get("permissions").cloned().unwrap_or(json!({}))
        } else {
            json!({})
        };
        return json!({ "permissions": granted, "scope": scope });
    }
    let decision = match decision {
        ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => "accept",
        ApprovalDecision::ApproveForSession => "acceptForSession",
        ApprovalDecision::Decline | ApprovalDecision::DeclineAlways => "decline",
        ApprovalDecision::Cancel => "cancel",
    };
    json!({ "decision": decision })
}

use crate::{Signal, send_signal, shutdown_child};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_schema_and_legacy_visibility_are_compatible() {
        let page = json!({"data":[{"model":"current", "hidden":false, "isDefault":true,
            "description":"Current model", "upgrade":"next", "upgradeInfo":{"retirementAt":"2026-12-01"},
            "availabilityNux":{"message":"Available"}, "serviceTiers":["default","fast"],
            "defaultServiceTier":"default", "inputModalities":["text","image"]}], "nextCursor":"next-page"});
        let (models, next) = parse_model_list_page(&page);
        assert_eq!(
            models[0].0.description.as_deref(),
            Some("Current model (upgrade: next)")
        );
        assert!(models[0].1);
        assert_eq!(next.as_deref(), Some("next-page"));
        assert!(!legacy_model_page(&page));
        assert!(legacy_model_page(&json!({"data":[{"model":"old"}]})));
        assert!(!legacy_model_page(&json!({"data":[]})));
    }

    #[test]
    fn approvals_describe_commands_file_changes_and_permissions() {
        let none = HashMap::new();
        let command = approval_request(
            "item/commandExecution/requestApproval",
            &json!({"itemId": "c1", "command": ["git", "push", "--force"], "cwd": "/w", "reason": "pushes"}),
            &none,
            "/repo",
        )
        .unwrap();
        assert_eq!(command.kind, ApprovalKind::Command);
        assert_eq!(command.command.as_deref(), Some("git push --force"));
        assert_eq!(command.cwd.as_deref(), Some("/w"));
        assert_eq!(command.reason.as_deref(), Some("pushes"));

        let network = approval_request(
            "item/commandExecution/requestApproval",
            &json!({"command": "curl x", "networkApprovalContext": {"host": "example.com", "protocol": "https"}}),
            &none,
            "/repo",
        )
        .unwrap();
        assert_eq!(network.kind, ApprovalKind::Network);
        assert!(network.title.contains("example.com"));

        let mut changes = HashMap::new();
        changes.insert(
            "f1".to_owned(),
            json!([{"path": "/repo/a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-a\n+b\n"}]),
        );
        let file = approval_request(
            "item/fileChange/requestApproval",
            &json!({"itemId": "f1"}),
            &changes,
            "/repo",
        )
        .unwrap();
        assert_eq!(file.kind, ApprovalKind::FileChange);
        assert_eq!(file.paths, ["/repo/a.rs"]);
        assert!(file.diff.unwrap().starts_with("--- a//repo/a.rs"));

        assert!(approval_request("execCommandApproval", &json!({}), &none, "/").is_none());
    }

    /// pingdotgg/t3code#14952 and #13987: `turn/start`'s `summary` and
    /// `sandboxPolicy` override the thread's config for every later turn, so
    /// ours replaced `model_reasoning_summary = "none"` and
    /// `sandbox_workspace_write.network_access`. Config shapes from
    /// `codex app-server generate-json-schema` (ConfigReadResponse, 0.155).
    #[test]
    fn turn_overrides_merge_with_the_users_codex_config() {
        let turn = || -> serde_json::Map<String, Value> {
            let mut p = serde_json::Map::new();
            p.insert("summary".into(), "auto".into());
            p.insert(
                "sandboxPolicy".into(),
                sandbox_policy_value(SandboxLevel::WorkspaceWrite, &["/data/a1".to_owned()]),
            );
            p
        };
        // Nothing configured: our defaults stand.
        let mut p = turn();
        merge_user_turn_config(&mut p, &json!({"model_reasoning_summary": null, "sandbox_workspace_write": null}));
        assert_eq!(p, turn());

        let mut p = turn();
        merge_user_turn_config(
            &mut p,
            &json!({
                "model_reasoning_summary": "none",
                "sandbox_workspace_write": {
                    "network_access": false, "exclude_slash_tmp": true,
                    "exclude_tmpdir_env_var": false, "writable_roots": ["/cache", "/data/a1"],
                },
            }),
        );
        assert!(p.get("summary").is_none(), "the config's summary applies");
        assert_eq!(
            p["sandboxPolicy"],
            json!({
                "type": "workspaceWrite", "networkAccess": false, "excludeSlashTmp": true,
                "excludeTmpdirEnvVar": false, "writableRoots": ["/data/a1", "/cache"],
            })
        );
        // Only workspace-write takes those settings.
        let mut p = serde_json::Map::new();
        p.insert("sandboxPolicy".into(), sandbox_policy_value(SandboxLevel::ReadOnly, &[]));
        let before = p.clone();
        merge_user_turn_config(&mut p, &json!({"sandbox_workspace_write": {"network_access": true}}));
        assert_eq!(p, before);
    }

    #[test]
    fn decisions_map_to_codex_responses() {
        let method = "item/commandExecution/requestApproval";
        let params = json!({});
        for (decision, wire) in [
            (ApprovalDecision::Approve, "accept"),
            (ApprovalDecision::ApproveForSession, "acceptForSession"),
            (ApprovalDecision::Decline, "decline"),
            (ApprovalDecision::Cancel, "cancel"),
        ] {
            assert_eq!(
                approval_response(method, &params, decision),
                json!({ "decision": wire })
            );
        }
        let asked = json!({"permissions": {"network": {"enabled": true}}});
        let method = "item/permissions/requestApproval";
        assert_eq!(
            approval_response(method, &asked, ApprovalDecision::ApproveForSession),
            json!({"permissions": {"network": {"enabled": true}}, "scope": "session"})
        );
        assert_eq!(
            approval_response(method, &asked, ApprovalDecision::Decline),
            json!({"permissions": {}, "scope": "turn"})
        );
    }

    #[test]
    fn access_modes_pick_policy_and_sandbox() {
        let mut request = RunRequest {
            prompt: String::new(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/repo".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            resume: None,
            attachments: Vec::new(),
            worktree: None,
            mcp: None,
            access: None,
            env: Default::default(),
        };
        for (mode, policy, sandbox) in [
            (AccessMode::Supervised, "untrusted", SandboxLevel::ReadOnly),
            (AccessMode::AutoAcceptEdits, "untrusted", SandboxLevel::WorkspaceWrite),
            (AccessMode::Auto, "on-request", SandboxLevel::WorkspaceWrite),
            (AccessMode::FullAccess, "never", SandboxLevel::DangerFullAccess),
        ] {
            request.set_access_mode(mode);
            assert_eq!(access_policy(&request, false), (policy, sandbox), "{mode:?}");
        }
        assert_eq!(access_policy(&request, true), ("never", SandboxLevel::ReadOnly));
        assert_eq!(
            sandbox_policy_value(SandboxLevel::WorkspaceWrite, &["/data/a1".to_owned()]),
            json!({"type": "workspaceWrite", "networkAccess": true, "writableRoots": ["/data/a1"]})
        );
    }

    #[test]
    fn model_page_skips_hidden_and_unknown_efforts() {
        let page = json!({
            "data": [
                {
                    "id": "hidden",
                    "model": "hidden",
                    "displayName": "Hidden",
                    "hidden": true,
                    "supportedReasoningEfforts": [{ "reasoningEffort": "high" }]
                },
                {
                    "id": "gpt-6-astra",
                    "model": "gpt-6-astra",
                    "displayName": "GPT-6-Astra",
                    "description": "  Most capable  ",
                    "hidden": false,
                    "supportedReasoningEfforts": [
                        { "reasoningEffort": "high" },
                        { "reasoningEffort": "future" }
                    ],
                    "serviceTiers": [{ "id": "priority", "name": "Fast" }],
                    "additionalSpeedTiers": ["fast"],
                    "defaultServiceTier": null,
                    "isDefault": true
                }
            ],
            "nextCursor": "next"
        });
        let (models, cursor) = parse_model_list_page(&page);
        assert_eq!(cursor.as_deref(), Some("next"));
        assert_eq!(models.len(), 1);
        let (astra, is_default) = &models[0];
        assert_eq!(astra.id, "gpt-6-astra");
        assert_eq!(astra.description.as_deref(), Some("Most capable"));
        assert_eq!(astra.reasoning_levels, vec![ReasoningLevel::High]);
        assert!(*is_default);
        assert_eq!(astra.options[0].choices.len(), 2);
        assert_eq!(astra.options[0].choices[1].id, "fast");
    }

    #[test]
    fn turn_router_never_revives_completed_turns() {
        let mut r = TurnRouter::default();
        r.note_completed("t-1");
        // The turn/start response arriving after turn/completed must not
        // resurrect the turn.
        r.adopt_started("t-1".into());
        assert_eq!(r.active, None);
        // Nor may a late turn/started notification.
        r.note_started("t-1".into());
        assert_eq!(r.active, None);

        r.note_started("t-2".into());
        assert_eq!(r.active.as_deref(), Some("t-2"));
        // A replacement started turn retires the stale one.
        r.note_started("t-3".into());
        assert_eq!(r.active.as_deref(), Some("t-3"));
        assert!(r.is_completed("t-2"));
    }
}

#[cfg(test)]
mod mcp_injection_tests {
    use super::*;

    #[test]
    fn codex_mcp_overrides_use_the_dotted_mcp_servers_keys() {
        let mcp = zeron_proto::McpServer {
            name: "godspeed".into(),
            command: "/opt/godspeed/godspeed".into(),
            args: vec!["mcp".into()],
            env: [("GODSPEED_CHAT_ID".to_owned(), "chat-1".to_owned())]
                .into_iter()
                .collect(),
        };
        let overrides: serde_json::Map<String, Value> =
            codex_mcp_overrides(&mcp).into_iter().collect();
        assert_eq!(overrides["mcp_servers.godspeed.command"], "/opt/godspeed/godspeed");
        assert_eq!(overrides["mcp_servers.godspeed.args"], json!(["mcp"]));
        assert_eq!(
            overrides["mcp_servers.godspeed.env"],
            json!({ "GODSPEED_CHAT_ID": "chat-1" })
        );
    }
}

#[cfg(test)]
mod skill_discovery_tests {
    use super::*;
    #[test]
    fn selected_skills_use_native_identity_for_initial_and_steered_inputs() {
        use zeron_proto::invocation::Invocation;
        let a = Invocation::Skill {
            command: None,
            name: "review".into(),
            path: "/repo/a b/SKILL.md".into(),
        };
        let b = Invocation::Skill {
            command: None,
            name: "review".into(),
            path: "/repo/other/SKILL.md".into(),
        };
        let raw = format!("Use {} then {} and {}", a.link(), b.link(), a.link());
        let input = prompt_input(&raw);
        assert_eq!(input.as_array().unwrap().len(), 3);
        assert_eq!(
            input[1],
            json!({"type":"skill","name":"review","path":"/repo/a b/SKILL.md"})
        );
        assert_eq!(input[2]["path"], "/repo/other/SKILL.md");
        assert!(!input[0]["text"].as_str().unwrap().contains("godspeed-invoke:"));
        for raw in [
            "$review".into(),
            format!("`{}`", a.link()),
            format!("\\{}", a.link()),
            format!("![skill example {}](example.png)", a.link()),
            format!("    {}", a.link()),
        ] {
            let input = prompt_input(&raw);
            assert_eq!(input.as_array().unwrap().len(), 1);
            assert_eq!(input[0]["text"], raw);
        }
        let command = Invocation::Command {
            name: "review".into(),
        };
        assert_eq!(
            command_request(&format!("  {} check", command.link()), "t")
                .unwrap()
                .unwrap()
                .0,
            "review/start"
        );
        assert!(command_request(&format!("/review {}", a.link()), "t").is_err());
    }

    #[test]
    fn backtick_labels_keep_native_skill_identity_with_repeated_selections() {
        use zeron_proto::invocation::{Invocation, harness_prompt};
        let skill = Invocation::Skill {
            command: None,
            name: "review`ui".into(),
            path: "/repo/é skill/SKILL.md".into(),
        };
        let file = zeron_proto::file_mentions::local_file_link("src/a`b.rs", false);
        let raw = format!("{} on {file} and {}", skill.link(), skill.link());
        let input = prompt_input(&harness_prompt(&raw, HarnessId::Codex));
        assert_eq!(input.as_array().unwrap().len(), 2);
        assert_eq!(
            input[1],
            json!({"type":"skill", "name":"review`ui", "path":"/repo/é skill/SKILL.md"})
        );
        let text = input[0]["text"].as_str().unwrap();
        assert!(!text.contains("godspeed-invoke:"));
        assert!(!text.contains("godspeed-file:"));
        assert_eq!(text.matches("/repo/%C3%A9%20skill/SKILL.md").count(), 2);
    }

    #[test]
    fn commands_map_arguments_and_leave_inline_mentions_literal() {
        assert!(
            command_request("please /review this", "t")
                .unwrap()
                .is_none()
        );
        for code in [
            "    /review",
            "\t/review",
            "\n    /compact",
            "\u{a0}/review",
            "`/review`",
            "```\n/review\n```",
        ] {
            assert!(command_request(code, "t").unwrap().is_none());
        }
        assert!(command_request("/tmp/file.rs", "t").unwrap().is_none());
        assert!(command_request("/tmp", "t").unwrap().is_none());
        assert!(command_request("/compact extra", "t").is_err());
        assert!(command_request("/model", "t").is_err());
        let (method, params) = command_request("/review check errors", "t")
            .unwrap()
            .unwrap();
        assert_eq!(method, "review/start");
        assert_eq!(
            params["target"],
            json!({"type":"custom","instructions":"check errors"})
        );
        assert_eq!(params["delivery"], "inline");
    }

    #[test]
    fn catalog_rejects_invalid_identities_without_changing_valid_names() {
        use zeron_proto::invocation::{Invocation, invocation_links};
        let mut entries = vec![];
        for name in [
            "",
            "two words",
            " padded",
            "padded ",
            "line\nbreak",
            "tab\tname",
            "nul\0name",
            "non\u{a0}breaking",
        ] {
            entries.push(json!({"name":name,"path":"/repo/SKILL.md"}));
        }
        for path in ["", "/repo/line\nbreak/SKILL.md", "/repo/\0/SKILL.md"] {
            entries.push(json!({"name":"invalid-path","path":path}));
        }
        for (name, path) in [
            (r"review[ui]\draft`", "/repo/é skill/SKILL.md"),
            ("审查-é", "harness-skill:custom:审查-é"),
        ] {
            entries.push(json!({"name":name,"path":path}));
        }
        let skills = parse_skills(&json!({"data":[{"skills":entries}]}));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, r"review[ui]\draft`");
        assert_eq!(skills[1].path, "harness-skill:custom:审查-é");
        for skill in skills {
            let invocation = Invocation::Skill {
                name: skill.name,
                path: skill.path,
                command: skill.command,
            };
            assert_eq!(invocation_links(&invocation.link())[0].1, invocation);
        }
    }

    #[test]
    fn preserves_paths_enabled_state_and_duplicate_names() {
        let value = json!({"data": [{"skills": [
            {"name":"review", "path":"/a/SKILL.md", "description":"A", "enabled":true},
            {"name":"review", "path":"/b/SKILL.md", "description":"B", "enabled":false},
            {"name":"review", "path":"/a/SKILL.md", "description":"duplicate"}
        ]}]});
        let skills = parse_skills(&value);
        assert_eq!(skills.len(), 2);
        assert_ne!(skills[0].path, skills[1].path);
        assert!(!skills[1].enabled);
    }
}
