//! Settings → Devices → Servers: "Run on a server". A wizard installs the
//! headless engine on a Linux machine over the user's own SSH (or a Tailscale
//! peer), signs it into this workspace and starts it as a user service, so
//! agents keep running when this computer sleeps. The server then syncs in as
//! a device chats can be started on. Saved servers show their status (online,
//! version, uptime) with Update / Stop / Start / Uninstall.
//!
//! All remote work goes through `zeron_remote` (BatchMode ssh, idempotent
//! scripts that never restart a running engine); this file is the UI and the
//! order of steps.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use gpui_tokio::Tokio;
use zeron_proto::AuthState;
use zeron_remote::login::{LoginEvent, LoginSession};
use zeron_remote::scripts::{self, InstallPlan, StartOutcome};
use zeron_remote::servers::{self as store, SavedServer};
use zeron_remote::tailscale::{self, TailscalePeer};
use zeron_remote::{Arch, ServerProbe, SshRunner, SshTarget};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const PROBE_TIMEOUT: Duration = Duration::from_secs(40);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(35 * 60);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Pure decisions (unit-tested)
// ---------------------------------------------------------------------------

/// The workspace a new server joins: the organization this computer is signed
/// into, if any.
pub fn workspace_org(auth: Option<&AuthState>) -> Option<String> {
    match auth {
        Some(AuthState::SignedIn {
            org_id: Some(org), ..
        }) => Some(org.clone()),
        _ => None,
    }
}

/// What happens after the binaries are installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterInstall {
    /// Sign the server in first (stopping a signed-out engine before that).
    SignIn { org: String, stop_first: bool },
    /// Already signed in, or this computer has no workspace to join.
    Start { local_only: bool },
}

pub fn after_install(org: Option<String>, probe: &ServerProbe) -> AfterInstall {
    match org {
        Some(_) if probe.signed_in => AfterInstall::Start { local_only: false },
        Some(org) => AfterInstall::SignIn {
            org,
            stop_first: probe.running,
        },
        None => AfterInstall::Start {
            local_only: !probe.signed_in,
        },
    }
}

/// The status fragments of a saved server's row.
pub fn status_fragments(probe: &ServerProbe, latest: Option<&str>) -> Vec<String> {
    let mut parts = Vec::new();
    match (&probe.installed, probe.running) {
        (None, _) => parts.push("Not installed".to_owned()),
        (Some(_), true) => parts.push("Online".to_owned()),
        (Some(_), false) => parts.push("Stopped".to_owned()),
    }
    if let Some(version) = &probe.installed {
        parts.push(format!("v{version}"));
    }
    if probe.running
        && let Some(secs) = probe.uptime_secs
    {
        parts.push(format!("up {}", zeron_remote::format_uptime(secs)));
    }
    if let (Some(installed), Some(latest)) = (&probe.installed, latest)
        && zeron_update::version_newer(latest, installed)
    {
        parts.push(format!("{latest} available"));
    }
    parts
}

/// Fill the target field from a Tailscale peer, keeping a typed user name.
pub fn target_for_peer(current: &str, peer: &TailscalePeer) -> String {
    match current.trim().split_once('@') {
        Some((user, _)) if !user.is_empty() => format!("{user}@{}", peer.ssh_host()),
        _ => peer.ssh_host(),
    }
}

/// The closing message of the wizard.
pub fn done_summary(host: &str, version: &str, outcome: &StartOutcome, local_only: bool) -> String {
    let how = if outcome.systemd {
        "a systemd user service"
    } else {
        "a background process"
    };
    let state = if outcome.already_running {
        "was already running"
    } else {
        "is running"
    };
    if local_only {
        format!(
            "Godspeed {version} {state} on {host} as {how}, local-only: this computer isn't signed in to a workspace, so the server can't appear here. Sign in (Settings → Account) and run this setup again to join it."
        )
    } else {
        format!(
            "Godspeed {version} {state} on {host} as {how}. It appears under Other devices within a minute; start chats on it from the new-chat device picker."
        )
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Update,
    Stop,
    Start,
    Uninstall,
}

impl RowAction {
    fn busy_label(self) -> &'static str {
        match self {
            Self::Update => "Updating…",
            Self::Stop => "Stopping…",
            Self::Start => "Starting…",
            Self::Uninstall => "Uninstalling…",
        }
    }
}

enum RowStatus {
    Checking,
    Ready(ServerProbe),
    Unreachable(String),
}

struct ServerRow {
    saved: SavedServer,
    status: RowStatus,
    busy: Option<&'static str>,
}

enum PeerList {
    /// No `tailscale` CLI.
    Unavailable,
    Loading,
    Loaded(Vec<TailscalePeer>),
    Failed(String),
}

enum Step {
    Target,
    Checking,
    Ready {
        arch: Arch,
    },
    Installing,
    Stopping,
    SignIn {
        url: Option<String>,
        sending: bool,
    },
    Starting,
    Done {
        summary: String,
        follow_up: Option<String>,
    },
}

