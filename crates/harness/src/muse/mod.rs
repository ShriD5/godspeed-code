//! Native Muse Code driver: MSP (the Muse Session Protocol) over `muse serve`
//! stdio — newline-delimited JSON-RPC 2.0, the wire Meta's own
//! `@muse-code/sdk` speaks. One host process carries one session. Every
//! prompt is a `turn/start` with `ifBusy: "steer"`, which Muse atomically
//! folds into the running turn or starts as the next one, so an idle race
//! can never strand a steer the way a separate status read + steer could.
//!
//! Access modes ride Muse's own approval modes and built-in permission
//! profiles (`:read-only`, `:ask-me`, `:auto-review`, `:unrestricted` —
//! read from the 1.4.8 binary and `muse serve --help`): supervised and
//! auto-accept edits are `promptUnmatched` + `:ask-me` (auto-accept edits
//! then approves in-workspace file writes here), auto is `:auto-review`
//! (Muse's AI reviewer decides), full access `allowAll` + `:unrestricted`
//! with the shell sandbox off. Approvals surface with Muse's own choices.
mod normalize;

use crate::{
    Harness, HarnessError, RunControls, SteerMessage, TurnControl,
    jsonrpc::{Incoming, RpcClient},
    process::{Command, Stdio, owned::Child},
};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use normalize::{Normalizer, string};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use zeron_proto::{
    AccessMode, AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

/// `initialize` and session load; a cold host loads skills and MCP servers.
const STARTUP: Duration = Duration::from_secs(120);

pub struct MuseHarness {
    models_cache: crate::catalog::Catalog,
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

impl Default for MuseHarness {
    fn default() -> Self {
        Self {
            models_cache: Default::default(),
            executable: None,
            interrupt_grace: Duration::from_secs(3),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl MuseHarness {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }
    pub fn with_graces(mut self, interrupt: Duration, kill: Duration) -> Self {
        self.interrupt_grace = interrupt;
        self.kill_grace = kill;
        self
    }
    pub fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = self
            .executable
            .clone()
            .or_else(|| std::env::var_os("MUSE_EXECUTABLE").map(PathBuf::from))
        {
            return crate::executable::validate_native_override(&path);
        }
        let home = crate::executable::home_or_current_dir();
        crate::executable::find_on_paths(
            "muse",
            vec![
                home.join(".local/bin/muse"),
                PathBuf::from("/opt/homebrew/bin/muse"),
                PathBuf::from("/usr/local/bin/muse"),
            ],
        )
        .ok_or_else(|| {
            HarnessError::NotInstalled(
                "Muse Code CLI (`muse`) was not found. Install Muse Code \
                 (`brew install --cask muse-code`) and run `muse login`."
                    .into(),
            )
        })
    }

    fn spawn(
        &self,
        cwd: &Path,
        args: &[&str],
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<Host, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        crate::process::owned::configure(&mut cmd);
        crate::compose_child_path(&mut cmd, &exe);
        crate::apply_run_env(&mut cmd, env, Some(cwd));
        // The CLI's own `muse login` is the account Godspeed runs on: an
        // inherited META_API_KEY would silently override it. Updates are the
        // user's (or Homebrew's) call, never a mid-session restart.
        cmd.env_remove("META_API_KEY")
            .env("MUSE_NO_AUTO_UPDATE", "1")
            .arg("serve")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = Child::new(cmd.spawn()?);
        let tail = crate::StderrTail::default();
        let mut lines = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
        let stderr = tail.clone();
        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                stderr.push(&line);
            }
            stderr.close();
        });
        let (client, incoming) = RpcClient::new(
            child.stdin.take().expect("piped stdin"),
            child.stdout.take().expect("piped stdout"),
        );
        Ok(Host {
            child,
            client,
            incoming,
            tail,
            stderr_task,
            exit_deadline: None,
            granted: Vec::new(),
        })
    }

    /// `model/list` from a throwaway host that can neither run commands nor
    /// write, and keeps no session log.
    async fn discover_models(&self) -> Result<Vec<Model>, HarnessError> {
        let cwd = crate::executable::home_or_current_dir();
        let mut host = self.spawn(
            &cwd,
            &["--disable-shell", "--disable-write", "--no-session-log"],
            &Default::default(),
        )?;
        let (mut tx, rx) = oneshot::channel();
        let grace = self.kill_grace;
        tokio::spawn(async move {
            let result = tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(30), async {
                    host.initialize().await?;
                    host.client.request("model/list", json!({})).await
                }) => result.unwrap_or_else(|_| Err(HarnessError::Protocol("Muse model discovery timed out".into()))),
                _ = tx.closed() => Err(HarnessError::Protocol("Muse model discovery cancelled".into())),
            };
            host.child.shutdown(grace).await;
            let _ = tx.send(result);
        });
        let list = rx
            .await
            .map_err(|_| HarnessError::Protocol("Muse model discovery failed".into()))??;
        let models = normalize::models(&list);
        // MSP has no auth signal, and a logged-out CLI answers `model/list`
        // with an empty bundled catalog rather than an error.
        if models.is_empty() {
            return Err(HarnessError::Discovery(crate::CatalogFailure {
                code: crate::CatalogFailureCode::AuthRequired,
                message: "Muse Code returned no models. Run `muse login` and refresh.".into(),
            }));
        }
        Ok(models)
    }
}

