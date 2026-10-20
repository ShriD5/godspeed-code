//! Adding a project to another device ([`zeron_proto::project_transfer`]):
//! which devices can receive a project, the dialog's file selection and
//! clone URL, and the relay that carries the picked ignored files from the
//! source engine to the new checkout once the clone lands.
//!
//! The viewport is the relay: it reads each file from the source device and
//! writes it to the target, each through this app's own engine link, so the
//! two engines never talk to each other. While files are owed to a project,
//! its composer holds sends ([`AppState::project_relay`]).

use std::collections::BTreeSet;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use gpui::{App, Entity};
use zeron_proto::project_transfer::{
    self as transfer, CloneProtocol, IgnoredFile, IgnoredFileListing, MAX_RELAY_FILE_BYTES,
    MAX_RELAY_FILES, MAX_RELAY_TOTAL_BYTES, ProjectFileChunk, RELAY_CHUNK_BYTES,
};
use zeron_proto::{Device, Space};
use zeron_rpc::{RpcClient, methods};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// A device offered in "Add to <device>…", disabled with a reason when it
/// can't take the project right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddTarget {
    pub device_id: String,
    pub name: String,
    pub disabled: Option<String>,
}

/// Every device that holds no checkout of the project, by name. A device
/// that is offline, or whose engine (or the source's) predates project
/// transfer, is listed disabled with the reason. Pure.
pub fn add_targets(
    devices: &[Device],
    holders: &[&str],
    source: &Device,
    source_online: bool,
    online: impl Fn(&Device) -> bool,
) -> Vec<AddTarget> {
    let mut targets: Vec<AddTarget> = devices
        .iter()
        .filter(|device| !holders.contains(&device.id.as_str()))
        .map(|device| {
            let disabled = if !online(device) {
                Some(format!("{} is offline", device.name))
            } else if !source_online {
                Some(format!("{} is offline", source.name))
            } else if !device.supports(transfer::CAPABILITY) {
                Some(format!("Update Godspeed on {}", device.name))
            } else if !source.supports(transfer::CAPABILITY) {
                Some(format!("Update Godspeed on {}", source.name))
            } else {
                None
            };
            AddTarget {
                device_id: device.id.clone(),
                name: device.name.clone(),
                disabled,
            }
        })
        .collect();
    targets.sort_by_key(|target| (target.name.to_lowercase(), target.device_id.clone()));
    targets
}

impl AppState {
    /// Devices that could receive `space`'s project ([`add_targets`]).
    pub fn add_targets(&self, space: &Space, now: DateTime<Utc>) -> Vec<AddTarget> {
        let members = self.project_members(space);
        let holders: Vec<&str> = members.iter().map(|s| s.device_id.as_str()).collect();
        let source = self.device_with_capabilities(&space.device_id);
        let Some(source) = source else {
            return Vec::new();
        };
        let devices: Vec<Device> = self
            .devices
            .iter()
            .map(|device| {
                self.device_with_capabilities(&device.id)
                    .unwrap_or_else(|| device.clone())
            })
            .collect();
        add_targets(
            &devices,
            &holders,
            &source,
            self.device_online(&space.device_id, now),
            |device| self.device_online(&device.id, now),
        )
    }

    /// A device row with the attached engine's live capabilities when it is
    /// this device (its synced row can lag a fresh upgrade).
    fn device_with_capabilities(&self, device_id: &str) -> Option<Device> {
        let mut device = self.devices.iter().find(|d| d.id == device_id)?.clone();
        if let Some(engine) = self.engine()
            && engine.engine_info().device_id == device_id
        {
            device.capabilities = engine.engine_info().capabilities.clone();
        }
        Some(device)
    }

    /// Files still owed to the project `space_id` names, if any.
    pub fn project_relay(&self, space_id: &str) -> Option<&ProjectRelay> {
        self.project_relays.get(space_id)
    }

    /// The relay holding the composer's project: the selected chat's, else
    /// the new-session canvas's.
    pub fn composer_project_relay(&self) -> Option<(&str, &ProjectRelay)> {
        let space_id = match self.selected_chat_row() {
            Some(chat) => chat.space_id.as_deref()?,
            None => self.selected_space.as_deref()?,
        };
        self.project_relays
            .get_key_value(space_id)
            .map(|(id, relay)| (id.as_str(), relay))
    }

    /// Forget a failed relay (the user dismissed it).
    pub fn dismiss_project_relay(&mut self, space_id: &str, cx: &mut gpui::Context<Self>) {
        if self.project_relays.remove(space_id).is_some() {
            cx.notify();
        }
    }
}

