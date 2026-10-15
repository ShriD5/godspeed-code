//! Claude Code harness: spawns the installed `claude` CLI and speaks its
//! stream-json protocol directly — no adapter process in between. Resurrected
//! from the pre-ACP driver (see docs/research/harness.md) and modernized
//! against CLI 2.1.228.
//!
//! - stdout JSONL frames are normalized into [`AgentEvent`]s (init dedupe,
//!   subagent tagging, typed tool decoding, error-code mapping).
//! - PERMISSIONS ride the stdio control channel: `--permission-prompt-tool
//!   stdio` (undocumented — absent from `claude --help`, but it is the same
//!   transport the Claude Agent SDK's `query()` drives, and was re-validated
//!   live against 2.1.228: `can_use_tool` control requests arrive and
//!   allow/deny responses are honored). The alternative channel — an MCP
//!   permission tool — needs a server process and was rejected.
//!   `AskUserQuestion` round-trips through [`RunControls::request_input`].
//! - ACCESS MODES map onto the CLI's own `--permission-mode` (validated
//!   against `claude --help` and the Agent SDK typings, 2.1.295):
//!   supervised = `default`, auto-accept edits = `acceptEdits`, auto =
//!   `auto` (the CLI's classifier), full access = `bypassPermissions`. The
//!   CLI decides what needs a yes; every `can_use_tool` it still raises
//!   becomes an [`ApprovalRequest`] the user answers. "Approve for this
//!   session" echoes the request's `permission_suggestions` back as
//!   `updatedPermissions` with `destination: "session"` (in memory only,
//!   never written to settings files). A mode change between non-bypass
//!   modes applies in place (`set_permission_mode`); entering or leaving
//!   bypass needs a relaunch. Extra writable roots ride `--add-dir`.
//! - DONE is the CLI's own `result` frame, eagerly: background work (a
//!   spawned subagent) never holds the turn. The CLI natively runs a second
//!   wake turn when a background task finishes — a fresh `init` (same
//!   session id, deduped) plus another `result` — and both are forwarded;
//!   the engine's parked-session resume path turns them into the
//!   done→Working→done wake.
//! - SUBAGENT frames arrive on the same stdout tagged with a top-level
//!   `parent_tool_use_id`; they are wrapped in [`AgentEvent::Subagent`] and
//!   NEVER folded into the parent feed (a background subagent interleaves
//!   with the parent's own stream — folding them in split contiguous text
//!   around phantom tool calls).
//! - A `result` that is not a turn's end: every stdin user line carries a
//!   uuid, and the CLI reports each one's `command_lifecycle`. A result
//!   while one of ours is still written/queued is a boundary (a `now` steer
//!   aborting the turn, resume settling dead background tasks) — that
//!   message's turn ends with its own result. See [`Commands`].
//! - Steering: queued [`SteerMessage`]s are written to stdin as user lines at
//!   any time; the CLI folds them into the running turn at its own step
//!   boundary. A steer's changed model/effort/options apply to the live
//!   process first (`set_model`, `apply_flag_settings`) — never a restart.
//! - Turn stop ([`crate::TurnControl::stop_turn`]): the `interrupt` control
//!   request with `cancel_queued` ends the turn and the CLI keeps reading
//!   stdin. The run opens with an `initialize` declaring
//!   `perTaskStopAffordance`, which makes that interrupt spare background
//!   agents (the CLI otherwise stops them all); background shells survive
//!   either way. `background_tasks_changed` reports the live set to the host.
//! - Interrupt: cancelling [`RunControls::interrupt`] tears the runtime down —
//!   the interrupt control request, then SIGTERM and SIGKILL.

pub mod catalog;
mod discovery;
mod normalize;
mod wire;

/// Transcript importers decode Claude `tool_use` blocks exactly as live runs do.
pub use normalize::decode_tool_use;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use zeron_proto::{
    AccessMode, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalRequest, DoneStatus,
    HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand, SteeringMode, UserInputAnswer,
    UserInputQuestion,
};

use crate::process::{Child, ChildStdin, Command, Stdio};
use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};
use catalog::{apply_ultrathink, to_effort};
use normalize::Normalizer;
use wire::{ControlRequestFrame, Frame, allow_response, control_response_line};

/// Locate the device's installed Claude Code CLI: our own PATH, then the
/// login-shell PATH snapshot (the user's shell init shapes PATH in ways a
/// GUI/service launch never sees — see [`crate::shell_env`]), then known
/// install locations as a last resort. The `CLAUDE_CODE_EXECUTABLE` override
/// is applied by [`ClaudeHarness::resolve_executable`], so availability and
/// launches agree on one resolution order. Resolved per call — cheap after
/// the snapshot is cached.
fn resolve_claude_executable() -> Option<PathBuf> {
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".claude").join("local").join("claude"));
        extra.push(home.join(".local").join("bin").join("claude"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/claude"));
    extra.push(PathBuf::from("/usr/local/bin/claude"));
    crate::executable::find_on_paths("claude", extra)
}

/// The inline `--mcp-config` JSON for an injected server (the CLI accepts a
/// JSON string as well as a file path).
fn mcp_config_arg(mcp: &zeron_proto::McpServer) -> String {
    serde_json::json!({
        "mcpServers": {
            &mcp.name: {
                "command": mcp.command,
                "args": mcp.args,
                "env": mcp.env,
            }
        }
    })
    .to_string()
}

/// The `--model` value: the 1M context window is selected via a model-id
/// suffix (`sonnet[1m]`), exactly how the CLI itself does it.
fn model_arg(request: &RunRequest) -> Option<String> {
    let model = request.model.as_ref()?;
    let one_m = request
        .model_options
        .get("contextWindow")
        .and_then(Value::as_str)
        == Some("1m");
    Some(if one_m {
        format!("{model}[1m]")
    } else {
        model.clone()
    })
}

/// Every flag-layer setting a request launches with, for
/// `apply_flag_settings` on a live process: `null` drops the override back
/// to the user's own settings, exactly as a launch without the flag
/// (verified live on 2.1.286 through `get_settings`).
fn flag_settings(request: &RunRequest) -> Value {
    let on = |set: bool| if set { Value::Bool(true) } else { Value::Null };
    serde_json::json!({
        "effortLevel": to_effort(request.reasoning, request.model.as_deref()),
        "fastMode": on(option_is_on(&request.model_options, "fastMode")),
        "alwaysThinkingEnabled": on(option_is_on(&request.model_options, "thinking")),
        "ultracode": on(request.reasoning == Some(ReasoningLevel::Ultracode)),
    })
}

fn option_is_on(options: &serde_json::Map<String, Value>, key: &str) -> bool {
    match options.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "on" || s == "true",
        _ => false,
    }
}

/// The Claude Code harness. Construct with [`ClaudeHarness::new`]; tests point
/// it at a fake CLI with [`ClaudeHarness::with_executable`].
pub struct ClaudeHarness {
    executable: Option<PathBuf>,
    /// Grace between the interrupt control request and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    initialize: discovery::InitializeCache,
    models_cache: crate::catalog::Catalog,
    workspace_commands: crate::skills::CommandDiscovery,
}

impl Default for ClaudeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            initialize: discovery::InitializeCache::default(),
            models_cache: crate::catalog::Catalog::default(),
            workspace_commands: crate::skills::CommandDiscovery::default(),
        }
    }
}