/// `muse serve` flags for a run. Sandbox posture is fixed for the host's
/// lifetime; approval mode is selected on the wire instead. Every mode but
/// full access keeps the shell sandbox.
fn serve_args(request: &RunRequest) -> Vec<&'static str> {
    match request.access_mode() {
        AccessMode::FullAccess => vec!["--trust-workspace", "--disable-sandbox"],
        _ => vec!["--trust-workspace"],
    }
}

/// The approval mode and permission profile a session selects. Without an
/// explicit profile Muse applies its own default, which can be
/// `:auto-review` — an AI reviewer deciding approvals instead of the user —
/// so every mode names one.
fn approval(mode: AccessMode) -> (&'static str, &'static str) {
    match mode {
        AccessMode::Supervised | AccessMode::AutoAcceptEdits => ("promptUnmatched", ":ask-me"),
        AccessMode::Auto => ("promptUnmatched", ":auto-review"),
        AccessMode::FullAccess => ("allowAll", ":unrestricted"),
    }
}

/// The picked model, unless it is the catalog default Muse already applies.
fn picked_model(request: &RunRequest) -> Option<&str> {
    request
        .model
        .as_deref()
        .filter(|m| !m.is_empty() && *m != "default")
}

/// Godspeed's MCP server as a session-scoped Muse server. Optional: Muse
/// defaults to `required`, which fails the whole run when the tools cannot
/// be reached, and the agent should still work without them.
fn session_config(mcp: Option<&zeron_proto::McpServer>) -> Option<Value> {
    let mcp = mcp?;
    let server = json!({
        "transport": "stdio",
        "command": mcp.command,
        "args": mcp.args,
        "env": mcp.env,
        "mode": "optional",
    });
    let mut servers = serde_json::Map::new();
    servers.insert(mcp.name.clone(), server);
    Some(json!({ "mcpServers": servers }))
}

/// MSP command ids are client-minted UUIDv7s.
fn command_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let random = uuid::Uuid::new_v4().into_bytes();
    let mut tail = [0u8; 10];
    tail.copy_from_slice(&random[6..]);
    uuid::Builder::from_unix_timestamp_millis(millis, &tail)
        .into_uuid()
        .to_string()
}

fn missing_session(error: &HarnessError) -> bool {
    let text = error.to_string();
    text.contains("sessionNotFound") || text.contains("\"kind\":\"notFound\"")
}

struct Host {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    tail: crate::StderrTail,
    stderr_task: tokio::task::JoinHandle<()>,
    exit_deadline: Option<tokio::time::Instant>,
    granted: Vec<String>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}