// ---------------------------------------------------------------------------
// The dialog's file selection
// ---------------------------------------------------------------------------

/// Why a listed file can't be ticked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileBlock {
    /// Over [`MAX_RELAY_FILE_BYTES`].
    TooLarge,
    /// Ticking it would pass [`MAX_RELAY_TOTAL_BYTES`] or [`MAX_RELAY_FILES`].
    OverBudget,
}

impl FileBlock {
    pub fn label(self) -> &'static str {
        match self {
            Self::TooLarge => "Over 1 MB",
            Self::OverBudget => "Over the 10 MB limit",
        }
    }
}

/// The ignored files listed for copying, and which are ticked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSelection {
    pub files: Vec<IgnoredFile>,
    pub selected: BTreeSet<String>,
}

impl FileSelection {
    /// The listing with env files ticked ([`transfer::is_default_copied_file`]),
    /// in order, as far as the caps allow.
    pub fn with_defaults(files: Vec<IgnoredFile>) -> Self {
        let mut selection = Self {
            files,
            selected: BTreeSet::new(),
        };
        let defaults: Vec<String> = selection
            .files
            .iter()
            .filter(|file| transfer::is_default_copied_file(&file.path))
            .map(|file| file.path.clone())
            .collect();
        for path in defaults {
            selection.set(&path, true);
        }
        selection
    }

    pub fn selected_bytes(&self) -> u64 {
        self.files
            .iter()
            .filter(|file| self.selected.contains(&file.path))
            .map(|file| file.size)
            .sum()
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Why `file` can't be ticked now, `None` when it can (or is ticked).
    pub fn block(&self, file: &IgnoredFile) -> Option<FileBlock> {
        if self.selected.contains(&file.path) {
            return None;
        }
        if file.size > MAX_RELAY_FILE_BYTES {
            return Some(FileBlock::TooLarge);
        }
        if self.selected_bytes() + file.size > MAX_RELAY_TOTAL_BYTES
            || self.selected_count() >= MAX_RELAY_FILES
        {
            return Some(FileBlock::OverBudget);
        }
        None
    }

    /// Tick or untick `path`; `false` when it isn't listed or is blocked.
    pub fn set(&mut self, path: &str, checked: bool) -> bool {
        let Some(file) = self.files.iter().find(|file| file.path == path) else {
            return false;
        };
        if !checked {
            return self.selected.remove(path);
        }
        if self.block(file).is_some() {
            return false;
        }
        self.selected.insert(path.to_string())
    }

    pub fn toggle(&mut self, path: &str) -> bool {
        let checked = self.selected.contains(path);
        self.set(path, !checked)
    }

    /// The ticked files in listing order.
    pub fn chosen(&self) -> Vec<IgnoredFile> {
        self.files
            .iter()
            .filter(|file| self.selected.contains(&file.path))
            .cloned()
            .collect()
    }
}

/// "12 KB", "1.4 MB", "512 B".
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.0} KB", (value / KB).max(1.0))
    } else {
        format!("{:.1} MB", value / (KB * KB))
    }
}

// ---------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------

/// The source listing as the dialog sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Loading,
    Ready(IgnoredFileListing),
    Failed(String),
}

/// Where a submitted transfer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferPhase {
    Editing,
    Reading {
        done: usize,
        total: usize,
    },
    Cloning {
        stage: String,
        percent: Option<u8>,
    },
    Registering,
    /// Cloned and registered; waiting for the new project to sync here.
    Opening,
    Cancelling,
    Failed(String),
}

impl TransferPhase {
    pub fn busy(&self) -> bool {
        !matches!(self, Self::Editing | Self::Failed(_))
    }

    /// One line for the dialog's progress row.
    pub fn label(&self, target: &str) -> Option<String> {
        Some(match self {
            Self::Editing | Self::Failed(_) => return None,
            Self::Reading { done, total } => format!("Reading files {done}/{total}…"),
            Self::Cloning { stage, percent } => match percent {
                Some(percent) => format!("{stage} {percent}%"),
                None => format!("Cloning on {target}…"),
            },
            Self::Registering => format!("Adding the project on {target}…"),
            Self::Opening => format!("Opening it on {target}…"),
            Self::Cancelling => "Cancelling…".into(),
        })
    }
}