impl ClaudeHarness {
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
        if let Some(p) = std::env::var_os("CLAUDE_CODE_EXECUTABLE")
            && !p.is_empty()
        {
            return crate::executable::validate_native_override(&PathBuf::from(p));
        }
        resolve_claude_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "claude (searched PATH, the login shell's PATH, ~/.claude/local, \
                 ~/.local/bin, /opt/homebrew/bin, /usr/local/bin, and \
                 fnm/nvm/volta/pnpm/bun install dirs; Windows also checks USERPROFILE \
                 and explicit NVM_SYMLINK/VOLTA_HOME/PNPM_HOME; set \
                 CLAUDE_CODE_EXECUTABLE to override)"
                    .into(),
            )
        })
    }

    /// The configuration arguments a run launches with — everything the
    /// process bakes in at spawn except the session to resume. Two requests
    /// with equal arguments (and cwd) are the same runtime.
    fn launch_args(request: &RunRequest) -> Vec<String> {
        let mut args: Vec<String> = [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Required by the CLI alongside `-p --output-format stream-json`.
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            // Route permission prompts to the stdio control channel so
            // `can_use_tool` (and AskUserQuestion in particular) reaches us.
            // Undocumented flag; validated live against 2.1.228.
            "--permission-prompt-tool",
            "stdio",
        ]
        .map(str::to_owned)
        .into();
        if let Some(model) = model_arg(request) {
            args.push("--model".into());
            args.push(model);
        }
        if let Some(effort) = to_effort(request.reasoning, request.model.as_deref()) {
            args.extend(["--effort".into(), effort.into()]);
        }
        let mode = request.access_mode();
        args.extend(["--permission-mode".into(), permission_mode(mode).into()]);
        if mode == AccessMode::FullAccess {
            args.push("--dangerously-skip-permissions".into());
        }
        for root in request.writable_roots() {
            args.extend(["--add-dir".into(), root.clone()]);
        }
        let mut settings = serde_json::Map::new();
        if option_is_on(&request.model_options, "fastMode") {
            settings.insert("fastMode".into(), Value::Bool(true));
        }
        if option_is_on(&request.model_options, "thinking") {
            settings.insert("alwaysThinkingEnabled".into(), Value::Bool(true));
        }
        if request.reasoning == Some(ReasoningLevel::Ultracode) {
            settings.insert("ultracode".into(), Value::Bool(true));
        }
        if !settings.is_empty() {
            args.push("--settings".into());
            args.push(Value::Object(settings).to_string());
        }
        args
    }

    fn build_command(&self, exe: &PathBuf, request: &RunRequest, thinking_display: bool) -> Command {
        let mut cmd = Command::new(exe);
        crate::compose_child_path(&mut cmd, exe);
        cmd.args(Self::launch_args(request));
        // Newer Claude models emit no readable thinking text unless a
        // summary is asked for (raw reasoning stays provider-private). A CLI
        // without the flag exits on it, so it rides only where supported.
        if thinking_display {
            cmd.args(["--thinking-display", "summarized"]);
        }
        if let Some(resume) = &request.resume {
            cmd.arg(format!("--resume={resume}"));
        }
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Share the complete initialize response between model and command discovery.
    /// No user message is written; the short-lived child is retired after initialize.
    async fn initialize(&self) -> Result<Value, HarnessError> {
        self.initialize
            .get(
                || self.model_context().map(|c| c.unwrap().key()),
                || self.probe_initialize(None),
            )
            .await
    }

    async fn discover_commands(
        &self,
        cwd: Option<&std::path::Path>,
    ) -> Result<Vec<SlashCommand>, HarnessError> {
        let response = match cwd {
            Some(cwd) => self.probe_initialize(Some(cwd)).await?,
            None => self.initialize().await?,
        };
        Ok(parse_initialize_commands(&response))
    }

    async fn probe_initialize(&self, cwd: Option<&std::path::Path>) -> Result<Value, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        // A project-less probe must not read the engine's cwd as a project.
        cmd.current_dir(cwd.map_or_else(crate::executable::neutral_cwd, |c| c.to_path_buf()));
        crate::compose_child_path(&mut cmd, &exe);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Mandatory with --print + stream-json output; without it the
            // CLI exits immediately with a usage error.
            "--verbose",
        ]);
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
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("claude child has no stdio".into()));
        };
        const PROBE_ID: &str = "godspeed-command-probe";
        let discovery = async {
            let request = serde_json::json!({
                "type": "control_request",
                "request_id": PROBE_ID,
                "request": { "subtype": "initialize" },
            });
            stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .map_err(HarnessError::Io)?;
            stdin.flush().await.map_err(HarnessError::Io)?;
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await.map_err(HarnessError::Io)? {
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("type").and_then(Value::as_str) != Some("control_response") {
                    continue;
                }
                let response = frame.get("response").cloned().unwrap_or(Value::Null);
                if response.get("request_id").and_then(Value::as_str) != Some(PROBE_ID) {
                    continue;
                }
                if response.get("subtype").and_then(Value::as_str) == Some("error") {
                    let msg = response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("initialize control request failed");
                    return Err(HarnessError::Protocol(msg.into()));
                }
                return Ok(response);
            }
            Err(HarnessError::Protocol(
                "claude exited before answering the initialize control request".into(),
            ))
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("Claude initialize timed out".into())),
        }
    }
}

/// Whether this CLI accepts `--thinking-display` (2.1.81 rejects it and
/// exits). The flag is hidden from `claude --help` (`hideHelp()`, 2.1.300),
/// so help text can't tell; the option parser can: given the flag and then
/// a sentinel unknown option, the CLI reports whichever it doesn't know
/// first and exits before any session, network or config write. One probe
/// per executable identity; an inconclusive probe (timeout, no output)
/// assumes support, as every current CLI has it, and is retried next run.
async fn supports_thinking_display(exe: &std::path::Path) -> bool {
    type Key = (PathBuf, Option<std::time::SystemTime>, u64);
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Key, bool>>> =
        std::sync::OnceLock::new();
    const SENTINEL: &str = "--godspeed-capability-probe";
    let key = std::fs::metadata(exe)
        .ok()
        .map(|m| (exe.to_path_buf(), m.modified().ok(), m.len()));
    let cache = CACHE.get_or_init(Default::default);
    if let Some(known) = key.as_ref().and_then(|k| {
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(k)
            .copied()
    }) {
        return known;
    }
    let mut cmd = Command::new(exe);
    crate::compose_child_path(&mut cmd, exe);
    cmd.args(["--thinking-display", "summarized", SENTINEL])
        .current_dir(crate::executable::neutral_cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
        Ok(Ok(output)) => output,
        _ => return true,
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let verdict = if text.contains("unknown option '--thinking-display'") {
        false
    } else if text.contains(SENTINEL) {
        true
    } else {
        return true;
    };
    if let Some(key) = key {
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, verdict);
    }
    verdict
}