struct Wizard {
    target_input: Entity<ComposerInput>,
    code_input: Entity<ComposerInput>,
    peers: PeerList,
    step: Step,
    target: Option<SshTarget>,
    probe: Option<ServerProbe>,
    version: Option<String>,
    local_only: bool,
    error: Option<String>,
    login: Arc<Mutex<Option<LoginSession>>>,
    task: Option<Task<()>>,
    _events: Vec<Subscription>,
}

impl Wizard {
    fn target_label(&self) -> String {
        self.target
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    fn end_login(&mut self) {
        if let Some(session) = self.login.lock().unwrap_or_else(|e| e.into_inner()).take() {
            session.cancel();
        }
    }
}

pub struct ServersPanel {
    state: Entity<AppState>,
    runner: SshRunner,
    loaded_from: Option<PathBuf>,
    rows: Vec<ServerRow>,
    latest_linux: Option<String>,
    wizard: Option<Wizard>,
    /// A row action waiting for its confirming second click.
    confirm: Option<(String, RowAction)>,
    error: Option<SharedString>,
}

impl ServersPanel {
    pub fn new(state: Entity<AppState>, _cx: &mut Context<Self>) -> Self {
        Self {
            state,
            runner: SshRunner::new(),
            loaded_from: None,
            rows: Vec::new(),
            latest_linux: None,
            wizard: None,
            confirm: None,
            error: None,
        }
    }

    fn data_dir(&self, cx: &Context<Self>) -> Option<PathBuf> {
        self.state.read(cx).data_dir.clone()
    }

    /// Load `servers.json` once per data dir and check each server.
    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.data_dir(cx) else {
            return;
        };
        if self.loaded_from.as_ref() == Some(&dir) {
            return;
        }
        self.loaded_from = Some(dir.clone());
        self.rows = store::load(&dir)
            .into_iter()
            .map(|saved| ServerRow {
                saved,
                status: RowStatus::Checking,
                busy: None,
            })
            .collect();
        if self.rows.is_empty() {
            return;
        }
        self.fetch_latest_linux(cx);
        let targets: Vec<String> = self.rows.iter().map(|r| r.saved.target.clone()).collect();
        for target in targets {
            self.refresh_row(target, cx);
        }
    }