/// Everything the dialog decides, apart from its text inputs. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogModel {
    pub source_space_id: String,
    pub source_device_id: String,
    pub source_name: String,
    pub source_path: String,
    pub project_name: String,
    pub target_device_id: String,
    pub target_name: String,
    pub listing: Listing,
    pub selection: FileSelection,
    pub protocol: Option<CloneProtocol>,
    pub phase: TransferPhase,
}

impl DialogModel {
    pub fn new(space: &Space, source_name: &str, project_name: &str, target: &AddTarget) -> Self {
        Self {
            source_space_id: space.id.clone(),
            source_device_id: space.device_id.clone(),
            source_name: source_name.to_string(),
            source_path: space.path.clone(),
            project_name: project_name.to_string(),
            target_device_id: target.device_id.clone(),
            target_name: target.name.clone(),
            listing: Listing::Loading,
            selection: FileSelection::default(),
            protocol: None,
            phase: TransferPhase::Editing,
        }
    }

    /// The source listing arrived: tick the env files, adopt its protocol.
    pub fn accept_listing(&mut self, listing: Result<IgnoredFileListing, String>) {
        match listing {
            Ok(listing) => {
                self.selection = FileSelection::with_defaults(listing.files.clone());
                self.protocol = listing
                    .remote_url
                    .as_deref()
                    .and_then(transfer::clone_url_protocol);
                self.listing = Listing::Ready(listing);
            }
            Err(error) => self.listing = Listing::Failed(error),
        }
    }

    fn ready(&self) -> Option<&IgnoredFileListing> {
        match &self.listing {
            Listing::Ready(listing) => Some(listing),
            _ => None,
        }
    }

    pub fn remote_url(&self) -> Option<&str> {
        self.ready()?.remote_url.as_deref()
    }

    /// The URL the target clones: the source's, rewritten to the picked protocol.
    pub fn clone_url(&self) -> Option<String> {
        let url = self.remote_url()?;
        match self.protocol {
            Some(protocol) => transfer::clone_url_for_protocol(url, protocol),
            None => Some(url.to_string()),
        }
    }

    /// Whether `protocol` can be picked: hosted URLs only, and never a
    /// rewrite that would have to guess (a custom port).
    pub fn protocol_available(&self, protocol: CloneProtocol) -> bool {
        self.remote_url()
            .is_some_and(|url| transfer::clone_url_for_protocol(url, protocol).is_some())
    }

    /// Show the SSH/HTTPS toggle at all (a hosted remote).
    pub fn shows_protocol(&self) -> bool {
        self.remote_url()
            .and_then(transfer::clone_url_protocol)
            .is_some()
    }

    pub fn pick_protocol(&mut self, protocol: CloneProtocol) -> bool {
        if self.phase.busy() || !self.protocol_available(protocol) {
            return false;
        }
        self.protocol = Some(protocol);
        true
    }

    /// Where the clone lands by default on the target.
    pub fn default_destination(&self) -> String {
        let (sub, name) = match self.ready() {
            Some(listing) => (
                listing.subdirectory.as_str(),
                listing.repository_name.as_str(),
            ),
            None => ("", ""),
        };
        let root = transfer::checkout_root_of(&self.source_path, sub);
        transfer::default_destination(&root, name)
    }

    pub fn subdirectory(&self) -> String {
        self.ready()
            .map(|listing| listing.subdirectory.clone())
            .unwrap_or_default()
    }

    /// Why Clone can't be pressed, `None` when it can. A listing that is
    /// still loading blocks too: submitting early would skip the env files.
    pub fn submit_blocked(&self, destination: &str) -> Option<String> {
        if self.phase.busy() {
            return Some("Working…".into());
        }
        match &self.listing {
            Listing::Loading => return Some("Reading the project…".into()),
            Listing::Failed(error) => return Some(error.clone()),
            Listing::Ready(_) => {}
        }
        if self.remote_url().is_none() {
            return Some("This project has no remote to clone from".into());
        }
        if self.clone_url().is_none() {
            return Some("That protocol can't reach this remote".into());
        }
        if destination.trim().is_empty() {
            return Some(format!("Choose a folder on {}", self.target_name));
        }
        None
    }

    /// The submit button's label for the phase.
    pub fn submit_label(&self) -> &'static str {
        match self.phase {
            TransferPhase::Failed(_) => "Retry",
            _ => "Clone",
        }
    }
}

// ---------------------------------------------------------------------------
// Relaying files
// ---------------------------------------------------------------------------

/// One relayed file's bytes, read from the source when the user pressed Clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayFile {
    pub path: String,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayPhase {
    Copying,
    /// Per-file reasons; the failed files stay in [`ProjectRelay::files`].
    Failed(Vec<String>),
}

