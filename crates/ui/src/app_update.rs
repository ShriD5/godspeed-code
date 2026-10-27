//! Desktop self-update: the app's own release checker plus the download /
//! install lifecycle behind the sidebar update strip, "Check for Updates…"
//! (macOS app menu; the account menu elsewhere) and install-on-quit.
//!
//! The checker runs in this process against this binary's version. The
//! engine keeps its own checker for daemons and remote reporting, but the UI
//! may be attached to a daemon of another version, whose status describes
//! that binary rather than the app the user is looking at.
//!
//! Lifecycle, idiomatic to desktop updaters (Sparkle, VS Code, Zed): a found
//! release downloads in the background and is verified, the strip offers
//! "restart to apply", and a staged update the user never restarts for is
//! installed when the app quits — so nobody stays on a stale version just
//! because they never clicked. Settings → General "Check for updates
//! automatically" (on by default) turns the scheduled checks, background
//! download and install-on-quit off together; `GODSPEED_AUTO_UPDATE=0` keeps
//! the download and install report-only regardless. "Check now" and the menu
//! item always check.

use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use gpui_tokio::Tokio;
use zeron_update::{InstallKind, UpdateBlocker, UpdateStatus, Updater};

/// Download/install lifecycle of the newest release in this process.
#[derive(Debug, Clone, PartialEq)]
pub enum Flow {
    Idle,
    Downloading {
        version: String,
    },
    /// Staged and verified: "restart to apply", or installed on quit.
    Ready {
        version: String,
        staged: PathBuf,
    },
    Failed {
        version: String,
        message: SharedString,
    },
    /// Swapped in; this process is on its way out.
    Installed,
}

/// The user-initiated check's dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    Checking,
    /// The check finished; the dialog renders from the live status + flow.
    Result,
    CheckFailed(SharedString),
}

/// What clicking the sidebar strip does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripAction {
    /// Nothing — progress only.
    None,
    Download,
    Restart,
    /// Explain why this install can't update itself (dialog).
    Explain,
    /// Advisory installs: open the releases page (unmanaged) and dismiss.
    Advise {
        open_releases: bool,
    },
}

pub struct AppUpdate {
    install: InstallKind,
    blocker: Option<UpdateBlocker>,
    /// Background download + install on quit: the Settings toggle, unless
    /// `GODSPEED_AUTO_UPDATE` turns it off.
    automatic: bool,
    /// Settings → General "Check for updates automatically".
    automatic_checks: bool,
    feed: String,
    data_dir: PathBuf,
    checker: Updater,
    status: Option<UpdateStatus>,
    flow: Flow,
    prompt: Option<Prompt>,
    /// Version whose advisory strip the user dismissed (a newer release shows
    /// it again).
    dismissed: Option<String>,
    _status_watch: Task<()>,
    download: Option<Task<()>>,
    user_check: Option<Task<()>>,
}

struct GlobalAppUpdate(Entity<AppUpdate>);

impl Global for GlobalAppUpdate {}

impl AppUpdate {
    /// Start the desktop checker. Call once at boot, after `gpui_tokio` is
    /// initialized.
    pub fn init(feed: String, automatic_checks: bool, data_dir: PathBuf, cx: &mut App) {
        let entity = cx.new(|cx| Self::new(feed, automatic_checks, data_dir, cx));
        cx.set_global(GlobalAppUpdate(entity));
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAppUpdate>()
            .map(|global| global.0.clone())
    }