    fn fetch_latest_linux(&mut self, cx: &mut Context<Self>) {
        let fetch = Tokio::spawn(cx, async {
            zeron_update::fetch_latest_for(zeron_update::DEFAULT_FEED_URL, "linux").await
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(manifest)) = fetch.await {
                this.update(cx, |panel, cx| {
                    panel.latest_linux = Some(manifest.version);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn refresh_row(&mut self, target: String, cx: &mut Context<Self>) {
        let Ok(parsed) = SshTarget::parse(&target) else {
            self.set_row_status(&target, RowStatus::Unreachable("invalid address".into()));
            return;
        };
        let runner = self.runner.clone();
        let probe = cx.background_spawn(async move {
            runner
                .run_script(&parsed, &scripts::probe_script(), PROBE_TIMEOUT)
                .map_err(|err| err.user_message(&parsed.to_string()))
                .and_then(|out| ServerProbe::parse(&out.stdout))
        });
        cx.spawn(async move |this, cx| {
            let result = probe.await;
            this.update(cx, |panel, cx| {
                panel.set_row_status(
                    &target,
                    match result {
                        Ok(probe) => RowStatus::Ready(probe),
                        Err(message) => RowStatus::Unreachable(message),
                    },
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_row_status(&mut self, target: &str, status: RowStatus) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.saved.target == target) {
            row.status = status;
        }
    }

    fn run_action(&mut self, target: String, action: RowAction, cx: &mut Context<Self>) {
        // Uninstall always confirms; Update and Stop confirm while the engine
        // is running (both stop live agents).
        let running = self.rows.iter().any(|r| {
            r.saved.target == target && matches!(&r.status, RowStatus::Ready(p) if p.running)
        });
        let needs_confirm = match action {
            RowAction::Uninstall => true,
            RowAction::Update | RowAction::Stop => running,
            RowAction::Start => false,
        };
        if needs_confirm && self.confirm != Some((target.clone(), action)) {
            self.confirm = Some((target, action));
            cx.notify();
            return;
        }
        self.confirm = None;
        let Ok(parsed) = SshTarget::parse(&target) else {
            return;
        };
        if let Some(row) = self.rows.iter_mut().find(|r| r.saved.target == target) {
            row.busy = Some(action.busy_label());
        }
        let script = match action {
            RowAction::Update => scripts::update_script(),
            RowAction::Stop => scripts::stop_script(),
            RowAction::Start => scripts::start_script(),
            RowAction::Uninstall => scripts::uninstall_script(),
        };
        let timeout = match action {
            RowAction::Update => INSTALL_TIMEOUT,
            _ => SERVICE_TIMEOUT,
        };
        let runner = self.runner.clone();
        let label = parsed.to_string();
        let work = cx.background_spawn(async move {
            runner
                .run_script(&parsed, &script, timeout)
                .map(|out| out.stdout)
                .map_err(|err| scripts::install_failure_message(&err, &label))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |panel, cx| {
                if let Some(row) = panel.rows.iter_mut().find(|r| r.saved.target == target) {
                    row.busy = None;
                }
                match result {
                    Ok(stdout) => {
                        if action == RowAction::Uninstall {
                            panel.forget(&target, cx);
                            crate::toast::info(
                                cx,
                                "Uninstalled. Chats and settings stay in ~/.godspeed on the server; delete that folder there to remove everything.",
                            );
                        } else {
                            if action == RowAction::Start
                                && let Some(note) = StartOutcome::parse(&stdout)
                                    .and_then(|o| o.follow_up(parsed_user(&target).as_str()))
                            {
                                crate::toast::info(cx, note);
                            }
                            panel.set_row_status(&target, RowStatus::Checking);
                            panel.refresh_row(target.clone(), cx);
                        }
                    }
                    Err(message) => panel.error = Some(message.into()),
                }
                cx.notify();
            })
            .ok();
        }).detach();
        cx.notify();
    }

    fn forget(&mut self, target: &str, cx: &mut Context<Self>) {
        self.rows.retain(|r| r.saved.target != target);
        if let Some(dir) = self.data_dir(cx) {
            let mut saved = store::load(&dir);
            store::remove(&mut saved, target);
            if let Err(err) = store::save(&dir, &saved) {
                self.error = Some(format!("Couldn't update servers.json: {err}").into());
            }
        }
    }

    fn remember(&mut self, saved: SavedServer, probe: Option<ServerProbe>, cx: &mut Context<Self>) {
        if let Some(dir) = self.data_dir(cx) {
            let mut all = store::load(&dir);
            store::upsert(&mut all, saved.clone());
            if let Err(err) = store::save(&dir, &all) {
                self.error = Some(format!("Couldn't save the server: {err}").into());
            }
        }
        let status = probe.map(RowStatus::Ready).unwrap_or(RowStatus::Checking);
        match self
            .rows
            .iter_mut()
            .find(|r| r.saved.target == saved.target)
        {
            Some(row) => {
                row.saved = saved;
                row.status = status;
            }
            None => self.rows.push(ServerRow {
                saved,
                status,
                busy: None,
            }),
        }
    }

    // -- wizard -------------------------------------------------------------

    pub fn open_wizard(&mut self, cx: &mut Context<Self>) {
        let target_input = cx.new(|cx| ComposerInput::new("user@host", cx));
        let code_input = cx.new(|cx| ComposerInput::new("Code from the browser", cx));
        let events = vec![
            cx.subscribe(&target_input, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    this.test_connection(cx);
                }
            }),
            cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    this.submit_code(cx);
                }
            }),
        ];
        let cli = tailscale::find_cli(std::env::var_os("PATH").as_deref());
        let peers = if cli.is_some() {
            PeerList::Loading
        } else {
            PeerList::Unavailable
        };
        self.wizard = Some(Wizard {
            target_input,
            code_input,
            peers,
            step: Step::Target,
            target: None,
            probe: None,
            version: None,
            local_only: false,
            error: None,
            login: Arc::new(Mutex::new(None)),
            task: None,
            _events: events,
        });
        if let Some(cli) = cli {
            let list = cx.background_spawn(async move { tailscale::list_online_peers(&cli) });
            cx.spawn(async move |this, cx| {
                let result = list.await;
                this.update(cx, |panel, cx| {
                    if let Some(wizard) = panel.wizard.as_mut() {
                        wizard.peers = match result {
                            Ok(peers) => PeerList::Loaded(peers),
                            Err(message) => PeerList::Failed(message),
                        };
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn close_wizard(&mut self, cx: &mut Context<Self>) {
        if let Some(mut wizard) = self.wizard.take() {
            wizard.end_login();
        }
        cx.notify();
    }

    /// Escape closes the wizard (or a pending confirmation). Returns whether
    /// it consumed the key.
    pub fn dismiss_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.wizard.is_some() {
            self.close_wizard(cx);
            return true;
        }
        if self.confirm.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    fn pick_peer(&mut self, peer: TailscalePeer, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_ref() else {
            return;
        };
        let input = wizard.target_input.clone();
        let current = input.read(cx).text().to_owned();
        input.update(cx, |input, cx| {
            input.set_text(target_for_peer(&current, &peer), cx)
        });
        cx.notify();
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        if !matches!(wizard.step, Step::Target) {
            return;
        }
        let text = wizard.target_input.read(cx).text().to_owned();
        let target = match SshTarget::parse(&text) {
            Ok(target) => target,
            Err(err) => {
                wizard.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        wizard.error = None;
        wizard.step = Step::Checking;
        wizard.target = Some(target.clone());
        let probe = cx.background_spawn(async move {
            runner
                .run_script(&target, &scripts::probe_script(), PROBE_TIMEOUT)
                .map_err(|err| err.user_message(&target.to_string()))
                .and_then(|out| ServerProbe::parse(&out.stdout))
        });
        wizard.task = Some(cx.spawn(async move |this, cx| {
            let result = probe.await;
            this.update(cx, |panel, cx| {
                let Some(wizard) = panel.wizard.as_mut() else {
                    return;
                };
                match result.and_then(|probe| probe.supported_arch().map(|arch| (probe, arch))) {
                    Ok((probe, arch)) => {
                        wizard.probe = Some(probe);
                        wizard.step = Step::Ready { arch };
                    }
                    Err(message) => {
                        wizard.error = Some(message);
                        wizard.step = Step::Target;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn install(&mut self, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let (Step::Ready { arch }, Some(target)) = (&wizard.step, wizard.target.clone()) else {
            return;
        };
        let arch = *arch;
        wizard.error = None;
        wizard.step = Step::Installing;
        let manifest = Tokio::spawn(cx, async {
            zeron_update::fetch_latest_for(zeron_update::DEFAULT_FEED_URL, "linux").await
        });
        wizard.task = Some(cx.spawn(async move |this, cx| {
            let plan = match manifest.await {
                Ok(Ok(manifest)) => {
                    let file = zeron_remote::linux_artifact(&manifest.version, arch);
                    match manifest.files.get(&file) {
                        Some(meta) => Ok(InstallPlan::github(&manifest.version, arch, meta.sha256.clone())),
                        None if manifest.files.is_empty() => {
                            Ok(InstallPlan::github(&manifest.version, arch, None))
                        }
                        None => Err(format!(
                            "The latest release ({}) has no Linux {} build yet. Linux builds are uploaded about an hour after a release; try again later.",
                            manifest.version,
                            arch.as_str()
                        )),
                    }
                }
                Ok(Err(err)) => Err(format!("Couldn't read the release feed: {err:#}")),
                Err(err) => Err(format!("Couldn't read the release feed: {err}")),
            };
            let result = match plan {
                Ok(plan) => {
                    let version = plan.version.clone();
                    let label = target.to_string();
                    cx.background_spawn(async move {
                        runner
                            .run_script(&target, &scripts::install_script(&plan), INSTALL_TIMEOUT)
                            .map(|_| version)
                            .map_err(|err| scripts::install_failure_message(&err, &label))
                    })
                    .await
                }
                Err(message) => Err(message),
            };
            this.update(cx, |panel, cx| {
                match result {
                    Ok(version) => {
                        if let Some(wizard) = panel.wizard.as_mut() {
                            wizard.version = Some(version);
                        }
                        panel.after_install(cx);
                    }
                    Err(message) => {
                        if let Some(wizard) = panel.wizard.as_mut() {
                            wizard.error = Some(message);
                            wizard.step = Step::Ready { arch };
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn after_install(&mut self, cx: &mut Context<Self>) {
        let org = workspace_org(self.state.read(cx).auth.as_ref());
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let Some(probe) = wizard.probe.clone() else {
            return;
        };
        match after_install(org, &probe) {
            AfterInstall::Start { local_only } => {
                wizard.local_only = local_only;
                self.start_service(cx);
            }
            AfterInstall::SignIn { org, stop_first } => {
                if stop_first {
                    self.stop_then_sign_in(org, cx);
                } else {
                    self.begin_sign_in(org, cx);
                }
            }
        }
    }

    /// A signed-out engine is running: it holds the data-dir lock sign-in
    /// needs, so stop it first (it's restarted, signed in, right after).
    fn stop_then_sign_in(&mut self, org: String, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let Some(target) = wizard.target.clone() else {
            return;
        };
        wizard.step = Step::Stopping;
        let label = target.to_string();
        let work = cx.background_spawn(async move {
            runner
                .run_script(&target, &scripts::stop_script(), SERVICE_TIMEOUT)
                .map_err(|err| scripts::install_failure_message(&err, &label))
        });
        wizard.task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |panel, cx| {
                match result {
                    Ok(_) => panel.begin_sign_in(org, cx),
                    Err(message) => {
                        if let Some(wizard) = panel.wizard.as_mut() {
                            wizard.error = Some(message);
                            wizard.step = Step::SignIn {
                                url: None,
                                sending: false,
                            };
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn begin_sign_in(&mut self, org: String, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let Some(target) = wizard.target.clone() else {
            return;
        };
        wizard.end_login();
        wizard.error = None;
        wizard.step = Step::SignIn {
            url: None,
            sending: false,
        };
        let slot = wizard.login.clone();
        wizard.task = Some(cx.spawn(async move |this, cx| {
            let started = {
                let slot = slot.clone();
                cx.background_spawn(async move {
                    let session = LoginSession::start(&runner, &target, Some(&org))
                        .map_err(|err| err.user_message(&target.to_string()))?;
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
                    Ok::<(), String>(())
                })
                .await
            };
            if let Err(message) = started {
                this.update(cx, |panel, cx| {
                    if let Some(wizard) = panel.wizard.as_mut() {
                        wizard.error = Some(message);
                    }
                    cx.notify();
                })
                .ok();
                return;
            }
            loop {
                let poll = slot.clone();
                let event = cx
                    .background_spawn(async move {
                        let mut guard = poll.lock().unwrap_or_else(|e| e.into_inner());
                        guard
                            .as_mut()
                            .map(|session| session.next_event(Duration::from_millis(300)))
                    })
                    .await;
                let Some(event) = event else {
                    return; // cancelled
                };
                let Some(event) = event else {
                    continue;
                };
                let finished = matches!(
                    event,
                    LoginEvent::SignedIn { .. } | LoginEvent::NotNeeded | LoginEvent::Failed(_)
                );
                let alive = this
                    .update(cx, |panel, cx| {
                        panel.on_login_event(event, cx);
                        cx.notify();
                    })
                    .is_ok();
                if finished || !alive {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn on_login_event(&mut self, event: LoginEvent, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        match event {
            LoginEvent::Url(url) => {
                cx.open_url(&url);
                wizard.step = Step::SignIn {
                    url: Some(url),
                    sending: false,
                };
            }
            LoginEvent::CodeRejected(reason) => {
                wizard.error = Some(format!(
                    "The server didn't accept that code ({reason}). Codes work once and expire after a few minutes: sign in again and paste the new one."
                ));
                if let Step::SignIn { sending, .. } = &mut wizard.step {
                    *sending = false;
                }
            }
            LoginEvent::SignedIn { .. } | LoginEvent::NotNeeded => {
                wizard.end_login();
                wizard.local_only = false;
                self.start_service(cx);
            }
            LoginEvent::Failed(message) => {
                wizard.end_login();
                wizard.error = Some(message);
                wizard.step = Step::SignIn {
                    url: None,
                    sending: false,
                };
            }
        }
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let Step::SignIn {
            url: Some(_),
            sending,
        } = &mut wizard.step
        else {
            return;
        };
        if *sending {
            return;
        }
        let code = wizard.code_input.read(cx).text().trim().to_owned();
        if !zeron_remote::login::valid_code(&code) {
            wizard.error =
                Some("Paste the whole code the browser shows (it contains a dot).".into());
            cx.notify();
            return;
        }
        *sending = true;
        wizard.error = None;
        wizard
            .code_input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        let slot = wizard.login.clone();
        let send = cx.background_spawn(async move {
            match slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                Some(session) => session.send_code(&code),
                None => Err("The sign-in has ended; start it again.".into()),
            }
        });
        cx.spawn(async move |this, cx| {
            if let Err(message) = send.await {
                this.update(cx, |panel, cx| {
                    if let Some(wizard) = panel.wizard.as_mut() {
                        wizard.error = Some(message);
                        if let Step::SignIn { sending, .. } = &mut wizard.step {
                            *sending = false;
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn retry_sign_in(&mut self, cx: &mut Context<Self>) {
        match workspace_org(self.state.read(cx).auth.as_ref()) {
            Some(org) => self.begin_sign_in(org, cx),
            None => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.local_only = true;
                }
                self.start_service(cx);
            }
        }
    }

    fn start_service(&mut self, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let Some(target) = wizard.target.clone() else {
            return;
        };
        wizard.error = None;
        wizard.step = Step::Starting;
        let label = target.to_string();
        let work = cx.background_spawn(async move {
            let started = runner
                .run_script(&target, &scripts::start_script(), SERVICE_TIMEOUT)
                .map_err(|err| scripts::install_failure_message(&err, &label))?;
            let outcome = StartOutcome::parse(&started.stdout)
                .ok_or_else(|| "The server didn't confirm the engine started.".to_owned())?;
            // Fresh status for the saved row (best effort).
            let probe = runner
                .run_script(&target, &scripts::probe_script(), PROBE_TIMEOUT)
                .ok()
                .and_then(|out| ServerProbe::parse(&out.stdout).ok());
            Ok::<_, String>((outcome, probe))
        });
        wizard.task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |panel, cx| {
                let Some(wizard) = panel.wizard.as_mut() else {
                    return;
                };
                match result {
                    Ok((outcome, probe)) => {
                        let first = wizard.probe.clone();
                        let host = first
                            .as_ref()
                            .and_then(|p| p.hostname.clone())
                            .unwrap_or_else(|| wizard.target_label());
                        let version = wizard
                            .version
                            .clone()
                            .or_else(|| probe.as_ref().and_then(|p| p.installed.clone()))
                            .unwrap_or_default();
                        let user = wizard
                            .target
                            .as_ref()
                            .and_then(|t| t.user.clone())
                            .or_else(|| first.as_ref().and_then(|p| p.user.clone()))
                            .unwrap_or_else(|| "$USER".into());
                        wizard.step = Step::Done {
                            summary: done_summary(&host, &version, &outcome, wizard.local_only),
                            follow_up: outcome.follow_up(&user),
                        };
                        let saved = SavedServer {
                            target: wizard.target_label(),
                            hostname: first.and_then(|p| p.hostname),
                            version: (!version.is_empty()).then_some(version),
                            added_at_ms: chrono::Utc::now().timestamp_millis(),
                        };
                        panel.remember(saved, probe, cx);
                    }
                    Err(message) => {
                        wizard.error = Some(message);
                        wizard.step = Step::SignIn {
                            url: None,
                            sending: false,
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // -- rendering ------------------------------------------------------------

    fn render_rows(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut block = widgets::section_card(theme).mt(px(8.0));
        let latest = self.latest_linux.clone();
        let confirm = self.confirm.clone();
        for (ix, row) in self.rows.iter().enumerate() {
            let target = row.saved.target.clone();
            let mut meta: Vec<AnyElement> = Vec::new();
            let mut actions: Vec<RowAction> = Vec::new();
            match &row.status {
                RowStatus::Checking => meta.push(
                    div()
                        .child(SharedString::from("Checking…"))
                        .into_any_element(),
                ),
                RowStatus::Unreachable(message) => meta.push(
                    div()
                        .text_color(theme.danger_muted)
                        .child(SharedString::from(message.clone()))
                        .into_any_element(),
                ),
                RowStatus::Ready(probe) => {
                    for (n, fragment) in status_fragments(probe, latest.as_deref())
                        .into_iter()
                        .enumerate()
                    {
                        let el = div().child(SharedString::from(fragment));
                        meta.push(if n == 0 && probe.running {
                            el.text_color(theme.success_muted).into_any_element()
                        } else {
                            el.into_any_element()
                        });
                    }
                    if probe.installed.is_some() {
                        actions.push(RowAction::Update);
                        actions.push(if probe.running {
                            RowAction::Stop
                        } else {
                            RowAction::Start
                        });
                    }
                    actions.push(RowAction::Uninstall);
                }
            }
            if let Some(busy) = row.busy {
                meta.push(div().child(SharedString::from(busy)).into_any_element());
            }
            let title = match &row.saved.hostname {
                Some(host) => format!("{host} · {target}"),
                None => target.clone(),
            };
            let mut buttons = div().flex_none().flex().items_center().gap(px(4.0));
            if row.busy.is_none() {
                for (n, action) in actions.into_iter().enumerate() {
                    let confirming = confirm.as_ref() == Some(&(target.clone(), action));
                    let (label, tone) = match (action, confirming) {
                        (RowAction::Update, false) => ("Update", widgets::ActionTone::Quiet),
                        (RowAction::Update, true) => {
                            ("Restart and update", widgets::ActionTone::Danger)
                        }
                        (RowAction::Stop, false) => ("Stop", widgets::ActionTone::Quiet),
                        (RowAction::Stop, true) => {
                            ("Stop running agents", widgets::ActionTone::Danger)
                        }
                        (RowAction::Start, _) => ("Start", widgets::ActionTone::Filled),
                        (RowAction::Uninstall, false) => ("Uninstall", widgets::ActionTone::Quiet),
                        (RowAction::Uninstall, true) => {
                            ("Really uninstall", widgets::ActionTone::Danger)
                        }
                    };
                    let target = target.clone();
                    buttons = buttons.child(
                        widgets::text_action(theme, tone, label)
                            .id(("server-action", ix * 8 + n))
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .aria_label(format!("{label} {target}"))
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_action(target.clone(), action, cx);
                            })),
                    );
                }
            }
            block = block.child(
                widgets::card_row(theme, ix == 0)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(theme, title))
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .child(buttons),
            );
        }
        block = block.child(
            widgets::card_row(theme, self.rows.is_empty())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(160.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "Keep agents running while this computer sleeps: install Godspeed on a Linux server over SSH.",
                        )),
                )
                .child(
                    widgets::text_action(theme, widgets::ActionTone::Filled, "Run on a server…")
                        .id("run-on-server")
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(|this, _, _, cx| this.open_wizard(cx))),
                ),
        );
        block.into_any_element()
    }

    fn render_wizard(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let wizard = self.wizard.as_ref()?;
        let target = wizard.target_label();
        let mut body = div().mt(px(12.0)).flex().flex_col().gap(px(10.0));
        let mut buttons: Vec<AnyElement> = Vec::new();
        let button = |id: &'static str, label: &str, tone: widgets::ActionTone| {
            widgets::text_action(&theme, tone, label.to_owned())
                .id(id)
                .tab_index(0)
                .role(gpui::Role::Button)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
        };
        let cancel = button("server-wizard-cancel", "Cancel", widgets::ActionTone::Quiet)
            .on_click(cx.listener(|this, _, _, cx| this.close_wizard(cx)))
            .into_any_element();
        match &wizard.step {
            Step::Target => {
                body = body
                    .child(popover::dialog_body(
                        &theme,
                        "Godspeed connects with your own SSH setup (keys, agent and ~/.ssh/config) and never asks for a password. The server needs Linux (x86_64 or arm64); nothing is installed outside your home folder.",
                    ))
                    .child(popover::dialog_field(
                        wizard.target_input.clone().into_any_element(),
                    ));
                match &wizard.peers {
                    PeerList::Unavailable => {}
                    PeerList::Loading => {
                        body = body.child(popover::dialog_body(
                            &theme,
                            "Looking for Tailscale machines…",
                        ));
                    }
                    PeerList::Failed(message) => {
                        body = body.child(popover::dialog_body(
                            &theme,
                            format!("Tailscale: {message}"),
                        ));
                    }
                    PeerList::Loaded(peers) if peers.is_empty() => {
                        body = body.child(popover::dialog_body(
                            &theme,
                            "No other Tailscale machines are online.",
                        ));
                    }
                    PeerList::Loaded(peers) => {
                        let mut chips = div().flex().flex_row().flex_wrap().gap(px(6.0));
                        for (ix, peer) in peers.iter().enumerate() {
                            let label = if peer.os.is_empty() {
                                peer.name.clone()
                            } else {
                                format!("{} · {}", peer.name, peer.os)
                            };
                            let pick = peer.clone();
                            chips = chips.child(
                                widgets::text_action(&theme, widgets::ActionTone::Outlined, label)
                                    .id(("tailscale-peer", ix))
                                    .tab_index(0)
                                    .role(gpui::Role::Button)
                                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.pick_peer(pick.clone(), cx)
                                    })),
                            );
                        }
                        body = body
                            .child(widgets::field_label(&theme, "Tailscale machines"))
                            .child(chips);
                    }
                }
                buttons.push(cancel);
                buttons.push(
                    button(
                        "server-wizard-test",
                        "Test connection",
                        widgets::ActionTone::Solid,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx)))
                    .into_any_element(),
                );
            }
            Step::Checking => {
                body = body.child(popover::dialog_body(
                    &theme,
                    format!("Connecting to {target}…"),
                ));
                buttons.push(cancel);
            }
            Step::Ready { .. } => {
                if let Some(probe) = &wizard.probe {
                    body = body.child(popover::dialog_body(
                        &theme,
                        format!("Connected to {target}: {}.", probe.summary()),
                    ));
                    if let Some(warning) = probe.glibc_warning() {
                        body = body.child(widgets::warning_strip(&theme, warning));
                    }
                    if probe.running {
                        body = body.child(popover::dialog_body(
                            &theme,
                            "An engine is already running there. Installing doesn't restart it; it moves to the new version by itself once it's idle.",
                        ));
                    }
                }
                body = body.child(popover::dialog_body(
                    &theme,
                    "Next: download the latest Godspeed release from GitHub on the server, check its checksum, install it into ~/.godspeed/app, sign it in to your workspace and start it as a user service.",
                ));
                buttons.push(cancel);
                buttons.push(
                    button(
                        "server-wizard-install",
                        "Install Godspeed",
                        widgets::ActionTone::Solid,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.install(cx)))
                    .into_any_element(),
                );
            }
            Step::Installing => {
                body = body.child(popover::dialog_body(
                    &theme,
                    format!("Downloading and verifying Godspeed on {target}. This takes a minute…"),
                ));
                buttons.push(cancel);
            }
            Step::Stopping => {
                body = body.child(popover::dialog_body(
                    &theme,
                    "Stopping the signed-out engine on the server so it can sign in…",
                ));
            }
            Step::SignIn { url: None, .. } if wizard.error.is_some() => {
                buttons.push(cancel);
                buttons.push(
                    button(
                        "server-wizard-retry",
                        "Try again",
                        widgets::ActionTone::Solid,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.retry_sign_in(cx)))
                    .into_any_element(),
                );
            }
            Step::SignIn { url: None, .. } => {
                body = body.child(popover::dialog_body(
                    &theme,
                    "Starting sign-in on the server…",
                ));
                buttons.push(cancel);
            }
            Step::SignIn {
                url: Some(url),
                sending,
            } => {
                let url = url.clone();
                body = body
                    .child(popover::dialog_body(
                        &theme,
                        "Sign the server in to your workspace: sign in in the browser window that opened, then paste the code it shows here. The server exchanges the code itself; no token leaves this computer.",
                    ))
                    .child(popover::dialog_field(
                        wizard.code_input.clone().into_any_element(),
                    ));
                buttons.push(cancel);
                buttons.push(
                    button(
                        "server-wizard-open",
                        "Open sign-in page",
                        widgets::ActionTone::Quiet,
                    )
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .into_any_element(),
                );
                buttons.push(
                    button(
                        "server-wizard-code",
                        if *sending {
                            "Signing in…"
                        } else {
                            "Finish sign-in"
                        },
                        widgets::ActionTone::Solid,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx)))
                    .into_any_element(),
                );
            }
            Step::Starting => {
                body = body.child(popover::dialog_body(
                    &theme,
                    "Starting the engine on the server…",
                ));
            }
            Step::Done { summary, follow_up } => {
                body = body.child(popover::dialog_body(&theme, summary.clone()));
                if let Some(note) = follow_up {
                    body = body.child(widgets::warning_strip(&theme, note.clone()));
                }
                buttons.push(
                    button("server-wizard-done", "Done", widgets::ActionTone::Solid)
                        .on_click(cx.listener(|this, _, _, cx| this.close_wizard(cx)))
                        .into_any_element(),
                );
            }
        }
        if let Some(error) = &wizard.error {
            body = body.child(widgets::error_strip(&theme, error.clone()).mt(px(0.0)));
        }
        let card = popover::dialog_card(&theme)
            .w(px(480.0))
            .id("server-wizard-card")
            .role(gpui::Role::Dialog)
            .aria_label("Run on a server")
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close_wizard(cx);
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(&theme, "Run on a server"))
            .child(body)
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .children(buttons),
            )
            .into_any_element();
        Some(popover::modal("server-wizard", viewport, card))
    }
}

fn parsed_user(target: &str) -> String {
    SshTarget::parse(target)
        .ok()
        .and_then(|t| t.user)
        .unwrap_or_else(|| "$USER".into())
}

impl Render for ServersPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let rows = self.render_rows(&theme, cx);
        let wizard = self.render_wizard(window.viewport_size(), cx);
        div()
            .flex()
            .flex_col()
            .child(widgets::section_label(&theme, "Servers").mt(px(28.0)))
            .child(rows)
            .when_some(wizard, |el, wizard| el.child(wizard))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::UserProfile;

    fn probe(text: &str) -> ServerProbe {
        ServerProbe::parse(text).unwrap()
    }

    const BASE: &str =
        "os=Linux\narch=x86_64\nglibc=2.39\ncurl=yes\nsha256=yes\nsystemd_user=yes\n";

    #[test]
    fn the_server_joins_the_signed_in_workspace() {
        let user = UserProfile {
            id: "u".into(),
            email: "me@example.com".into(),
            name: None,
        };
        assert_eq!(
            workspace_org(Some(&AuthState::SignedIn {
                user: user.clone(),
                org_id: Some("org_1".into())
            })),
            Some("org_1".into())
        );
        assert_eq!(
            workspace_org(Some(&AuthState::SignedIn {
                user: user.clone(),
                org_id: None
            })),
            None
        );
        assert_eq!(
            workspace_org(Some(&AuthState::NeedsOrganization { user })),
            None
        );
        assert_eq!(workspace_org(Some(&AuthState::SignedOut)), None);
        assert_eq!(workspace_org(None), None);
    }

    #[test]
    fn after_install_signs_in_once_and_never_clobbers_a_signed_in_engine() {
        let fresh = probe(&format!("{BASE}running=no\nsession=no\n"));
        assert_eq!(
            after_install(Some("org_1".into()), &fresh),
            AfterInstall::SignIn {
                org: "org_1".into(),
                stop_first: false
            }
        );
        let running_signed_out = probe(&format!("{BASE}running=yes\nsession=no\n"));
        assert_eq!(
            after_install(Some("org_1".into()), &running_signed_out),
            AfterInstall::SignIn {
                org: "org_1".into(),
                stop_first: true
            }
        );
        // Re-running the wizard against a signed-in, running server: just
        // the (no-op) start, nothing stops.
        let running_signed_in = probe(&format!("{BASE}running=yes\nsession=yes\n"));
        assert_eq!(
            after_install(Some("org_1".into()), &running_signed_in),
            AfterInstall::Start { local_only: false }
        );
        assert_eq!(
            after_install(None, &fresh),
            AfterInstall::Start { local_only: true }
        );
        assert_eq!(
            after_install(None, &running_signed_in),
            AfterInstall::Start { local_only: false }
        );
    }

    #[test]
    fn row_status_shows_online_version_uptime_and_updates() {
        let online = probe(&format!(
            "{BASE}installed=0.5.9\nrunning=yes\nuptime=7500\nsession=yes\n"
        ));
        assert_eq!(
            status_fragments(&online, Some("0.6.0")),
            vec!["Online", "v0.5.9", "up 2h 5m", "0.6.0 available"]
        );
        assert_eq!(
            status_fragments(&online, Some("0.5.9")),
            vec!["Online", "v0.5.9", "up 2h 5m"]
        );
        let stopped = probe(&format!("{BASE}installed=0.5.9\nrunning=no\nuptime=\n"));
        assert_eq!(status_fragments(&stopped, None), vec!["Stopped", "v0.5.9"]);
        let bare = probe(&format!("{BASE}installed=\nrunning=no\n"));
        assert_eq!(
            status_fragments(&bare, Some("0.6.0")),
            vec!["Not installed"]
        );
    }

    #[test]
    fn tailscale_peers_fill_the_host_and_keep_the_user() {
        let peer = TailscalePeer {
            name: "box".into(),
            dns_name: Some("box.tail1.ts.net".into()),
            ip: Some("100.64.0.2".into()),
            os: "linux".into(),
            online: true,
        };
        assert_eq!(target_for_peer("", &peer), "box.tail1.ts.net");
        assert_eq!(
            target_for_peer("deploy@old", &peer),
            "deploy@box.tail1.ts.net"
        );
        assert_eq!(target_for_peer("@x", &peer), "box.tail1.ts.net");
    }

    #[test]
    fn done_summaries() {
        let started = StartOutcome {
            systemd: true,
            already_running: false,
            linger: Some(true),
            reboot_cron: None,
        };
        let text = done_summary("box", "0.5.9", &started, false);
        assert!(text.contains("Godspeed 0.5.9 is running on box as a systemd user service"));
        assert!(text.contains("Other devices"));
        let again = StartOutcome {
            already_running: true,
            systemd: false,
            ..started
        };
        let text = done_summary("box", "0.5.9", &again, true);
        assert!(text.contains("was already running"));
        assert!(text.contains("background process"));
        assert!(text.contains("local-only"));
    }
}