/// `commands` out of an `initialize` control_response payload
/// (`response.response.commands`: name / description / argumentHint).
fn parse_initialize_commands(response: &Value) -> Vec<SlashCommand> {
    response
        .get("response")
        .and_then(|r| r.get("commands"))
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            Some(SlashCommand {
                name: name.to_owned(),
                description: c
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_hint: c
                    .get("argumentHint")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect()
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn display_name(&self) -> &str {
        "Claude Code"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    }
    fn installed(&self) -> bool {
        // The launch resolver, not bare discovery: a valid CLAUDE_CODE_EXECUTABLE
        // (or a test `executable`) must report installed, and an invalid one
        // must not — availability and launches share one resolution.
        self.resolve_executable().is_ok()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_executable().ok()
    }
    /// Done is the CLI's own terminal frame, for wake turns too.
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    /// The `interrupt` control request ends the turn and the CLI keeps
    /// reading stdin; background agents and shells run on.
    fn stops_turn_in_place(&self) -> bool {
        true
    }
    /// Same launch arguments and cwd. The CLI ignores the sandbox level, and
    /// option spellings that launch the same flags are the same runtime.
    fn same_runtime(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.cwd == next.cwd && Self::launch_args(live) == Self::launch_args(next)
    }
    /// Model, context window, effort, fast mode, thinking and ultracode all
    /// change on the live process (`set_model`, `apply_flag_settings`), and
    /// so does the permission mode (`set_permission_mode`) — except into or
    /// out of bypass, which the CLI only allows for a process launched with
    /// it. The directory and extra roots need a new one.
    fn reconfigures_in_place(&self, live: &RunRequest, next: &RunRequest) -> bool {
        let bypass = |r: &RunRequest| r.access_mode() == AccessMode::FullAccess;
        live.cwd == next.cwd
            && bypass(live) == bypass(next)
            && live.writable_roots() == next.writable_roots()
    }

    /// Credential and executable identity scopes both initialize and catalog caches.
    fn model_context(&self) -> Result<Option<crate::ModelContext>, HarnessError> {
        crate::model_context::context(self.id(), &self.resolve_executable()?, &[]).map(Some)
    }
    fn fallback_models(&self) -> Vec<Model> {
        catalog::configured_models()
    }
    async fn model_catalog(&self, force: bool) -> Result<crate::ModelCatalog, HarnessError> {
        self.model_context()?.unwrap().log();
        self.models_cache
            .get_with_timeout(
                force,
                Duration::from_secs(35),
                || self.model_context().map(|c| c.unwrap().key()),
                || async {
                    let response = self.initialize().await?;
                    catalog::with_discovered_models(catalog::configured_models(), &response)
                },
            )
            .await
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        match self.model_catalog(false).await {
            Ok(catalog) => Ok(catalog.models),
            Err(error) => {
                tracing::warn!(%error, source = "static", "Claude model discovery failed");
                Ok(self.fallback_models())
            }
        }
    }

    /// Slash commands from the CLI's `initialize` control-request handshake —
    /// the same channel the Claude Agent SDK's `query()` opens. The response
    /// carries every command with description + argument hint and involves no
    /// model turn (verified live, 2.1.228: the control_response is the first
    /// stdout line, well before any API traffic). Cached on success.
    async fn skills(
        &self,
        cwd: &std::path::Path,
    ) -> Result<Option<Vec<zeron_proto::invocation::Skill>>, HarnessError> {
        let (skills, commands) = tokio::try_join!(
            crate::skills::discover(self.id(), cwd),
            self.workspace_commands
                .get(cwd, self.discover_commands(Some(cwd)))
        )?;
        // The native advertised catalog controls availability (including plugin
        // enablement and skillOverrides). Shared Agent Skills can use file delivery.
        Ok(Some(
            skills
                .into_iter()
                .filter_map(|mut skill| {
                    if crate::skills::is_shared_skill(&skill.path) {
                        // Shared files are not Claude command definitions. A
                        // same-named built-in must not replace their identity.
                        Some(skill)
                    } else if zeron_proto::invocation::valid_skill_command_name(&skill.name)
                        && commands.iter().any(|command| command.name == skill.name)
                    {
                        skill.command = Some(zeron_proto::invocation::SkillCommand {
                            name: skill.name.clone(),
                            harness: self.id(),
                        });
                        Some(skill)
                    } else {
                        None
                    }
                })
                .collect(),
        ))
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.discover_commands(None).await
    }

    async fn commands_for(&self, cwd: &std::path::Path) -> Result<Vec<SlashCommand>, HarnessError> {
        self.workspace_commands
            .get(cwd, self.discover_commands(Some(cwd)))
            .await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.run_with_mode(request, controls, false).await
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
        request.set_access_mode(AccessMode::Supervised);
        if let Some(access) = &mut request.access {
            access.writable_roots.clear();
        }
        self.run_with_mode(request, controls, true).await
    }
}

impl ClaudeHarness {
    async fn run_with_mode(
        &self,
        request: RunRequest,
        controls: RunControls,
        title_only: bool,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let thinking_display = supports_thinking_display(&exe).await;
        let mut cmd = self.build_command(&exe, &request, thinking_display);
        if title_only {
            cmd.args([
                "--system-prompt",
                crate::TITLE_INSTRUCTIONS,
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                "{\"mcpServers\":{}}",
                "--setting-sources",
                "",
            ]);
        } else if let Some(mcp) = &request.mcp {
            // Godspeed's own server rides beside the user's configured servers
            // (no `--strict-mcp-config`): the CLI merges an inline JSON
            // config with settings-sourced ones.
            cmd.args(["--mcp-config", &mcp_config_arg(mcp)]);
        }
        let normalizer = if let Some(session_id) = &request.resume {
            let config = crate::model_context::root(
                "CLAUDE_CONFIG_DIR",
                crate::executable::home_or_current_dir().join(".claude"),
            );
            Normalizer::for_resume(&config, session_id).await
        } else {
            Normalizer::new()
        };
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
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "zeron_harness::claude", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<StdinMsg>();
        tokio::spawn(stdin_writer(stdin, stdin_rx));
        if !title_only {
            let _ = stdin_tx.send(StdinMsg::Line(wire::initialize_request_line(
                "zeron_initialize",
            )));
        }

        // The initial prompt as the first stdin user line (streaming-input
        // mode). Ultrathink rides every user message — steers included.
        // Staged image attachments are inlined as base64 image content blocks
        // ahead of the text (verified against the real CLI); their path refs
        // also ride the prompt text, so a skipped/unreadable file degrades to
        // the old-app behavior (the agent opens the path with its Read tool).
        let images = load_image_blocks(&request.attachments).await;
        let first_command = uuid::Uuid::new_v4().to_string();
        let first = wire::prompt_line(
            &apply_ultrathink(request.reasoning, &request.prompt),
            &first_command,
            &images,
        );
        let _ = stdin_tx.send(StdinMsg::Line(first));

        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            normalizer,
            title_only,
            child,
            stdout_lines: BufReader::new(stdout).lines(),
            first_command,
            stdin_tx,
            event_tx,
            controls,
            config: request,
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

enum StdinMsg {
    Line(String),
    /// Close stdin (end of steering input): the CLI finishes the current turn
    /// and exits, which ends the run stream at stdout EOF.
    Close,
}

/// Anthropic's API caps inline images at 5MB of raw bytes; larger files stay
/// path refs only.
const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Media type for an inline image block — extension first, magic bytes as the
/// fallback (pasted screenshots may carry odd names). Only the API-supported
/// inline types map; anything else (svg/bmp/tiff/…) returns `None`.
fn image_media_type(path: &std::path::Path, bytes: &[u8]) -> Option<&'static str> {
    let by_ext = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    by_ext.or(match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    })
}

/// Load `RunRequest::attachments` into inline image blocks, best-effort: an
/// unreadable, oversized, or unsupported file is skipped — its path ref still
/// rides the prompt text — never fatal to the run.
async fn load_image_blocks(paths: &[String]) -> Vec<wire::ImageBlock> {
    use base64::Engine as _;
    let mut blocks = Vec::new();
    for path in paths {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(target: "zeron_harness::claude", %path, error = %err, "attachment unreadable; path ref only");
                continue;
            }
        };
        if bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            tracing::debug!(target: "zeron_harness::claude", %path, "attachment over inline cap; path ref only");
            continue;
        }
        let Some(media_type) = image_media_type(std::path::Path::new(path), &bytes) else {
            tracing::debug!(target: "zeron_harness::claude", %path, "attachment not an inline-supported image; path ref only");
            continue;
        };
        blocks.push(wire::ImageBlock {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    blocks
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged.
async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<StdinMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            StdinMsg::Line(line) => {
                let write = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if let Err(e) = write.await {
                    tracing::debug!(target: "zeron_harness::claude", "stdin write failed (tolerated): {e}");
                    return;
                }
            }
            StdinMsg::Close => {
                let _ = stdin.shutdown().await;
                return;
            }
        }
    }
}

struct Session {
    normalizer: Normalizer,
    title_only: bool,
    child: Child,
    stdout_lines: tokio::io::Lines<BufReader<crate::process::ChildStdout>>,
    /// The uuid the opening prompt was written with.
    first_command: String,
    stdin_tx: mpsc::UnboundedSender<StdinMsg>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    /// The configuration the process runs with now: its launch request, then
    /// whatever a steer's `config` applied in place.
    config: RunRequest,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
}