    fn new(
        feed: String,
        automatic_checks: bool,
        data_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let install = zeron_update::detect_install();
        let blocker = install
            .supports_desktop_update()
            .then(|| install.desktop_update_blocker())
            .flatten();
        let checker = {
            let _runtime = Tokio::handle(cx).enter();
            Updater::spawn_desktop(feed.clone())
        };
        checker.set_scheduled_checks(automatic_checks);
        let mut statuses = checker.watch();
        let status_watch = cx.spawn(async move |this, cx| {
            while statuses.changed().await.is_ok() {
                let status = statuses.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| this.apply_status(status, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        tracing::info!(?install, ?blocker, "desktop update checker started");
        Self {
            install,
            blocker,
            automatic: automatic_checks && zeron_update::desktop_auto_update_enabled(),
            automatic_checks,
            feed,
            data_dir,
            checker,
            status: None,
            flow: Flow::Idle,
            prompt: None,
            dismissed: None,
            _status_watch: status_watch,
            download: None,
            user_check: None,
        }
    }

    pub fn install(&self) -> &InstallKind {
        &self.install
    }

    pub fn blocker(&self) -> Option<&UpdateBlocker> {
        self.blocker.as_ref()
    }

    pub fn flow(&self) -> &Flow {
        &self.flow
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// The newer release the last check found, if any.
    pub fn available(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|status| status.update_available)
            .and_then(|status| status.latest_version.as_deref())
    }

    /// Whether this app downloads and installs updates itself right now.
    pub fn self_updating(&self) -> bool {
        self.install.supports_desktop_update() && self.blocker.is_none()
    }

    /// The last check's result (Settings → General shows when it ran).
    pub fn status(&self) -> Option<&UpdateStatus> {
        self.status.as_ref()
    }

    pub fn automatic_checks(&self) -> bool {
        self.automatic_checks
    }

    /// Settings → General "Check for updates automatically": scheduled
    /// checks, background download and install-on-quit follow it together.
    pub fn set_automatic_checks(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.automatic_checks = enabled;
        self.automatic = enabled && zeron_update::desktop_auto_update_enabled();
        self.checker.set_scheduled_checks(enabled);
        if self.automatic {
            self.download_if_needed(cx);
        }
        cx.notify();
    }

    /// Window activation / wake: check if one is due by the wall clock.
    pub fn poke(&self) {
        self.checker.poke();
    }

    fn apply_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        let succeeded = status.error.is_none();
        self.status = Some(status);
        if succeeded && self.automatic {
            self.download_if_needed(cx);
        }
        cx.notify();
    }

    /// Start (or restart) the background download unless the newest release
    /// is already downloading or staged. A failed download retries at the
    /// next successful check.
    fn download_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.self_updating() {
            return;
        }
        let Some(latest) = self.available() else {
            return;
        };
        let current = match &self.flow {
            Flow::Downloading { .. } | Flow::Installed => return,
            Flow::Ready { version, .. } => Some(version),
            Flow::Idle | Flow::Failed { .. } => None,
        };
        if current.is_some_and(|version| !zeron_update::version_newer(latest, version)) {
            return;
        }
        self.start_download(cx);
    }

    pub fn start_download(&mut self, cx: &mut Context<Self>) {
        if !self.self_updating() || matches!(self.flow, Flow::Downloading { .. }) {
            return;
        }
        let Some(version) = self.available().map(str::to_owned) else {
            return;
        };
        let feed = self.feed.clone();
        let data_dir = self.data_dir.clone();
        let install = self.install.clone();
        self.flow = Flow::Downloading {
            version: version.clone(),
        };
        let stage = Tokio::spawn(cx, async move {
            // Re-read the manifest so a long-lived status still downloads the
            // newest release, with that release's checksums.
            let manifest = zeron_update::fetch_latest(&feed).await?;
            anyhow::ensure!(
                zeron_update::version_newer(&manifest.version, zeron_update::current_version()),
                "the release feed no longer offers a newer version"
            );
            let staged = install
                .stage_desktop(&feed, &manifest, &data_dir)
                .await?;
            anyhow::Ok((manifest.version, staged))
        });
        self.download = Some(cx.spawn(async move |this, cx| {
            let outcome = match stage.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join) => Err(join.to_string()),
            };
            this.update(cx, |this, cx| {
                this.flow = match outcome {
                    Ok((version, staged)) => {
                        tracing::info!(%version, staged = %staged.display(), "update staged");
                        crate::toast::info(cx, ready_label(&version));
                        Flow::Ready { version, staged }
                    }
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        Flow::Failed {
                            version,
                            message: message.into(),
                        }
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// "Check for Updates…": check now and report the outcome in a dialog. A
    /// found release starts downloading at once (it would anyway on the
    /// hourly check), and a failed download gets a fresh attempt.
    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.prompt = Some(Prompt::Checking);
        let checker = self.checker.clone();
        let check = Tokio::spawn(cx, async move { checker.check().await });
        self.user_check = Some(cx.spawn(async move |this, cx| {
            let result = match check.await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(err)) => {
                    tracing::warn!(error = %format!("{err:#}"), "update check failed");
                    // The dialog shows the cause; the log keeps the chain.
                    Err(err.root_cause().to_string())
                }
                Err(join) => Err(join.to_string()),
            };
            this.update(cx, |this, cx| {
                let outcome = match result {
                    Ok(status) => {
                        this.status = Some(status);
                        if this.automatic {
                            this.download_if_needed(cx);
                        }
                        Prompt::Result
                    }
                    Err(message) => Prompt::CheckFailed(message.into()),
                };
                // The user may have closed the dialog while it was checking.
                if this.prompt.is_some() {
                    this.prompt = Some(outcome);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub fn dismiss_prompt(&mut self, cx: &mut Context<Self>) {
        self.prompt = None;
        self.user_check = None;
        cx.notify();
    }

    /// Open the dialog on its current result (the strip's "explain" action).
    pub fn show_result(&mut self, cx: &mut Context<Self>) {
        self.prompt = Some(Prompt::Result);
        cx.notify();
    }

    /// The staged update "Restart to update" would install.
    pub fn staged(&self) -> Option<PathBuf> {
        match &self.flow {
            Flow::Ready { staged, .. } => Some(staged.clone()),
            _ => None,
        }
    }

    /// Install `staged` and arrange the relaunch. The caller quits on success.
    pub fn install_for_restart(
        &mut self,
        staged: &Path,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let result = self.install.apply_desktop(staged, true);
        self.flow = match &result {
            Ok(()) => Flow::Installed,
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                Flow::Failed {
                    version: self.available().unwrap_or_default().to_owned(),
                    message: format!("{err:#}").into(),
                }
            }
        };
        cx.notify();
        result
    }

    /// Quit hook: a staged update the user never restarted for installs now,
    /// so the next launch is current. Runs synchronously inside the quit.
    fn install_on_quit(&mut self) {
        if !self.automatic {
            return;
        }
        let Flow::Ready { version, staged } = &self.flow else {
            return;
        };
        match self.install.apply_desktop(staged, false) {
            Ok(()) => {
                tracing::info!(%version, "installed staged update on quit");
                self.flow = Flow::Installed;
            }
            Err(err) => tracing::warn!(error = %err, "installing the staged update on quit failed"),
        }
    }

    pub fn dismiss_advisory(&mut self, cx: &mut Context<Self>) {
        self.dismissed = self.available().map(str::to_owned);
        cx.notify();
    }

    /// The sidebar strip for the current state: `None` while there is
    /// nothing newer (or the advisory was dismissed for this version).
    pub fn strip(&self) -> Option<(SharedString, StripAction)> {
        let latest = self.available()?;
        if self.dismissed.as_deref() == Some(latest) {
            return None;
        }
        Some(strip_for(
            &self.install,
            self.blocker.is_some(),
            &self.flow,
            latest,
        ))
    }
}

/// Label + click action of the strip. Self-updating installs drive their flow
/// from it; blocked installs explain themselves; managed installs without a
/// desktop path get the `godspeed update` hint; unmanaged installs (source builds,
/// hand-copied binaries) are pointed at the GitHub releases page.
pub fn strip_for(
    install: &InstallKind,
    blocked: bool,
    flow: &Flow,
    latest: &str,
) -> (SharedString, StripAction) {
    if install.supports_desktop_update() {
        if blocked {
            return (
                format!("Update available — v{latest}").into(),
                StripAction::Explain,
            );
        }
        return match flow {
            Flow::Idle => (
                format!("Update available — v{latest}").into(),
                StripAction::Download,
            ),
            Flow::Downloading { version } => {
                (format!("Downloading v{version}…").into(), StripAction::None)
            }
            Flow::Ready { version, .. } => (
                format!("Godspeed {version} is ready").into(),
                StripAction::Restart,
            ),
            Flow::Failed { message, .. } => (
                format!("Update failed: {message}").into(),
                StripAction::Download,
            ),
            Flow::Installed => ("Restarting…".into(), StripAction::None),
        };
    }
    if matches!(install, InstallKind::Managed { .. }) {
        (
            format!("Update available — v{latest} · run `godspeed update`").into(),
            StripAction::Advise {
                open_releases: false,
            },
        )
    } else {
        (
            format!("Update available — v{latest} · download from GitHub").into(),
            StripAction::Advise {
                open_releases: true,
            },
        )
    }
}

/// Settings → General's line under "Check for updates automatically": the
/// update in flight if any, else the running version and when the last check
/// ran. `now_ms` is epoch milliseconds. Pure.
pub fn settings_detail(
    current: &str,
    status: Option<&UpdateStatus>,
    flow: &Flow,
    now_ms: i64,
) -> String {
    match flow {
        Flow::Ready { version, .. } => return format!("Godspeed {version} is ready — restart to update."),
        Flow::Downloading { version } => return format!("Downloading Godspeed {version}…"),
        Flow::Failed { message, .. } => return format!("The last update failed: {message}"),
        Flow::Installed => return "Restarting…".to_owned(),
        Flow::Idle => {}
    }
    let Some(status) = status else {
        return format!("You're on {current}.");
    };
    if status.update_available
        && let Some(latest) = &status.latest_version
    {
        return format!("Godspeed {latest} is available. You're on {current}.");
    }
    match (status.checked_at, &status.error) {
        (_, Some(_)) => format!("You're on {current}. The last check failed."),
        (Some(at), None) => format!(
            "You're on {current}, the newest release. Checked {}.",
            ago(now_ms.saturating_sub(at))
        ),
        (None, None) => format!("You're on {current}."),
    }
}

fn ago(elapsed_ms: i64) -> String {
    let minutes = elapsed_ms.max(0) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => format!("{} d ago", minutes / 1440),
    }
}

/// The one-time toast when an update finishes staging.
pub fn ready_label(version: &str) -> String {
    format!("Godspeed {version} is ready — Restart to update")
}

/// "Check for Updates…" from a menu: surface the main window, then check.
pub fn check_for_updates(cx: &mut App) {
    crate::activate_main_window(cx);
    if let Some(update) = AppUpdate::global(cx) {
        update.update(cx, |update, cx| update.check_for_updates(cx));
    }
}

/// See [`AppUpdate::install_on_quit`].
pub fn install_on_quit(cx: &mut App) {
    if let Some(update) = AppUpdate::global(cx) {
        update.update(cx, |update, _| update.install_on_quit());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_follows_the_flow_on_self_updating_installs() {
        let mac_app = InstallKind::MacApp {
            bundle: PathBuf::from("/Applications/Godspeed.app"),
        };
        assert_eq!(
            strip_for(&mac_app, false, &Flow::Idle, "0.2.86"),
            (
                SharedString::from("Update available — v0.2.86"),
                StripAction::Download
            )
        );
        let downloading = Flow::Downloading {
            version: "0.2.86".into(),
        };
        assert_eq!(
            strip_for(&mac_app, false, &downloading, "0.2.86"),
            (
                SharedString::from("Downloading v0.2.86…"),
                StripAction::None
            )
        );
        let ready = Flow::Ready {
            version: "0.2.86".into(),
            staged: PathBuf::from("/tmp/Godspeed.app"),
        };
        assert_eq!(
            strip_for(&mac_app, false, &ready, "0.2.86"),
            (
                SharedString::from("Godspeed 0.2.86 is ready"),
                StripAction::Restart
            )
        );
        assert_eq!(
            ready_label("0.5.8"),
            "Godspeed 0.5.8 is ready — Restart to update"
        );
        let failed = Flow::Failed {
            version: "0.2.86".into(),
            message: "offline".into(),
        };
        assert_eq!(
            strip_for(&mac_app, false, &failed, "0.2.86"),
            (
                SharedString::from("Update failed: offline"),
                StripAction::Download
            )
        );
        // A translocated or read-only bundle explains instead of failing.
        assert_eq!(
            strip_for(&mac_app, true, &Flow::Idle, "0.2.86").1,
            StripAction::Explain
        );
    }

    #[test]
    fn settings_detail_reports_the_flow_then_the_last_check() {
        let now = 10 * 60 * 60 * 1000;
        let checked = |minutes_ago: i64, latest: &str, available: bool| UpdateStatus {
            current_version: "0.5.6".into(),
            latest_version: Some(latest.into()),
            update_available: available,
            checked_at: Some(now - minutes_ago * 60_000),
            error: None,
        };
        assert_eq!(
            settings_detail("0.5.6", None, &Flow::Idle, now),
            "You're on 0.5.6."
        );
        assert_eq!(
            settings_detail("0.5.6", Some(&checked(0, "0.5.6", false)), &Flow::Idle, now),
            "You're on 0.5.6, the newest release. Checked just now."
        );
        assert_eq!(
            settings_detail("0.5.6", Some(&checked(125, "0.5.6", false)), &Flow::Idle, now),
            "You're on 0.5.6, the newest release. Checked 2 h ago."
        );
        assert_eq!(
            settings_detail("0.5.6", Some(&checked(3, "0.5.8", true)), &Flow::Idle, now),
            "Godspeed 0.5.8 is available. You're on 0.5.6."
        );
        let failed = UpdateStatus {
            error: Some("offline".into()),
            ..checked(3, "0.5.6", false)
        };
        assert_eq!(
            settings_detail("0.5.6", Some(&failed), &Flow::Idle, now),
            "You're on 0.5.6. The last check failed."
        );
        let ready = Flow::Ready {
            version: "0.5.8".into(),
            staged: PathBuf::from("/tmp/Godspeed.app"),
        };
        assert_eq!(
            settings_detail("0.5.6", Some(&checked(3, "0.5.8", true)), &ready, now),
            "Godspeed 0.5.8 is ready — restart to update."
        );
    }

    #[test]
    fn strip_advises_installs_without_a_desktop_path() {
        let unmanaged = strip_for(&InstallKind::Unmanaged, false, &Flow::Idle, "0.2.86");
        assert_eq!(
            unmanaged,
            (
                SharedString::from("Update available — v0.2.86 · download from GitHub"),
                StripAction::Advise {
                    open_releases: true
                }
            )
        );
        let managed = InstallKind::Managed {
            app_root: PathBuf::from("/home/u/.godspeed/app"),
        };
        let strip = strip_for(&managed, false, &Flow::Idle, "0.2.86");
        if cfg!(target_os = "linux") {
            // Linux desktop installs share the managed layout and update
            // themselves like the other desktop platforms.
            assert_eq!(strip.1, StripAction::Download);
        } else {
            assert_eq!(
                strip.0,
                SharedString::from("Update available — v0.2.86 · run `godspeed update`")
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_installs_drive_the_desktop_flow() {
        let installed = InstallKind::WindowsPortable {
            directory: PathBuf::from(r"C:\Users\u\AppData\Local\Programs\Godspeed"),
        };
        assert_eq!(
            strip_for(&installed, false, &Flow::Idle, "0.2.86"),
            (
                SharedString::from("Update available — v0.2.86"),
                StripAction::Download
            )
        );
    }
}