/// Files owed to a freshly added project. Its composer holds sends while
/// this exists; a failure stays until retried or dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRelay {
    pub target_device_id: String,
    pub target_name: String,
    pub files: Vec<RelayFile>,
    pub phase: RelayPhase,
}

impl ProjectRelay {
    pub fn title(&self) -> String {
        let count = self.files.len();
        let noun = if count == 1 { "file" } else { "files" };
        match &self.phase {
            RelayPhase::Copying => format!("Copying {count} {noun} to {}…", self.target_name),
            RelayPhase::Failed(_) => {
                format!("Couldn't copy {count} {noun} to {}", self.target_name)
            }
        }
    }
}

/// `params` addressed to `device` (no address for this device's engine).
pub fn device_params(
    mut params: serde_json::Value,
    device: &str,
    local: Option<&str>,
) -> serde_json::Value {
    if local != Some(device) {
        params["targetDeviceId"] = serde_json::json!(device);
    }
    params
}

/// Read one file from the source project, chunk by chunk.
pub async fn read_project_file(
    client: &RpcClient,
    device: &str,
    local: Option<&str>,
    space_id: &str,
    path: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    let mut expected: Option<u64> = None;
    for _ in 0..=(MAX_RELAY_FILE_BYTES as usize / RELAY_CHUNK_BYTES + 1) {
        let params = device_params(
            serde_json::json!({ "spaceId": space_id, "path": path, "offset": offset }),
            device,
            local,
        );
        let chunk: ProjectFileChunk = client
            .call_as(methods::READ_PROJECT_FILE, params)
            .await
            .map_err(|error| error.to_string())?;
        if *expected.get_or_insert(chunk.size) != chunk.size || chunk.size > MAX_RELAY_FILE_BYTES {
            return Err("The file changed while it was read".into());
        }
        let data = BASE64
            .decode(chunk.data.as_bytes())
            .map_err(|_| "The source sent a damaged chunk".to_string())?;
        bytes.extend_from_slice(&data);
        offset = chunk.next_offset;
        if chunk.done {
            return Ok(bytes);
        }
    }
    Err("The file kept growing while it was read".into())
}

/// Write one file into the new checkout, chunk by chunk.
pub async fn write_project_file(
    client: &RpcClient,
    device: &str,
    local: Option<&str>,
    space_id: &str,
    file: &RelayFile,
) -> Result<(), String> {
    let chunks: Vec<&[u8]> = if file.bytes.is_empty() {
        vec![&[]]
    } else {
        file.bytes.chunks(RELAY_CHUNK_BYTES).collect()
    };
    let last = chunks.len() - 1;
    let mut offset = 0u64;
    for (ix, chunk) in chunks.into_iter().enumerate() {
        let params = device_params(
            serde_json::json!({
                "spaceId": space_id,
                "path": file.path,
                "offset": offset,
                "data": BASE64.encode(chunk),
                "done": ix == last,
            }),
            device,
            local,
        );
        client
            .call(methods::WRITE_PROJECT_FILE, params)
            .await
            .map_err(|error| error.to_string())?;
        offset += chunk.len() as u64;
    }
    Ok(())
}

/// Hand `files` to the new project `space_id` and start writing them.
pub fn start_relay(
    state: &Entity<AppState>,
    space_id: String,
    target_device_id: String,
    target_name: String,
    files: Vec<RelayFile>,
    cx: &mut App,
) {
    if files.is_empty() {
        return;
    }
    state.update(cx, |state, cx| {
        state.project_relays.insert(
            space_id.clone(),
            ProjectRelay {
                target_device_id,
                target_name,
                files,
                phase: RelayPhase::Copying,
            },
        );
        cx.notify();
    });
    run_relay(state, space_id, cx);
}