/// The per-run event loop: one task multiplexing stdout frames, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        normalizer: mut norm,
        title_only,
        mut child,
        mut stdout_lines,
        first_command,
        stdin_tx,
        event_tx,
        controls,
        mut config,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        realtime: _,
        execution_lease: _execution_lease,
        request_input,
        mut steering,
        interrupt,
        turn,
    } = controls;
    let request_input = Arc::new(request_input);

    let mut pending_steers = std::collections::VecDeque::new();
    let mut reconfigures = 0u64;
    // Top-level tool calls in flight: a steer must not abort them (see
    // `wire::steer_message_line`).
    let mut open_tools = std::collections::HashSet::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut any_done = false;
    // A turn stop in flight (`TurnControl::stop_turn`): its request id, and
    // once the CLI has answered, how long a turn-ending `result` may still
    // take. The CLI answers a stop between turns without any result — the
    // deadline settles that shape so the host never waits on it.
    const STOP_SETTLE: Duration = Duration::from_secs(3);
    let mut stops_sent = 0u64;
    let mut stopping: Option<String> = None;
    let mut stop_settle_at: Option<tokio::time::Instant> = None;
    // A turn end held for steers when the stop came: that turn's end, sent
    // as stopped once the stop settles — never before. Released early, the
    // host started the replacement prompt while the stopped turn could still
    // report, and that late result settled the replacement.
    let mut stopped_end: Option<AgentEvent> = None;
    // When the stopped turn's one Done went out, and whether anything has
    // happened since — a message of ours written, or new output streamed. A
    // result in that window with neither can only be the stopped turn
    // reporting late, and is dropped. Anything else is reconciled like any
    // result (held for unconfirmed messages, settled by their fallback):
    // dropping on time alone lost a quick replacement's own end.
    let mut stop_done_at: Option<tokio::time::Instant> = None;
    let mut active_since_stop = false;
    let mut commands = Commands::default();
    commands.written(first_command);
    // A `result` that is a steer boundary rather than the turn's end (see
    // [`Commands`]). It is superseded — dropped — once the waiting message
    // starts its own turn, which ends with its own result; it is released
    // as the turn's end if that message is cancelled before it starts.
    let mut held_done: Option<AgentEvent> = None;
    // Last stdout activity, for the held result's settle fallback.
    let mut last_frame_at = tokio::time::Instant::now();
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // The runtime's process tree when its teardown began (see
    // [`crate::shutdown_agent`]).
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut torn_down: Vec<i32> = Vec::new();

    'main: loop {
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // The CLI is still producing: whatever it is doing is not
                    // the quiet end a held turn end's fallback waits for.
                    last_frame_at = tokio::time::Instant::now();
                    let frame = match wire::parse_frame(line) {
                        Ok(frame) => frame,
                        Err(e) => {
                            tracing::debug!(target: "zeron_harness::claude", "unparseable frame (skipped): {e}");
                            continue;
                        }
                    };
                    if let Frame::ControlResponse(response) = &frame {
                        if stopping.as_deref() == Some(response.response.request_id.as_str()) {
                            stop_settle_at = Some(tokio::time::Instant::now() + STOP_SETTLE);
                        }
                        continue;
                    }
                    if let Frame::System(system) = &frame
                        && system.subtype == "background_tasks_changed"
                    {
                        turn.set_background(system.tasks.as_ref().map_or(0, Vec::len));
                    }
                    if let Frame::ControlRequest(req) = frame {
                        if title_only {
                            if let Some(id) = &req.request.tool_use_id {
                                lock_denials(&norm.user_denials).insert(id.clone());
                            }
                            let line = control_response_line(&req.request_id, serde_json::json!({
                                "behavior": "deny", "message": "Tools are disabled for title generation"
                            }));
                            let _ = stdin_tx.send(StdinMsg::Line(line));
                        } else {
                            handle_control_request(
                                req,
                                &config,
                                &request_input,
                                &stdin_tx,
                                &norm.user_denials,
                            );
                        }
                        continue;
                    }
                    // Only the CLI confirms that a prompt joined its
                    // conversation — its lifecycle or its replay, whichever
                    // comes first. Writing stdin must not split ongoing text.
                    // A confirmation covers its steer and every earlier one:
                    // superseded steers are never replayed themselves. A
                    // message a turn took up (started, or replayed) also
                    // supersedes a result held for it: that result was only
                    // the boundary, and the message's turn ends with its own.
                    let (confirmed, taken_up) = match &frame {
                        Frame::CommandLifecycle(cmd) => {
                            let started = commands.lifecycle(&cmd.command_uuid, &cmd.state);
                            let confirmed = started || Commands::is_terminal(&cmd.state);
                            (confirmed.then(|| cmd.command_uuid.clone()), started)
                        }
                        Frame::User(user) if user.parent_tool_use_id.is_none() => {
                            let replayed = user.uuid.clone().filter(|id| pending_steers.contains(id));
                            let taken_up = replayed.is_some();
                            (replayed, taken_up)
                        }
                        _ => (None, false),
                    };
                    if taken_up {
                        held_done = None;
                        active_since_stop = true;
                    }
                    if let Some(at) = confirmed
                        .and_then(|id| pending_steers.iter().position(|p| *p == id))
                    {
                        for _ in 0..=at {
                            pending_steers.pop_front();
                            let (prev, next) = norm.rotate_for_steer();
                            if event_tx.send(Ok(AgentEvent::Steered {
                                assistant_message_id: Some(prev), next_assistant_message_id: Some(next),
                            })).await.is_err() { break 'main; }
                        }
                    }
                    // Every waiting message was cancelled before it started:
                    // the held result was the turn's real end after all.
                    if matches!(frame, Frame::CommandLifecycle(_))
                        && !commands.waiting()
                        && let Some(done) = held_done.take()
                    {
                        if event_tx.send(Ok(done)).await.is_err() {
                            break 'main;
                        }
                        any_done = true;
                    }
                    // The stopped turn reporting late: a result with nothing
                    // since the stop's Done. Judged on the frame, before it
                    // expands — its own Usage is not new activity — and
                    // dropped whole, usage included.
                    if matches!(frame, Frame::Result(_))
                        && !interrupted
                        && stopping.is_none()
                        && !active_since_stop
                        && stop_done_at.is_some_and(|at| at.elapsed() < STOP_SETTLE)
                    {
                        tracing::debug!(
                            target: "zeron_harness::claude",
                            "late result of a stopped turn dropped"
                        );
                        continue;
                    }
                    for ev in norm.normalize(frame, interrupted || stopping.is_some()) {
                        match &ev {
                            AgentEvent::ToolCall { id, .. } => {
                                open_tools.insert(id.clone());
                            }
                            AgentEvent::ToolResult { id, .. } => {
                                open_tools.remove(id);
                            }
                            AgentEvent::Done { .. } => open_tools.clear(),
                            _ => {}
                        }
                        let is_done = matches!(ev, AgentEvent::Done { .. });
                        // A `now` steer ends the turn it interrupts with a
                        // result frame, and a message queued behind a turn
                        // that just ended starts another: either way that
                        // result is a steer boundary, not the end of the turn.
                        let boundary = if commands.reported {
                            commands.waiting()
                        } else {
                            !pending_steers.is_empty()
                        };
                        // New content is a new turn's: its result is its own.
                        if stop_done_at.is_some()
                            && match &ev {
                                AgentEvent::TextDelta { text }
                                | AgentEvent::ReasoningDelta { text } => !text.is_empty(),
                                AgentEvent::ToolCall { .. }
                                | AgentEvent::ToolResult { .. }
                                | AgentEvent::InputRequested { .. }
                                | AgentEvent::Subagent { .. } => true,
                                _ => false,
                            }
                        {
                            active_since_stop = true;
                        }
                        if is_done && !interrupted && stopping.is_none() && boundary {
                            held_done = Some(ev);
                            continue;
                        }
                        if is_done {
                            held_done = None;
                            // The stopped turn has ended; the runtime lives on.
                            if stopping.take().is_some() {
                                stopped_end = None;
                                stop_done_at = Some(tokio::time::Instant::now());
                                active_since_stop = false;
                            }
                            stop_settle_at = None;
                        }
                        if event_tx.send(Ok(ev)).await.is_err() {
                            break 'main; // consumer gone — reap below
                        }
                        if is_done {
                            any_done = true;
                            if interrupted {
                                done_after_interrupt = true;
                                break 'main;
                            }
                        }
                    }
                }
                Ok(None) => break 'main, // stdout EOF: the CLI exited
                Err(e) => {
                    let _ = event_tx.send(Err(HarnessError::Io(e))).await;
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    // A message of ours now owns whatever result comes next.
                    active_since_stop = true;
                    // A changed configuration applies before the prompt, on
                    // the same stdin the CLI reads in order.
                    if let Some(next) = msg.config {
                        reconfigures += 1;
                        if model_arg(&next) != model_arg(&config) {
                            let line = wire::set_model_request_line(
                                &format!("zeron_model_{reconfigures}"),
                                model_arg(&next).as_deref(),
                            );
                            if stdin_tx.send(StdinMsg::Line(line)).is_err() { break 'main; }
                        }
                        if flag_settings(&next) != flag_settings(&config) {
                            let line = wire::apply_flag_settings_line(
                                &format!("zeron_settings_{reconfigures}"),
                                flag_settings(&next),
                            );
                            if stdin_tx.send(StdinMsg::Line(line)).is_err() { break 'main; }
                        }
                        if next.access_mode() != config.access_mode() {
                            let line = wire::set_permission_mode_line(
                                &format!("zeron_mode_{reconfigures}"),
                                permission_mode(next.access_mode()),
                            );
                            if stdin_tx.send(StdinMsg::Line(line)).is_err() { break 'main; }
                        }
                        config = *next;
                    }
                    let id = uuid::Uuid::new_v4().to_string();
                    let images = load_image_blocks(&msg.attachments).await;
                    let line = wire::steer_message_line_with_images(
                        &apply_ultrathink(config.reasoning, &msg.prompt),
                        &id,
                        open_tools.is_empty(),
                        &images,
                    );
                    pending_steers.push_back(id.clone());
                    commands.written(id);
                    if stdin_tx.send(StdinMsg::Line(line)).is_err() { break 'main; }
                }
                None => {
                    // Mailbox closed: end the input so the run can finish
                    // after the current turn.
                    steering_open = false;
                    let _ = stdin_tx.send(StdinMsg::Close);
                }
            },

            // End the in-flight turn, not the session: the CLI stays up, so
            // its background agents and shells keep running. Steers written
            // before the stop are cancelled with it (`cancel_queued`) — a
            // stopped turn must not continue into them.
            _ = turn.stop_requested(), if !interrupted => {
                stops_sent += 1;
                let id = format!("zeron_stop_{stops_sent}");
                pending_steers.clear();
                commands.cancel_waiting();
                stopping = Some(id.clone());
                stop_settle_at = None;
                stop_done_at = None;
                if stdin_tx.send(StdinMsg::Line(wire::stop_turn_request_line(&id))).is_err() {
                    break 'main;
                }
                // A turn end held for steer replays is that turn's real end:
                // its steers were just cancelled, so it ends stopped — once
                // the stop settles. A turn the CLI had already begun for
                // those steers reports first, and that report is the end.
                if let Some(mut done) = held_done.take() {
                    if let AgentEvent::Done { status, .. } = &mut done {
                        *status = DoneStatus::Interrupted;
                    }
                    stopped_end = Some(done);
                }
            },

            // The CLI answered the stop and no turn ended: it was between
            // turns. Settle the stop so the host sees the turn over.
            _ = tokio::time::sleep_until(stop_settle_at.unwrap_or_else(tokio::time::Instant::now)),
                if stop_settle_at.is_some() => {
                stop_settle_at = None;
                if stopping.take().is_some() {
                    let done = stopped_end.take().unwrap_or(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: norm.session_id.clone(),
                    });
                    if event_tx.send(Ok(done)).await.is_err() {
                        break 'main;
                    }
                    any_done = true;
                    stop_done_at = Some(tokio::time::Instant::now());
                    active_since_stop = false;
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
                let _ = stdin_tx.send(StdinMsg::Line(wire::interrupt_request_line("int_1")));
                // Escalate if the CLI doesn't wind down within the grace
                // periods: SIGTERM (kills bash trees, runs SessionEnd hooks),
                // then SIGKILL. Aborted once the child is reaped.
                if let Some(pid) = crate::process::signal_target(&child) {
                    escalation = Some(tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        send_signal(&pid, Signal::Term);
                        tokio::time::sleep(kill_grace).await;
                        send_signal(&pid, Signal::Kill);
                    }));
                }
            },

            _ = tokio::time::sleep_until(
                last_frame_at + commands.held_settle().unwrap_or_default()
            ), if held_done.is_some() && commands.held_settle().is_some() => {
                // The steers were absorbed into the turn that just ended.
                commands.cancel_waiting();
                while pending_steers.pop_front().is_some() {
                    let (prev, next) = norm.rotate_for_steer();
                    if event_tx.send(Ok(AgentEvent::Steered {
                        assistant_message_id: Some(prev), next_assistant_message_id: Some(next),
                    })).await.is_err() { break 'main; }
                }
                let done = held_done.take().expect("guarded by if");
                if event_tx.send(Ok(done)).await.is_err() {
                    break 'main;
                }
                any_done = true;
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // A turn end still held when the CLI exited is the run's real end.
    if let Some(done) = held_done.take()
        && !event_tx.is_closed()
        && event_tx.send(Ok(done)).await.is_ok()
    {
        any_done = true;
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
                    session_id: norm.session_id.clone(),
                }))
                .await;
        } else if !interrupted && !any_done {
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("claude", status, &stderr_tail)),
                    session_id: norm.session_id.clone(),
                }))
                .await;
        }
    }

    crate::shutdown_agent(&mut child, torn_down, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// How far the CLI has taken each stdin user message we wrote, by the uuid
/// it was written with. A `result` frame ends the TURN only when no message
/// is still waiting to be taken up. A waiting one makes the result a steer
/// boundary — a `now` steer aborting the turn it interrupts, or a message
/// queued behind a turn that just finished — and that message's own turn
/// ends with its own result. Deciding this from the CLI's own lifecycle
/// rather than from quiet time: a held result released by a quiet timer
/// marked the steered turn done mid-way through any long tool call.
#[derive(Default)]
struct Commands {
    /// Uuid → how far it got. Ended commands are dropped.
    open: std::collections::HashMap<String, CommandState>,
    /// The CLI reports `command_lifecycle`. Older CLIs never do: their only
    /// confirmation is the replay, and the settle fallback covers the rest.
    reported: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommandState {
    /// On stdin; the CLI has not acknowledged it yet.
    Written,
    /// The CLI read it and will start or cancel it.
    Queued,
    /// A turn took it up.
    Started,
}

impl Commands {
    /// Without lifecycle reports, how long a held result waits in silence
    /// for the replay that supersedes it.
    const UNREPORTED_SETTLE: Duration = Duration::from_secs(5);
    /// A lifecycle-reporting CLI acknowledges a message as soon as it reads
    /// it. A hold resting only on unacknowledged messages is a safety net
    /// against a message the CLI never reports, not a timing guess.
    const UNACKNOWLEDGED_SETTLE: Duration = Duration::from_secs(30);

    fn written(&mut self, id: String) {
        self.open.insert(id, CommandState::Written);
    }

    fn is_terminal(state: &str) -> bool {
        matches!(state, "completed" | "cancelled" | "canceled" | "failed")
    }

    /// Apply one `command_lifecycle` frame. True when a turn took the
    /// command up (`started`).
    fn lifecycle(&mut self, id: &str, state: &str) -> bool {
        self.reported = true;
        if Self::is_terminal(state) {
            self.open.remove(id);
            return false;
        }
        let next = match state {
            "started" => CommandState::Started,
            "queued" => CommandState::Queued,
            _ => return false,
        };
        if let Some(current) = self.open.get_mut(id)
            && *current != CommandState::Started
        {
            *current = next;
        }
        next == CommandState::Started
    }

    /// A message is written or queued but no turn has taken it up.
    fn waiting(&self) -> bool {
        self.open.values().any(|s| *s != CommandState::Started)
    }

    /// A stop's `cancel_queued` drops every message not yet taken up.
    fn cancel_waiting(&mut self) {
        self.open.retain(|_, s| *s == CommandState::Started);
    }

    /// The quiet time after which a held result is released as the turn's
    /// end; `None` = only the CLI's own lifecycle settles it.
    fn held_settle(&self) -> Option<Duration> {
        if !self.reported {
            Some(Self::UNREPORTED_SETTLE)
        } else if self.open.values().any(|s| *s == CommandState::Queued) {
            // A queued message always starts or is cancelled.
            None
        } else {
            Some(Self::UNACKNOWLEDGED_SETTLE)
        }
    }
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// Serve one `can_use_tool` control request. The CLI already applied the
/// run's permission mode, so whatever reaches us needs a decision:
/// `AskUserQuestion` surfaces its questions; every other tool becomes an
/// approval (allowed outright only under full access, as a belt to the
/// CLI's own bypass). Both wait in a subtask so the frame loop keeps flowing
/// — the CLI blocks the tool until SOME response arrives.
type Denials = Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

fn lock_denials(denials: &Denials) -> std::sync::MutexGuard<'_, std::collections::HashSet<String>> {
    denials
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn handle_control_request(
    req: ControlRequestFrame,
    config: &RunRequest,
    request_input: &Arc<RequestInputFn>,
    stdin_tx: &mpsc::UnboundedSender<StdinMsg>,
    user_denials: &Denials,
) {
    if req.request.subtype == "elicitation" {
        // The CLI blocks the MCP call until an answer arrives; unanswered,
        // the tool hung until the server gave up.
        let request_input = Arc::clone(request_input);
        let stdin_tx = stdin_tx.clone();
        tokio::spawn(async move {
            let result = answer_elicitation(&req.request, request_input.as_ref()).await;
            let line = control_response_line(&req.request_id, result);
            let _ = stdin_tx.send(StdinMsg::Line(line));
        });
        return;
    }
    if req.request.subtype != "can_use_tool" {
        tracing::debug!(
            target: "zeron_harness::claude",
            "unhandled control_request subtype: {}", req.request.subtype
        );
        return;
    }
    if req.request.tool_name != "AskUserQuestion" {
        if config.access_mode() == AccessMode::FullAccess {
            let line = control_response_line(&req.request_id, allow_response(req.request.input));
            let _ = stdin_tx.send(StdinMsg::Line(line));
            return;
        }
        let approval = approval_request(&req.request, &config.cwd);
        let request_input = Arc::clone(request_input);
        let stdin_tx = stdin_tx.clone();
        let user_denials = Arc::clone(user_denials);
        tokio::spawn(async move {
            let decision = crate::approval::ask(request_input.as_ref(), approval).await;
            if !decision.approves()
                && let Some(id) = &req.request.tool_use_id
            {
                lock_denials(&user_denials).insert(id.clone());
            }
            let response = permission_result(&req.request, decision);
            let line = control_response_line(&req.request_id, response);
            let _ = stdin_tx.send(StdinMsg::Line(line));
        });
        return;
    }
    let request_input = Arc::clone(request_input);
    let stdin_tx = stdin_tx.clone();
    tokio::spawn(async move {
        let request_id = req.request_id;
        let input = req.request.input;
        let questions = parse_questions(&input);
        // The engine's input bridge is the SOLE emitter of
        // `InputRequested`/`InputResolved`: it mints the request id, parks the
        // resolver for `respond_input`, and surfaces both events. Emitting our
        // own copy here (keyed by Claude's control-request id) folded a SECOND
        // input part into the doc whose id no resolver knew — the QuestionPanel
        // answered that unanswerable twin and the run never resumed.
        //
        // A dropped sender (caller went away) degrades to empty answers so the
        // agent is unblocked rather than wedged.
        let answers = (request_input)(questions.clone()).await.unwrap_or_default();
        let updated = updated_input_with_answers(&input, &questions, &answers);
        let line = control_response_line(&request_id, allow_response(updated));
        let _ = stdin_tx.send(StdinMsg::Line(line));
    });
}

/// Answer an MCP `elicitation` with the user's input. A form that asks for
/// nothing (an MCP server requesting access, say) is an approval; a form
/// with fields becomes questions, one per field; a URL is an approval whose
/// card shows the link. Anything the user leaves unanswered declines, and a
/// run that ends first cancels — never a silent accept.
async fn answer_elicitation(
    req: &wire::ControlRequestBody,
    request_input: &crate::approval::RequestInputFn,
) -> Value {
    let server = req
        .display_name
        .as_deref()
        .or(req.mcp_server_name.as_deref())
        .unwrap_or("An MCP server");
    let message = req.message.as_deref().unwrap_or_default().trim();
    let schema = req.requested_schema.clone().unwrap_or_default();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .filter(|p| !p.is_empty());
    let Some(properties) = properties.filter(|_| req.mode.as_deref() != Some("url")) else {
        let url = req.url.as_deref().filter(|_| req.mode.as_deref() == Some("url"));
        let title = req
            .title
            .clone()
            .unwrap_or_else(|| match url {
                Some(_) => format!("{server} asks you to open a link"),
                None => format!("{server} asks for your approval"),
            });
        let mut approval = ApprovalRequest::new(ApprovalKind::McpTool, title);
        let reason = [Some(message), req.description.as_deref(), url]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        approval.reason = (!reason.is_empty()).then_some(reason);
        approval = approval.with_decisions(&[
            ApprovalDecision::Approve,
            ApprovalDecision::Decline,
            ApprovalDecision::Cancel,
        ]);
        return match crate::approval::ask(request_input, approval).await {
            ApprovalDecision::Cancel => serde_json::json!({ "action": "cancel" }),
            decision if decision.approves() => match url {
                Some(_) => serde_json::json!({ "action": "accept" }),
                None => serde_json::json!({ "action": "accept", "content": {} }),
            },
            _ => serde_json::json!({ "action": "decline" }),
        };
    };
    // The server's own field order when the CLI reports it.
    let mut names: Vec<&String> = req
        .property_order
        .iter()
        .flatten()
        .filter(|name| properties.contains_key(*name))
        .collect();
    let rest: Vec<&String> = properties.keys().filter(|k| !names.contains(k)).collect();
    names.extend(rest);
    let fields: Vec<(String, &Value, UserInputQuestion)> = names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let field = &properties[name];
            let text = |key: &str| field.get(key).and_then(Value::as_str).map(str::to_owned);
            let (options, multi_select) = elicitation_choices(field);
            let mut question = text("description").unwrap_or_else(|| name.clone());
            if index == 0 && !message.is_empty() {
                question = format!("{message}\n\n{question}");
            }
            let q = UserInputQuestion {
                id: uuid::Uuid::new_v4().to_string(),
                header: text("title").unwrap_or_else(|| server.to_owned()),
                question,
                options: options.iter().map(|(label, _)| label.clone()).collect(),
                multi_select,
                prefill: field.get("default").map(|d| match d {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }),
                multiline: false,
                approval: None,
            };
            (name.clone(), field, q)
        })
        .collect();
    let questions: Vec<UserInputQuestion> = fields.iter().map(|(_, _, q)| q.clone()).collect();
    let Ok(answers) = request_input(questions).await else {
        return serde_json::json!({ "action": "cancel" });
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut content = serde_json::Map::new();
    for (name, field, q) in &fields {
        let labels: Vec<String> = answers
            .iter()
            .find(|a| a.question_id == q.id)
            .map(|a| a.labels.iter().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
        let Some(value) = elicitation_value(field, &labels) else {
            if required.contains(&name.as_str()) {
                return serde_json::json!({ "action": "decline" });
            }
            continue;
        };
        content.insert(name.clone(), value);
    }
    serde_json::json!({ "action": "accept", "content": content })
}

/// A field's choices as (label, value) pairs, and whether several may be
/// picked: `enum` (with `enumNames`), `oneOf`/`anyOf` consts, booleans, and
/// arrays of those.
fn elicitation_choices(field: &Value) -> (Vec<(String, Value)>, bool) {
    let consts = |schema: &Value| -> Vec<(String, Value)> {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let names = schema.get("enumNames").and_then(Value::as_array);
            return values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let label = names
                        .and_then(|n| n.get(i))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
                    (label, v.clone())
                })
                .collect();
        }
        ["oneOf", "anyOf"]
            .iter()
            .find_map(|k| schema.get(*k).and_then(Value::as_array))
            .map(|variants| {
                variants
                    .iter()
                    .filter_map(|v| {
                        let value = v.get("const")?.clone();
                        let label = v
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.as_str().map_or_else(|| value.to_string(), str::to_owned));
                        Some((label, value))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    match field.get("type").and_then(Value::as_str) {
        Some("boolean") => (
            vec![("Yes".into(), Value::Bool(true)), ("No".into(), Value::Bool(false))],
            false,
        ),
        Some("array") => (consts(field.get("items").unwrap_or(&Value::Null)), true),
        _ => (consts(field), false),
    }
}

/// The value a field's answer labels stand for, typed per the schema;
/// `None` when unanswered or not a valid value.
fn elicitation_value(field: &Value, labels: &[String]) -> Option<Value> {
    let (choices, multi) = elicitation_choices(field);
    let pick = |label: &String| {
        choices
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.clone())
    };
    if multi {
        let values: Vec<Value> = labels.iter().filter_map(pick).collect();
        return (!values.is_empty()).then_some(Value::Array(values));
    }
    let label = labels.first()?;
    if !choices.is_empty() {
        return pick(label);
    }
    match field.get("type").and_then(Value::as_str) {
        Some("integer") => label.parse::<i64>().ok().map(Value::from),
        Some("number") => label
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        _ => Some(Value::String(label.clone())),
    }
}

/// `--permission-mode` for an access mode. `default` is the CLI's own
/// spelling of "ask" (`manual` is a newer alias for it).
fn permission_mode(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::Supervised => "default",
        AccessMode::AutoAcceptEdits => "acceptEdits",
        AccessMode::Auto => "auto",
        AccessMode::FullAccess => "bypassPermissions",
    }
}

/// Describe a `can_use_tool` request for the approval card: the exact Bash
/// command, the edited path with a diff preview, or the tool and its target.
fn approval_request(req: &wire::ControlRequestBody, cwd: &str) -> ApprovalRequest {
    let input = &req.input;
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    let tool = req.tool_name.as_str();
    let (kind, title) = match tool {
        "Bash" => (
            ApprovalKind::Command,
            text("description").unwrap_or_else(|| "Run a command".into()),
        ),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let path = text("file_path")
                .or_else(|| text("notebook_path"))
                .unwrap_or_default();
            (
                ApprovalKind::FileChange,
                format!("Edit {}", display_path(&path, cwd)),
            )
        }
        "Read" | "Glob" | "Grep" | "LS" => (ApprovalKind::FileRead, format!("Use {tool}")),
        "WebFetch" => (
            ApprovalKind::Network,
            format!("Fetch {}", text("url").unwrap_or_default()),
        ),
        "WebSearch" => (
            ApprovalKind::Network,
            format!("Search the web for {}", text("query").unwrap_or_default()),
        ),
        _ if tool.starts_with("mcp__") => {
            let mut parts = tool.splitn(3, "__").skip(1);
            let server = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default();
            (ApprovalKind::McpTool, format!("Use {name} ({server})"))
        }
        _ => (ApprovalKind::Other, format!("Use {tool}")),
    };
    let mut approval = ApprovalRequest::new(kind, req.title.clone().unwrap_or(title));
    approval.reason = req
        .decision_reason
        .clone()
        .or_else(|| req.description.clone());
    approval.cwd = (!cwd.is_empty()).then(|| cwd.to_owned());
    match tool {
        "Bash" => approval.command = text("command"),
        "Edit" => {
            if let Some(path) = text("file_path") {
                approval.diff = Some(crate::approval::unified_diff(
                    &display_path(&path, cwd),
                    Some(&text("old_string").unwrap_or_default()),
                    &text("new_string").unwrap_or_default(),
                ));
                approval.paths.push(path);
            }
        }
        "MultiEdit" => {
            if let Some(path) = text("file_path") {
                let shown = display_path(&path, cwd);
                let diff: String = input
                    .get("edits")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(|edit| {
                        let field = |k: &str| edit.get(k).and_then(Value::as_str).unwrap_or("");
                        crate::approval::unified_diff(
                            &shown,
                            Some(field("old_string")),
                            field("new_string"),
                        )
                    })
                    .collect();
                approval.diff = (!diff.is_empty()).then_some(diff);
                approval.paths.push(path);
            }
        }
        "Write" => {
            if let Some(path) = text("file_path") {
                let full = std::path::Path::new(cwd).join(&path);
                let old = std::fs::metadata(&full)
                    .ok()
                    .filter(|m| m.is_file() && m.len() <= 1024 * 1024)
                    .and_then(|_| std::fs::read_to_string(&full).ok());
                approval.diff = Some(crate::approval::unified_diff(
                    &display_path(&path, cwd),
                    old.as_deref(),
                    &text("content").unwrap_or_default(),
                ));
                approval.paths.push(path);
            }
        }
        "NotebookEdit" => approval.paths.extend(text("notebook_path")),
        "Read" | "LS" => approval
            .paths
            .extend(text("file_path").or_else(|| text("path"))),
        "Glob" | "Grep" => approval.command = text("pattern"),
        _ => {}
    }
    if let Some(blocked) = &req.blocked_path
        && !approval.paths.contains(blocked)
    {
        approval.paths.push(blocked.clone());
    }
    if req.suppress_always_allow_rule {
        approval =
            approval.with_decisions(&[ApprovalDecision::Approve, ApprovalDecision::Decline]);
    }
    approval
}

fn display_path(path: &str, cwd: &str) -> String {
    std::path::Path::new(path)
        .strip_prefix(cwd)
        .ok()
        .filter(|_| !cwd.is_empty())
        .map_or_else(|| path.to_owned(), |p| p.display().to_string())
}

/// The `PermissionResult` for a decision. "This session" echoes the CLI's
/// own suggestions scoped to the session (in memory, never a settings
/// file); without suggestions it allows this tool — this exact command, for
/// Bash — for the session.
fn permission_result(req: &wire::ControlRequestBody, decision: ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => {
            allow_response(req.input.clone())
        }
        ApprovalDecision::ApproveForSession => {
            let mut response = allow_response(req.input.clone());
            response["updatedPermissions"] = Value::Array(session_permissions(req));
            response
        }
        ApprovalDecision::Decline | ApprovalDecision::DeclineAlways => serde_json::json!({
            "behavior": "deny",
            "message": "The user declined this action.",
        }),
        ApprovalDecision::Cancel => serde_json::json!({
            "behavior": "deny",
            "message": "The user declined this action and stopped the turn.",
            "interrupt": true,
        }),
    }
}