impl Host {
    async fn initialize(&mut self) -> Result<(), HarnessError> {
        let result = self
            .client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "godspeed",
                        "title": "Godspeed",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "requestedCapabilities": ["sessionMcp"] },
                }),
            )
            .await?;
        // A fingerprint mismatch only means additive schema changes; the
        // envelope version is the compatibility line.
        if result["schema"]["version"] != 1 {
            return Err(HarnessError::Protocol(
                "Muse speaks an unsupported MSP version; update Muse Code".into(),
            ));
        }
        self.granted = result["grantedCapabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_owned))
            .collect();
        self.client.notify("initialized", None);
        Ok(())
    }

    async fn next(&mut self) -> Result<Incoming, HarnessError> {
        loop {
            tokio::select! {
                // Drain buffered output before reporting process death. A
                // descendant may retain stdout, so EOF alone cannot govern liveness.
                biased;
                _ = async {
                    match self.exit_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(HarnessError::Protocol("Muse exited".into())),
                status = self.child.wait(), if self.exit_deadline.is_none() => {
                    status?;
                    self.exit_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(100));
                }
                incoming = self.incoming.recv() => return match incoming {
                    Some(Incoming::Eof) | None => Err(HarnessError::Protocol("Muse disconnected".into())),
                    Some(incoming) => Ok(incoming),
                },
            }
        }
    }
}

#[async_trait]
impl Harness for MuseHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Muse
    }
    fn display_name(&self) -> &str {
        "Muse Code"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// A steer lands at the running turn's next model-call boundary.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    /// Efforts are per model (`model/list` variants).
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_executable().ok()
    }
    /// Every turn — prompted, steered, or one Muse starts itself to report
    /// a finished workflow — ends with its own durable `turn/completed`.
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    /// `turn/interrupt` cancels only the turn; the host and its background
    /// subagents and workflows stay up.
    fn stops_turn_in_place(&self) -> bool {
        true
    }
    /// The sandbox is a `muse serve` flag and the permission profile is
    /// chosen when the session loads; both need a fresh host.
    fn same_runtime(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.model == next.model
            && live.reasoning == next.reasoning
            && crate::same_model_options(&live.model_options, &next.model_options)
            && live.cwd == next.cwd
            && live.access_mode() == next.access_mode()
            && live.writable_roots() == next.writable_roots()
    }
    /// `session/setModel` switches the live session, and effort rides each
    /// turn. Clearing back to the default model cannot be expressed that way.
    fn reconfigures_in_place(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.cwd == next.cwd
            && live.access_mode() == next.access_mode()
            && live.writable_roots() == next.writable_roots()
            && (picked_model(next).is_some() || picked_model(live).is_none())
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(self.model_catalog(false).await?.models)
    }
    fn model_context(&self) -> Result<Option<crate::ModelContext>, HarnessError> {
        crate::model_context::context(HarnessId::Muse, &self.resolve_executable()?, &[]).map(Some)
    }
    async fn model_catalog(&self, force: bool) -> Result<crate::ModelCatalog, HarnessError> {
        self.models_cache
            .get_with_timeout(
                force,
                Duration::from_secs(35),
                || Ok(self.model_context()?.expect("Muse model context").key()),
                || self.discover_models(),
            )
            .await
    }
    fn fallback_models(&self) -> Vec<Model> {
        vec![Model {
            id: "default".into(),
            label: "Muse default".into(),
            description: Some("Model configured in Muse Code".into()),
            reasoning_levels: vec![],
            options: vec![],
        }]
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let host = self.spawn(Path::new(&request.cwd), &serve_args(&request), &request.env)?;
        let (tx, rx) = mpsc::channel(256);
        let kill_grace = self.kill_grace;
        let interrupt_grace = self.interrupt_grace;
        tokio::spawn(async move {
            // The lease lives through shutdown even if the consumer drops its stream.
            let RunControls {
                realtime: _,
                execution_lease: _lease,
                request_input,
                mut steering,
                interrupt,
                turn,
            } = controls;
            let mut runner = Runner {
                host,
                tx,
                request,
                input: Arc::from(request_input),
                turn,
                norm: Normalizer::default(),
                session: String::new(),
                assistant: uuid::Uuid::new_v4().to_string(),
                lost_context: None,
                active: true,
                interrupted: false,
                stopping: false,
                running: Vec::new(),
                seen: HashSet::new(),
                expected: HashSet::new(),
                admitting: HashSet::new(),
                acks: mpsc::unbounded_channel(),
                queued: VecDeque::new(),
                deliveries: VecDeque::new(),
                dialogs: Dialogs::default(),
            };
            let consumer = runner.tx.clone();
            let result = tokio::select! {
                result = runner.run(&mut steering) => result,
                _ = interrupt.cancelled() => { runner.interrupt(interrupt_grace).await; Ok(()) },
                _ = consumer.closed() => Ok(()),
            };
            if let Err(error) = result {
                let status = runner.host.child.try_wait().ok().flatten();
                runner.norm.error = Some(if status.is_some() {
                    crate::crash_message("Muse", status, &runner.host.tail)
                } else {
                    error.to_string()
                });
            }
            runner.dialogs.abort_all();
            runner.host.child.shutdown(kill_grace).await;
            let _ = runner.finish().await;
        });
        Ok(
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
                .boxed(),
        )
    }
}