/// (Re)write the files still owed to `space_id`. Failures keep their files
/// for a retry; success clears the relay and releases the composer.
pub fn run_relay(state: &Entity<AppState>, space_id: String, cx: &mut App) {
    let Some((engine, local, relay)) = state.update(cx, |state, cx| {
        let engine = state.engine().cloned()?;
        let local = state.local_device_id.clone();
        let relay = state.project_relays.get_mut(&space_id)?;
        relay.phase = RelayPhase::Copying;
        let relay = relay.clone();
        cx.notify();
        Some((engine, local, relay))
    }) else {
        return;
    };
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        let mut failed_files = Vec::new();
        let mut failures = Vec::new();
        for file in &relay.files {
            if let Err(error) = write_project_file(
                engine.client(),
                &relay.target_device_id,
                local.as_deref(),
                &space_id,
                file,
            )
            .await
            {
                failures.push(format!("{}: {error}", file.path));
                failed_files.push(file.clone());
            }
        }
        state
            .update(cx, |state, cx| {
                if failed_files.is_empty() {
                    state.project_relays.remove(&space_id);
                } else if let Some(entry) = state.project_relays.get_mut(&space_id) {
                    entry.files = failed_files;
                    entry.phase = RelayPhase::Failed(failures);
                }
                cx.notify();
            })
            .ok();
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> IgnoredFile {
        IgnoredFile {
            path: path.into(),
            size,
        }
    }

    fn device(id: &str, name: &str, capable: bool) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "darwin".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
            cursor_sdk_version: None,
            capabilities: if capable {
                vec![transfer::CAPABILITY.into()]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn env_files_are_ticked_by_default_within_the_caps() {
        let selection = FileSelection::with_defaults(vec![
            file(".env", 10),
            file("apps/web/.env.local", 20),
            file(".envrc", 5),
            file(".dev.vars", 7),
            file("dist.log", 3),
            file(".env.huge", MAX_RELAY_FILE_BYTES + 1),
        ]);
        assert_eq!(
            selection
                .selected
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [".dev.vars", ".env", ".envrc", "apps/web/.env.local"]
        );
        assert_eq!(selection.selected_bytes(), 42);
        assert_eq!(
            selection.block(&file(".env.huge", MAX_RELAY_FILE_BYTES + 1)),
            Some(FileBlock::TooLarge)
        );
        assert_eq!(selection.block(&file("dist.log", 3)), None);
    }

    #[test]
    fn the_total_and_count_caps_hold() {
        let per = MAX_RELAY_FILE_BYTES;
        let files: Vec<IgnoredFile> = (0..12).map(|ix| file(&format!("f{ix:02}"), per)).collect();
        let mut selection = FileSelection {
            files,
            selected: BTreeSet::new(),
        };
        for ix in 0..12 {
            selection.set(&format!("f{ix:02}"), true);
        }
        assert_eq!(selection.selected_count(), 10);
        assert!(selection.selected_bytes() <= MAX_RELAY_TOTAL_BYTES);
        assert_eq!(
            selection.block(&file("f11", per)),
            Some(FileBlock::OverBudget)
        );
        assert!(!selection.toggle("f11"));
        assert!(selection.toggle("f00")); // unticking always works
        assert!(selection.toggle("f11"));

        let many: Vec<IgnoredFile> = (0..MAX_RELAY_FILES + 5)
            .map(|ix| file(&format!(".env.{ix:03}"), 1))
            .collect();
        let selection = FileSelection::with_defaults(many);
        assert_eq!(selection.selected_count(), MAX_RELAY_FILES);
    }

    #[test]
    fn targets_are_devices_without_a_checkout_and_say_why_not() {
        let mac = device("mac", "Mac", true);
        let server = device("srv", "server", true);
        let old = device("old", "Old laptop", false);
        let away = device("away", "Away", true);
        let devices = [mac.clone(), server, old, away];
        let targets = add_targets(&devices, &["mac"], &mac, true, |d| d.id != "away");
        assert_eq!(
            targets,
            [
                AddTarget {
                    device_id: "away".into(),
                    name: "Away".into(),
                    disabled: Some("Away is offline".into()),
                },
                AddTarget {
                    device_id: "old".into(),
                    name: "Old laptop".into(),
                    disabled: Some("Update Godspeed on Old laptop".into()),
                },
                AddTarget {
                    device_id: "srv".into(),
                    name: "server".into(),
                    disabled: None,
                },
            ]
        );
        let stale_source = device("mac", "Mac", false);
        let targets = add_targets(&devices, &["mac"], &stale_source, true, |_| true);
        assert_eq!(
            targets
                .iter()
                .find(|t| t.device_id == "srv")
                .unwrap()
                .disabled
                .as_deref(),
            Some("Update Godspeed on Mac")
        );
        let targets = add_targets(&devices, &["mac"], &mac, false, |_| true);
        assert_eq!(targets[0].disabled.as_deref(), Some("Mac is offline"));
        assert!(
            add_targets(&devices, &["mac", "srv", "old", "away"], &mac, true, |_| {
                true
            })
            .is_empty()
        );
    }

    fn model() -> DialogModel {
        let space = Space {
            id: "s".into(),
            device_id: "mac".into(),
            path: "/Users/me/code/mono/apps/web".into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            repository_id: None,
            created_at: Utc::now(),
        };
        DialogModel::new(
            &space,
            "Mac",
            "web",
            &AddTarget {
                device_id: "srv".into(),
                name: "server".into(),
                disabled: None,
            },
        )
    }

    #[test]
    fn the_dialog_waits_for_the_listing_and_rewrites_the_url() {
        let mut model = model();
        assert_eq!(
            model.submit_blocked("~/x").as_deref(),
            Some("Reading the project…")
        );
        model.accept_listing(Ok(IgnoredFileListing {
            files: vec![file(".env.local", 4), file("cache.db", 9)],
            truncated: false,
            remote_url: Some("git@github.com:me/mono.git".into()),
            subdirectory: "apps/web".into(),
            repository_name: "mono".into(),
        }));
        assert_eq!(model.default_destination(), "~/code/mono");
        assert_eq!(model.subdirectory(), "apps/web");
        assert_eq!(model.protocol, Some(CloneProtocol::Ssh));
        assert!(model.shows_protocol());
        assert_eq!(
            model.clone_url().as_deref(),
            Some("git@github.com:me/mono.git")
        );
        assert!(model.pick_protocol(CloneProtocol::Https));
        assert_eq!(
            model.clone_url().as_deref(),
            Some("https://github.com/me/mono.git")
        );
        assert_eq!(model.selection.chosen(), [file(".env.local", 4)]);
        assert_eq!(model.submit_blocked("~/code/mono"), None);
        assert!(model.submit_blocked("  ").is_some());
        model.phase = TransferPhase::Cloning {
            stage: "Receiving objects".into(),
            percent: Some(40),
        };
        assert!(model.submit_blocked("~/code/mono").is_some());
        assert!(!model.pick_protocol(CloneProtocol::Ssh));
        assert_eq!(
            model.phase.label("server").as_deref(),
            Some("Receiving objects 40%")
        );
        model.phase = TransferPhase::Failed("nope".into());
        assert_eq!(model.submit_label(), "Retry");
        assert_eq!(model.submit_blocked("~/code/mono"), None);
    }

    #[test]
    fn custom_ports_and_local_remotes_keep_their_url() {
        let mut model = model();
        model.accept_listing(Ok(IgnoredFileListing {
            remote_url: Some("ssh://git@host.example:2222/a/b.git".into()),
            ..Default::default()
        }));
        assert!(model.shows_protocol());
        assert!(!model.protocol_available(CloneProtocol::Https));
        assert!(!model.pick_protocol(CloneProtocol::Https));
        assert_eq!(
            model.clone_url().as_deref(),
            Some("ssh://git@host.example:2222/a/b.git")
        );
        let mut local = self::model();
        local.accept_listing(Ok(IgnoredFileListing {
            remote_url: Some("/srv/git/repo.git".into()),
            ..Default::default()
        }));
        assert!(!local.shows_protocol());
        assert_eq!(local.clone_url().as_deref(), Some("/srv/git/repo.git"));
        let mut none = self::model();
        none.accept_listing(Ok(IgnoredFileListing::default()));
        assert_eq!(
            none.submit_blocked("~/x").as_deref(),
            Some("This project has no remote to clone from")
        );
        let mut failed = self::model();
        failed.accept_listing(Err("Update Godspeed on Mac".into()));
        assert_eq!(
            failed.submit_blocked("~/x").as_deref(),
            Some("Update Godspeed on Mac")
        );
    }

    #[test]
    fn relay_titles_and_sizes_read_well() {
        let relay = ProjectRelay {
            target_device_id: "srv".into(),
            target_name: "server".into(),
            files: vec![RelayFile {
                path: ".env".into(),
                bytes: Arc::new(b"A=1".to_vec()),
            }],
            phase: RelayPhase::Copying,
        };
        assert_eq!(relay.title(), "Copying 1 file to server…");
        let failed = ProjectRelay {
            phase: RelayPhase::Failed(vec![".env: nope".into()]),
            ..relay
        };
        assert_eq!(failed.title(), "Couldn't copy 1 file to server");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(1536 * 1024), "1.5 MB");
        assert_eq!(
            device_params(serde_json::json!({}), "mac", Some("mac")),
            serde_json::json!({})
        );
        assert_eq!(
            device_params(serde_json::json!({}), "srv", Some("mac")),
            serde_json::json!({ "targetDeviceId": "srv" })
        );
    }
}