fn session_permissions(req: &wire::ControlRequestBody) -> Vec<Value> {
    let suggestions: Vec<Value> = req
        .permission_suggestions
        .iter()
        .cloned()
        .map(|mut update| {
            update["destination"] = "session".into();
            update
        })
        .collect();
    if !suggestions.is_empty() {
        return suggestions;
    }
    let mut rule = serde_json::json!({ "toolName": req.tool_name });
    if req.tool_name == "Bash"
        && let Some(command) = req.input.get("command").and_then(Value::as_str)
    {
        rule["ruleContent"] = command.into();
    }
    vec![serde_json::json!({
        "type": "addRules",
        "rules": [rule],
        "behavior": "allow",
        "destination": "session",
    })]
}

/// A question's text as the tool input wrote it: the key its answer goes
/// back under.
fn question_text(q: &Value) -> &str {
    ["question", "prompt"]
        .iter()
        .find_map(|k| q.get(*k).and_then(Value::as_str))
        .unwrap_or("")
}

/// The option descriptions and `preview`s (a draft, a code snippet, a
/// mockup the user is asked to approve) appended to the question text: the
/// wire type carries option labels only, and a card showing "Post / Skip"
/// without the draft asks the user to approve text they cannot see.
fn question_with_option_details(question: &str, options: &[Value]) -> String {
    const PREVIEW_LINES: usize = 40;
    const PREVIEW_CHARS: usize = 4000;
    let mut details = Vec::new();
    for option in options {
        let text = |key: &str| {
            option
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let Some(label) = text("label").or_else(|| text("value")) else {
            continue;
        };
        let (description, preview) = (text("description"), text("preview"));
        if description.is_none() && preview.is_none() {
            continue;
        }
        let mut entry = match description {
            Some(description) => format!("• {label}: {description}"),
            None => format!("• {label}"),
        };
        if let Some(preview) = preview {
            let mut truncated = preview.lines().count() > PREVIEW_LINES;
            let mut shown: String = preview
                .lines()
                .take(PREVIEW_LINES)
                .map(|line| format!("\n    {line}"))
                .collect();
            if shown.chars().count() > PREVIEW_CHARS {
                shown = shown.chars().take(PREVIEW_CHARS).collect();
                truncated = true;
            }
            if truncated {
                shown.push_str("\n    …");
            }
            entry.push_str(&shown);
        }
        details.push(entry);
    }
    if details.is_empty() {
        question.to_owned()
    } else {
        format!("{question}\n\n{}", details.join("\n"))
    }
}

/// Parse Claude's `AskUserQuestion` tool input into [`UserInputQuestion`]s
/// (tolerant of `header`/`title`, `question`/`prompt`, string or object
/// options). Options carry their labels; descriptions and previews ride the
/// question text ([`question_with_option_details`]).
fn parse_questions(input: &Value) -> Vec<UserInputQuestion> {
    let raw = input.get("questions").and_then(Value::as_array);
    raw.map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|q| {
            let field =
                |keys: [&str; 2]| keys.iter().find_map(|k| q.get(*k).and_then(Value::as_str));
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            UserInputQuestion {
                id: uuid::Uuid::new_v4().to_string(),
                header: field(["header", "title"]).unwrap_or("Question").into(),
                question: question_with_option_details(question_text(q), options),
                prefill: None,
                multiline: false,
                multi_select: ["multiSelect", "multi_select"]
                    .iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_bool))
                    .unwrap_or(false),
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
                    approval: None,
            }
        })
        .collect()
}