type Input =
    dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync;

/// Open approvals and questions, by native id. Muse may settle one itself
/// (a policy decision, another client, a timeout), which retires our dialog.
#[derive(Default)]
struct Dialogs {
    tasks: JoinSet<()>,
    open: HashMap<String, tokio::task::AbortHandle>,
}

impl Dialogs {
    fn open(&mut self, id: String, task: impl std::future::Future<Output = ()> + Send + 'static) {
        while self.tasks.try_join_next().is_some() {}
        if let Some(previous) = self.open.insert(id, self.tasks.spawn(task)) {
            previous.abort();
        }
    }
    fn contains(&self, id: &str) -> bool {
        self.open.contains_key(id)
    }
    fn settle(&mut self, id: &str) {
        if let Some(task) = self.open.remove(id) {
            task.abort();
        }
    }
    fn abort_all(&mut self) {
        self.open.clear();
        self.tasks.abort_all();
    }
}

/// A `turn/start` ack for the submission with this command id.
type Ack = (String, Result<Value, HarnessError>);

/// A submitted steer awaiting proof Muse consumed it: its user message
/// item, or the turn it started. The input is kept for a resubmit.
struct Delivery {
    command: String,
    input: Vec<Value>,
}

struct Runner {
    host: Host,
    tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    request: RunRequest,
    input: Arc<Input>,
    turn: TurnControl,
    norm: Normalizer,
    session: String,
    assistant: String,
    lost_context: Option<String>,
    /// The engine has a turn open and is owed a `Done`.
    active: bool,
    /// The runtime is being torn down.
    interrupted: bool,
    /// The user stopped the turn; the host stays up.
    stopping: bool,
    /// Muse turns started and not yet completed, in start order. A turn
    /// Muse runs ahead of ours (a workflow reporting back) joins the same
    /// engine turn, so `Done` waits for all of them.
    running: Vec<String>,
    seen: HashSet<String>,
    /// Turns Muse admitted but has not launched yet.
    expected: HashSet<String>,
    /// Submissions whose `turn/start` ack is outstanding, and the channel
    /// their acks arrive on (outside the ordered notification stream).
    admitting: HashSet<String>,
    acks: (mpsc::UnboundedSender<Ack>, mpsc::UnboundedReceiver<Ack>),
    /// Steers waiting for the previous submission's admission.
    queued: VecDeque<SteerMessage>,
    deliveries: VecDeque<Delivery>,
    dialogs: Dialogs,
}