/// Merge the user's answers back into the tool input, keyed by the input's
/// own question text (single-select ⇒ a string, multi-select ⇒ an array),
/// as the tool expects. `questions` are [`parse_questions`]'s, in order.
fn updated_input_with_answers(
    input: &Value,
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> Value {
    let mut updated = match input {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let raw = input
        .get("questions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut by_question = serde_json::Map::new();
    for (index, q) in questions.iter().enumerate() {
        let labels: Vec<String> = answers
            .iter()
            .find(|a| a.question_id == q.id)
            .map(|a| a.labels.clone())
            .unwrap_or_default();
        let value = if q.multi_select {
            Value::Array(labels.into_iter().map(Value::String).collect())
        } else {
            Value::String(labels.into_iter().next().unwrap_or_default())
        };
        let key = raw.get(index).map_or(q.question.as_str(), question_text);
        by_question.insert(key.to_owned(), value);
    }
    updated.insert("answers".into(), Value::Object(by_question));
    Value::Object(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_questions_tolerantly() {
        let input = json!({
            "questions": [
                {
                    "header": "Choice",
                    "question": "Pick one",
                    "options": ["A", {"label": "B", "description": "second"}],
                    "multiSelect": false
                },
                { "title": "Alt", "prompt": "Pick many", "multi_select": true }
            ]
        });
        let qs = parse_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header, "Choice");
        assert_eq!(qs[0].options, vec!["A".to_string(), "B".to_string()]);
        assert!(!qs[0].multi_select);
        assert_eq!(qs[1].header, "Alt");
        assert_eq!(qs[1].question, "Pick many");
        assert!(qs[1].multi_select);
    }

    /// zeronsh/zeron#477: a CLI that predates `--thinking-display` exits on
    /// it, failing every run. Probed against the parser's own error, since
    /// the flag is hidden from `--help` on current CLIs.
    #[cfg(unix)]
    #[tokio::test]
    async fn thinking_display_rides_only_clis_that_accept_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // Commander reports the first option it doesn't know.
        let old = script(
            "claude-2.1.81",
            r#"echo "error: unknown option '$1'" >&2; exit 1"#,
        );
        let new = script(
            "claude-2.1.300",
            r#"echo "error: unknown option '$3'" >&2; exit 1"#,
        );
        let silent = script("claude-silent", "exit 1");
        assert!(!supports_thinking_display(&old).await);
        assert!(supports_thinking_display(&new).await);
        assert!(supports_thinking_display(&silent).await, "inconclusive keeps the flag");

        let harness = ClaudeHarness::new();
        let request = RunRequest {
            mcp: None,
            prompt: "hi".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: String::new(),
            sandbox: zeron_proto::SandboxLevel::DangerFullAccess,
            auto_approve: true,
            attachments: Vec::new(),
            worktree: None,
            resume: None,
            access: None,
        };
        let args = |cmd: &Command| -> Vec<String> {
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert!(
            !args(&harness.build_command(&old, &request, false))
                .contains(&"--thinking-display".to_owned())
        );
        let with = args(&harness.build_command(&new, &request, true));
        let at = with.iter().position(|a| a == "--thinking-display").unwrap();
        assert_eq!(with[at + 1], "summarized");
    }

    /// pingdotgg/t3code#14237: an option's `preview` (the draft the user is
    /// asked to post, a code snippet) and its description reach the card,
    /// and the answer still goes back under the tool input's own question.
    #[test]
    fn option_descriptions_and_previews_reach_the_question() {
        let input = json!({"questions": [{
            "header": "Review",
            "question": "Post this comment?",
            "options": [
                {"label": "Post", "description": "Publish as written", "preview": "Looks good,\nbut rename `foo`."},
                {"label": "Skip"},
            ],
        }]});
        let qs = parse_questions(&input);
        assert_eq!(qs[0].options, ["Post", "Skip"]);
        assert_eq!(
            qs[0].question,
            "Post this comment?\n\n• Post: Publish as written\n    Looks good,\n    but rename `foo`."
        );
        let answers = vec![UserInputAnswer {
            question_id: qs[0].id.clone(),
            labels: vec!["Post".into()],
        }];
        let updated = updated_input_with_answers(&input, &qs, &answers);
        assert_eq!(updated["answers"], json!({"Post this comment?": "Post"}));
        // Long previews are capped, visibly.
        let long = (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let text = question_with_option_details("Q", &[json!({"label": "A", "preview": long})]);
        assert!(text.contains("line 39") && !text.contains("line 40"));
        assert!(text.ends_with("\n    …"));
        // Nothing to add: the question is unchanged.
        assert_eq!(question_with_option_details("Q", &[json!("A"), json!({"label": "B"})]), "Q");
    }

    /// An answering bridge that records the questions and replies with
    /// `reply(questions)`; `None` drops the sender (the run went away).
    fn bridge(
        reply: impl Fn(&[UserInputQuestion]) -> Option<Vec<UserInputAnswer>> + Send + Sync + 'static,
    ) -> Box<crate::approval::RequestInputFn> {
        Box::new(move |questions| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if let Some(answers) = reply(&questions) {
                let _ = tx.send(answers);
            }
            rx
        })
    }

    fn elicitation(raw: Value) -> wire::ControlRequestBody {
        serde_json::from_value(raw).unwrap()
    }

    /// pingdotgg/t3code#14878: an MCP server's `elicitation` was never
    /// answered, so the tool hung. An approval-only form becomes an approval
    /// card; a form with fields becomes questions whose answers go back
    /// typed per the schema; nothing is accepted without the user.
    #[tokio::test]
    async fn mcp_elicitations_reach_the_user_and_answer_the_server() {
        let access = elicitation(json!({
            "subtype": "elicitation", "mcp_server_name": "codex-cua",
            "message": "Allow access to Dia?", "mode": "form",
            "requested_schema": {"type": "object", "properties": {}},
        }));
        let approve = bridge(|qs| {
            assert!(qs[0].approval.is_some(), "an approval card");
            assert!(qs[0].question.contains("codex-cua"));
            Some(vec![UserInputAnswer { question_id: qs[0].id.clone(), labels: vec!["approve".into()] }])
        });
        assert_eq!(
            answer_elicitation(&access, approve.as_ref()).await,
            json!({"action": "accept", "content": {}})
        );
        let decline = bridge(|qs| {
            Some(vec![UserInputAnswer { question_id: qs[0].id.clone(), labels: vec!["decline".into()] }])
        });
        assert_eq!(answer_elicitation(&access, decline.as_ref()).await, json!({"action": "decline"}));
        let gone = bridge(|_| None);
        assert_eq!(
            answer_elicitation(&access, gone.as_ref()).await,
            json!({"action": "decline"}),
            "a dropped approval declines"
        );

        let form = elicitation(json!({
            "subtype": "elicitation", "mcp_server_name": "deploy", "message": "Configure the deploy",
            "requested_schema": {"type": "object", "required": ["env", "replicas"], "properties": {
                "replicas": {"type": "integer", "title": "Replicas"},
                "env": {"type": "string", "enum": ["prod", "dev"], "enumNames": ["Production", "Development"]},
                "notify": {"type": "boolean", "description": "Notify the team?"},
                "note": {"type": "string"},
            }},
            "property_order": ["env", "replicas", "notify", "note"],
        }));
        let answer = bridge(|qs| {
            assert_eq!(qs.len(), 4);
            assert!(qs[0].question.starts_with("Configure the deploy"));
            assert_eq!(qs[0].options, ["Production", "Development"]);
            assert_eq!(qs[1].header, "Replicas");
            assert_eq!(qs[2].options, ["Yes", "No"]);
            let label = |i: usize, l: &str| UserInputAnswer { question_id: qs[i].id.clone(), labels: vec![l.into()] };
            Some(vec![label(0, "Development"), label(1, " 3 "), label(2, "No")])
        });
        assert_eq!(
            answer_elicitation(&form, answer.as_ref()).await,
            json!({"action": "accept", "content": {"env": "dev", "replicas": 3, "notify": false}})
        );
        // A required field left blank declines; a run that ends cancels.
        let blank = bridge(|qs| {
            Some(vec![UserInputAnswer { question_id: qs[0].id.clone(), labels: vec!["Production".into()] }])
        });
        assert_eq!(answer_elicitation(&form, blank.as_ref()).await, json!({"action": "decline"}));
        assert_eq!(answer_elicitation(&form, gone.as_ref()).await, json!({"action": "cancel"}));

        let url = elicitation(json!({
            "subtype": "elicitation", "mcp_server_name": "github", "mode": "url",
            "message": "Sign in", "url": "https://example.com/auth",
        }));
        let open = bridge(|qs| {
            let approval = qs[0].approval.as_ref().unwrap();
            assert!(approval.reason.as_deref().unwrap().contains("https://example.com/auth"));
            Some(vec![UserInputAnswer { question_id: qs[0].id.clone(), labels: vec!["approve".into()] }])
        });
        assert_eq!(answer_elicitation(&url, open.as_ref()).await, json!({"action": "accept"}));
    }

    #[test]
    fn answers_key_by_question_text() {
        let input =
            json!({"questions": [{"header": "H", "question": "Pick one", "options": ["A", "B"]}]});
        let qs = parse_questions(&input);
        let answers = vec![UserInputAnswer {
            question_id: qs[0].id.clone(),
            labels: vec!["B".into()],
        }];
        let updated = updated_input_with_answers(&input, &qs, &answers);
        assert_eq!(updated["answers"]["Pick one"], json!("B"));
        // Original input is preserved alongside the answers.
        assert!(updated["questions"].is_array());
    }

    fn tool_request(tool: &str, input: Value) -> wire::ControlRequestBody {
        wire::ControlRequestBody {
            subtype: "can_use_tool".into(),
            tool_name: tool.into(),
            input,
            ..Default::default()
        }
    }

    #[test]
    fn decisions_map_to_permission_results() {
        let req = tool_request("Bash", json!({"command": "make"}));
        let cancel = permission_result(&req, ApprovalDecision::Cancel);
        assert_eq!(cancel["behavior"], "deny");
        assert_eq!(cancel["interrupt"], true);
        let once = permission_result(&req, ApprovalDecision::Approve);
        assert_eq!(once["behavior"], "allow");
        assert!(once.get("updatedPermissions").is_none());
        // No suggestions: allow this exact command for the session.
        let session = permission_result(&req, ApprovalDecision::ApproveForSession);
        assert_eq!(
            session["updatedPermissions"],
            json!([{
                "type": "addRules",
                "rules": [{"toolName": "Bash", "ruleContent": "make"}],
                "behavior": "allow",
                "destination": "session",
            }])
        );
    }

    #[test]
    fn approvals_describe_writes_mcp_tools_and_suppressed_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let cwd = dir.path().to_str().unwrap();
        let write = approval_request(
            &tool_request(
                "Write",
                json!({"file_path": dir.path().join("a.txt"), "content": "new\n"}),
            ),
            cwd,
        );
        assert_eq!(write.kind, ApprovalKind::FileChange);
        assert_eq!(write.title, "Edit a.txt");
        assert!(write.diff.unwrap().contains("-old\n+new"));
        let mcp = approval_request(&tool_request("mcp__linear__search", json!({})), cwd);
        assert_eq!(mcp.kind, ApprovalKind::McpTool);
        assert_eq!(mcp.title, "Use search (linear)");
        let mut suppressed = tool_request("Bash", json!({"command": "x"}));
        suppressed.suppress_always_allow_rule = true;
        let decisions: Vec<_> = approval_request(&suppressed, cwd)
            .decisions
            .iter()
            .map(|d| d.decision)
            .collect();
        assert_eq!(
            decisions,
            [ApprovalDecision::Approve, ApprovalDecision::Decline]
        );
    }
}

#[cfg(test)]
mod mcp_injection_tests {
    use super::*;

    #[test]
    fn mcp_config_arg_spells_the_server_the_way_the_cli_reads_it() {
        let mcp = zeron_proto::McpServer {
            name: "godspeed".into(),
            command: "/opt/godspeed/godspeed".into(),
            args: vec!["mcp".into()],
            env: [("GODSPEED_CHAT_ID".to_owned(), "chat-1".to_owned())]
                .into_iter()
                .collect(),
        };
        let parsed: Value = serde_json::from_str(&mcp_config_arg(&mcp)).unwrap();
        assert_eq!(parsed["mcpServers"]["godspeed"]["command"], "/opt/godspeed/godspeed");
        assert_eq!(
            parsed["mcpServers"]["godspeed"]["args"],
            serde_json::json!(["mcp"])
        );
        assert_eq!(
            parsed["mcpServers"]["godspeed"]["env"]["GODSPEED_CHAT_ID"],
            "chat-1"
        );
    }
}