impl Runner {
    async fn emit(&self, event: AgentEvent) -> Result<(), HarnessError> {
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| HarnessError::Protocol("Muse event consumer closed".into()))
    }

    async fn started(&self, model: String) -> Result<(), HarnessError> {
        self.emit(AgentEvent::SessionStarted {
            harness: HarnessId::Muse,
            model,
            tools: vec![],
            cwd: self.request.cwd.clone(),
            session_id: self.session.clone(),
            assistant_message_id: self.assistant.clone(),
        })
        .await
    }

    async fn finish(&mut self) -> Result<(), HarnessError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.dialogs.abort_all();
        let status = if self.interrupted || self.stopping {
            zeron_proto::DoneStatus::Interrupted
        } else {
            self.norm.status()
        };
        self.stopping = false;
        if status == zeron_proto::DoneStatus::Errored
            && let Some(reset) = self.norm.limit
        {
            self.emit(crate::limits::event(reset, None)).await?;
        }
        self.emit(AgentEvent::Done {
            status,
            result: None,
            error: self.norm.error.clone(),
            session_id: (!self.session.is_empty()).then(|| self.session.clone()),
        })
        .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.host.client.request(method, params).await
    }

    /// Fire a command whose admission the view stream confirms anyway.
    fn command(&self, method: &str, mut params: Value) {
        params["commandId"] = command_id().into();
        params["sessionId"] = self.session.clone().into();
        drop(self.host.client.request_now(method, params));
    }

    async fn bootstrap(&mut self) -> Result<String, HarnessError> {
        self.host.initialize().await?;
        let config = if self.host.granted.iter().any(|c| c == "sessionMcp") {
            session_config(self.request.mcp.as_ref())
        } else {
            None
        };
        let (mode, profile) = approval(self.request.access_mode());
        if let Some(id) = self.request.resume.clone() {
            let mut params = json!({
                "commandId": command_id(),
                "sessionId": id,
                // Godspeed already has the transcript.
                "excludeItems": true,
                // Selected again on resume: the chat may predate it, or its
                // approval setting changed (the T3 Code driver does the same).
                "permissionProfile": profile,
            });
            if let Some(config) = &config {
                params["config"] = config.clone();
            }
            match self.request("session/resume", params).await {
                Ok(result) => {
                    self.session = string(&result["session"], "sessionId").into();
                    if self.session != id {
                        return Err(HarnessError::Protocol(
                            "Muse resumed a different session".into(),
                        ));
                    }
                    self.request(
                        "session/setApprovalMode",
                        json!({"commandId": command_id(), "sessionId": id, "mode": mode}),
                    )
                    .await?;
                    // A resumed session keeps the model it last ran with.
                    if let Some(model) = picked_model(&self.request) {
                        let model = model.to_owned();
                        self.set_model(&model).await?;
                        return Ok(model);
                    }
                    return Ok(result["session"]["modelId"]
                        .as_str()
                        .unwrap_or("default")
                        .into());
                }
                // Like every other harness, a session that can no longer be
                // reopened starts fresh with a visible notice. Failing instead
                // would strand the chat: the engine resumes the same id forever.
                Err(error) if missing_session(&error) => {
                    self.lost_context = Some(format!(
                        "Muse could not restore session {id}; starting a new session without the previous context: {error}"
                    ));
                    self.request.resume = None;
                }
                Err(error) => return Err(error),
            }
        }
        // Muse rejects non-canonical workspace roots (macOS /tmp, say).
        let root = std::fs::canonicalize(&self.request.cwd)
            .unwrap_or_else(|_| PathBuf::from(&self.request.cwd));
        let mut params = json!({
            "commandId": command_id(),
            "workspaceRoot": root,
            "approvalMode": mode,
            "permissionProfile": profile,
        });
        if let Some(model) = picked_model(&self.request) {
            params["modelId"] = model.into();
        }
        if let Some(config) = config {
            params["config"] = config;
        }
        let result = self.request("session/start", params).await?;
        self.session = string(&result["session"], "sessionId").into();
        if self.session.is_empty() {
            return Err(HarnessError::Protocol(
                "Muse session/start omitted sessionId".into(),
            ));
        }
        Ok(result["session"]["modelId"]
            .as_str()
            .or(picked_model(&self.request))
            .unwrap_or("default")
            .into())
    }

    async fn set_model(&self, model: &str) -> Result<(), HarnessError> {
        self.request(
            "session/setModel",
            json!({
                "commandId": command_id(),
                "sessionId": self.session,
                "model": { "modelId": model },
            }),
        )
        .await
        .map(drop)
    }

    async fn run(
        &mut self,
        steering: &mut mpsc::Receiver<SteerMessage>,
    ) -> Result<(), HarnessError> {
        let model = tokio::time::timeout(STARTUP, self.bootstrap())
            .await
            .map_err(|_| HarnessError::Protocol("Muse startup timed out".into()))??;
        self.started(model).await?;
        if let Some(message) = self.lost_context.take() {
            self.emit(AgentEvent::Error { message }).await?;
        }
        let input = input_parts(&self.request.prompt, &self.request.attachments).await;
        self.submit(input, false);
        let mut open = true;
        loop {
            // One admission at a time: Muse may handle back-to-back commands
            // out of order, which would run a later prompt ahead of an earlier
            // one. A prompt sent during a stop waits for the stopped turn's end.
            if self.admitting.is_empty()
                && !self.stopping
                && let Some(steer) = self.queued.pop_front()
            {
                self.steer(steer).await?;
            }
            if !self.active && !open && self.queued.is_empty() {
                return Ok(());
            }
            tokio::select! {
                incoming = self.host.next() => self.incoming(incoming?).await?,
                Some((command, ack)) = self.acks.1.recv() => self.acked(command, ack).await?,
                steer = steering.recv(), if open => match steer {
                    Some(steer) => self.queued.push_back(steer),
                    None => open = false,
                },
                _ = self.turn.stop_requested() => self.stop().await?,
            }
            self.turn.set_background(self.norm.background());
        }
    }

    /// Submit a prompt. `ifBusy: "steer"` lands it in the running turn at
    /// its next model call, or starts a turn when Muse is idle.
    fn submit(&mut self, input: Vec<Value>, delivery: bool) {
        let command = command_id();
        let mut params = json!({
            "commandId": command,
            "sessionId": self.session,
            "input": input,
            "ifBusy": "steer",
        });
        if let Some(effort) = self.request.reasoning.and_then(normalize::effort) {
            params["reasoningEffort"] = effort.into();
        }
        let ack = self.host.client.request_now("turn/start", params);
        let (acks, id) = (self.acks.0.clone(), command.clone());
        tokio::spawn(async move {
            let _ = acks.send((id, ack.await));
        });
        self.admitting.insert(command.clone());
        if delivery {
            self.deliveries.push_back(Delivery { command, input });
        }
    }

    async fn steer(&mut self, steer: SteerMessage) -> Result<(), HarnessError> {
        // A changed model applies on the live session before the message it
        // was sent with; effort rides the turn itself.
        if let Some(next) = steer.config {
            if let Some(model) =
                picked_model(&next).filter(|m| Some(*m) != picked_model(&self.request))
            {
                self.set_model(model).await?;
            }
            self.request.model = next.model.clone();
            self.request.reasoning = next.reasoning;
            self.request.model_options = next.model_options.clone();
        }
        if !self.active {
            self.active = true;
            self.norm.reset();
        }
        let input = input_parts(&steer.prompt, &steer.attachments).await;
        self.submit(input, true);
        Ok(())
    }

    async fn acked(
        &mut self,
        command: String,
        ack: Result<Value, HarnessError>,
    ) -> Result<(), HarnessError> {
        self.admitting.remove(&command);
        // A refused prompt never runs: end the runtime with Muse's reason.
        // The engine re-dispatches any steer it never saw confirmed.
        let ack = ack?;
        let turn = string(&ack, "turnId");
        if matches!(string(&ack, "disposition"), "started" | "queued") && !self.seen.contains(turn)
        {
            self.expected.insert(turn.to_owned());
        }
        tracing::debug!(%command, disposition = string(&ack, "disposition"), "Muse admitted prompt");
        self.settle().await
    }

    /// Stop the in-flight turn, keeping the host: steers it had accepted
    /// but not delivered are cancelled with it.
    async fn stop(&mut self) -> Result<(), HarnessError> {
        if !self.active {
            return Ok(());
        }
        self.stopping = true;
        self.queued.clear();
        self.deliveries.clear();
        self.dialogs.abort_all();
        for turn in &self.running {
            self.command("turn/interrupt", json!({ "turnId": turn }));
        }
        for turn in &self.expected {
            self.command("turn/unqueue", json!({ "turnId": turn }));
        }
        // Nothing in flight (a steer queued behind no admission): done now.
        self.settle().await
    }

    async fn interrupt(&mut self, grace: Duration) {
        self.interrupted = true;
        let _ = self.stop().await;
        let _ = tokio::time::timeout(grace, async {
            while self.active {
                let handled = tokio::select! {
                    incoming = self.host.next() => match incoming {
                        Ok(incoming) => self.incoming(incoming).await,
                        Err(error) => Err(error),
                    },
                    Some((command, ack)) = self.acks.1.recv() => self.acked(command, ack).await,
                };
                if handled.is_err() {
                    break;
                }
            }
        })
        .await;
    }

    async fn incoming(&mut self, incoming: Incoming) -> Result<(), HarnessError> {
        match incoming {
            // Presentation receipts only: the matching view notifications
            // drive the dialog, and decisions travel as commands.
            Incoming::Request { id, method, .. } => {
                if matches!(method.as_str(), "approval/request" | "userInput/request") {
                    self.host.client.respond(&id, json!({}));
                } else {
                    self.host.client.respond_error(
                        &id,
                        -32601,
                        &format!("unsupported method: {method}"),
                    );
                }
                Ok(())
            }
            Incoming::Notification { method, params } => self.notification(&method, params).await,
            Incoming::Eof => Err(HarnessError::Protocol("Muse disconnected".into())),
        }
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<(), HarnessError> {
        if params["sessionId"]
            .as_str()
            .is_some_and(|id| id != self.session)
        {
            return Ok(());
        }
        match method {
            "turn/started" => self.turn_started(string(&params, "turnId")).await?,
            "turn/completed" => {
                let turn = string(&params, "turnId");
                self.running.retain(|t| t != turn);
                self.norm.turn_completed(&params);
            }
            "turn/unqueued" => {
                self.expected.remove(string(&params, "turnId"));
            }
            "view/gap" => {
                return Err(HarnessError::Protocol(
                    "Muse dropped view events; the session must be reopened".into(),
                ));
            }
            "approval/requested" => self.approval(&params),
            "approval/updated" if self.dialogs.contains(string(&params, "approvalId")) => {
                self.approval(&params)
            }
            "approval/resolved" => self.dialogs.settle(string(&params, "approvalId")),
            "userInput/requested" => self.question(&params),
            "userInput/settled" => self.dialogs.settle(string(&params, "userInputId")),
            "item/started" | "item/updated" | "item/completed"
                if params["item"]["kind"] == "userMessage" =>
            {
                self.confirm(string(&params["item"], "commandId")).await?;
            }
            _ => {}
        }
        for event in self.norm.map(method, &params) {
            self.emit(event).await?;
        }
        if matches!(method, "turn/completed" | "turn/unqueued") {
            self.settle().await?;
        }
        Ok(())
    }

    async fn turn_started(&mut self, turn: &str) -> Result<(), HarnessError> {
        self.seen.insert(turn.to_owned());
        self.expected.remove(turn);
        if !self.running.iter().any(|t| t == turn) {
            self.running.push(turn.to_owned());
        }
        if self.stopping {
            self.command("turn/interrupt", json!({ "turnId": turn }));
        }
        if !self.active {
            // A turn Muse started itself, e.g. to report a finished workflow.
            self.active = true;
            self.norm.reset();
            self.assistant = uuid::Uuid::new_v4().to_string();
            self.started(picked_model(&self.request).unwrap_or("default").into())
                .await?;
        }
        // A fresh turn's id is its submitting command's.
        self.confirm(turn).await
    }

    /// The steer submitted as `command` reached the model.
    async fn confirm(&mut self, command: &str) -> Result<(), HarnessError> {
        let Some(at) = self.deliveries.iter().position(|d| d.command == command) else {
            return Ok(());
        };
        self.deliveries.remove(at);
        self.norm.segment();
        let old = std::mem::replace(&mut self.assistant, uuid::Uuid::new_v4().to_string());
        self.emit(AgentEvent::Steered {
            assistant_message_id: Some(old),
            next_assistant_message_id: Some(self.assistant.clone()),
        })
        .await
    }

    /// End the engine's turn once Muse has nothing running, launching, or
    /// awaiting admission.
    async fn settle(&mut self) -> Result<(), HarnessError> {
        if !self.active
            || !self.running.is_empty()
            || !self.expected.is_empty()
            || !self.admitting.is_empty()
            || (!self.queued.is_empty() && !self.stopping)
        {
            return Ok(());
        }
        if !self.deliveries.is_empty() && !self.interrupted && !self.stopping {
            // Admitted into a turn that ended before its next model call: run
            // it as its own turn rather than leave the engine waiting on it.
            for delivery in std::mem::take(&mut self.deliveries) {
                tracing::warn!(command = %delivery.command, "Muse steer outlived its turn; resubmitting");
                self.submit(delivery.input, true);
            }
            return Ok(());
        }
        self.finish().await
    }

    fn approval(&mut self, params: &Value) {
        let id = string(params, "approvalId").to_owned();
        let client = self.host.client.clone();
        let decide = {
            let params = params.clone();
            let session = self.session.clone();
            move |choice: String| {
                client.request_now(
                    "approval/decide",
                    json!({
                        "commandId": command_id(),
                        "sessionId": session,
                        "approvalId": params["approvalId"],
                        "requirementId": params["currentRequirementId"],
                        "choiceId": choice,
                    }),
                )
            }
        };
        let (question, choices) = normalize::approval_question(params);
        // allowAll does not reach a workflow child's approvals; full access
        // approves them here. Auto-accept edits approves in-workspace writes.
        let mode = self.request.access_mode();
        let auto = question.approval.as_ref().is_some_and(|approval| {
            let in_workspace = crate::approval::paths_within(
                approval.paths.iter().map(String::as_str),
                &self.request.cwd,
                self.request.writable_roots(),
            );
            crate::approval::auto_decision(mode, approval.kind, in_workspace)
        });
        if auto {
            if let Some(choice) = normalize::default_choice(params, true) {
                self.dialogs.open(id, async move {
                    let _ = decide(choice).await;
                });
            }
            return;
        }
        let fallback = normalize::default_choice(params, false);
        let input = self.input.clone();
        self.dialogs.open(id, async move {
            // A dropped answer (the turn stopped) declines; never a silent allow.
            let answers = (input)(vec![question.clone()]).await.unwrap_or_default();
            // By label; a decision spelled another way (an orchestrator's
            // wire name, an old client's yes/no) by its matching choice.
            let picked = answers
                .iter()
                .find(|a| a.question_id == question.id)
                .and_then(|a| a.labels.first())
                .and_then(|label| {
                    choices
                        .iter()
                        .find(|(l, _)| l == label)
                        .map(|(_, choice)| choice.clone())
                        .or_else(|| {
                            let approval = question.approval.as_ref()?;
                            let decision = approval.decision_for(label)?;
                            let option = approval.decisions.iter().find(|o| o.decision == decision)?;
                            choices
                                .iter()
                                .find(|(l, _)| *l == option.label)
                                .map(|(_, choice)| choice.clone())
                        })
                });
            if let Some(choice) = picked.or(fallback)
                && let Err(error) = decide(choice).await
            {
                tracing::warn!(%error, "Muse approval decision refused");
            }
        });
    }

    fn question(&mut self, params: &Value) {
        let id = string(params, "userInputId").to_owned();
        let questions = normalize::input_questions(params);
        let client = self.host.client.clone();
        let session = self.session.clone();
        let input = self.input.clone();
        let task_id = id.clone();
        self.dialogs.open(id, async move {
            let asked = questions.iter().map(|(_, q)| q.clone()).collect();
            let answers = (input)(asked).await.unwrap_or_default();
            let mut params = json!({
                "commandId": command_id(),
                "sessionId": session,
                "userInputId": task_id,
            });
            let method = match normalize::input_answers(&questions, &answers) {
                Some(answers) => {
                    params["answers"] = answers;
                    "userInput/answer"
                }
                None => "userInput/cancel",
            };
            if let Err(error) = client.request(method, params).await {
                tracing::warn!(%error, "Muse question answer refused");
            }
        });
    }
}

/// The prompt as MSP input parts. Images inline best-effort like the Pi and
/// Claude drivers: the path refs already ride the prompt text, so an
/// unreadable, oversized or non-inlinable file must not fail the turn.
async fn input_parts(prompt: &str, attachments: &[String]) -> Vec<Value> {
    use base64::Engine;
    let mut parts = Vec::new();
    if !prompt.is_empty() {
        parts.push(json!({ "type": "text", "text": prompt }));
    }
    for path in attachments {
        match tokio::fs::metadata(path).await {
            Ok(meta) if meta.len() <= 20 * 1024 * 1024 => {}
            _ => {
                tracing::debug!(%path, "Muse attachment unreadable or over the inline cap; path ref only");
                continue;
            }
        }
        let Ok(bytes) = tokio::fs::read(path).await else {
            continue;
        };
        let media = if bytes.starts_with(b"\x89PNG") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            tracing::debug!(%path, "Muse attachment is not an inlinable image; path ref only");
            continue;
        };
        parts.push(json!({
            "type": "image",
            "mediaType": media,
            "base64Data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
    }
    // MSP requires a non-empty input.
    if parts.is_empty() {
        parts.push(json!({ "type": "text", "text": " " }));
    }
    parts
}

#[cfg(test)]
mod tests;
