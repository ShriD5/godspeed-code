//! zeron-update — release checking and self-update, shared by the engine (the
//! background checker + `ApplyUpdate`), the CLI (`godspeed update`), and the UI
//! (its own report-only checker, the "Check for Updates…" menu item, the
//! sidebar update strip, and the desktop install paths).
//!
//! Release feed: GitHub Releases. [`DEFAULT_FEED_URL`] is
//! `https://github.com/ShriD5/godspeed-code/releases/latest/download`, which
//! GitHub redirects (302, then to its asset CDN) to the newest published
//! release's asset of that name. Each release carries `manifest.json`
//! (written by `scripts/release-manifest.py`: the version plus a sha256 and
//! size per artifact) next to the artifacts. Artifacts themselves are fetched
//! from the release the manifest names (`releases/download/v<version>/<file>`),
//! so a release published mid-download can't swap the file under the manifest.
//! `latest.txt` (version only) remains as the fallback for feeds without a
//! manifest. `GODSPEED_RELEASES_URL` overrides the base (self-hosting,
//! testing); it must be HTTPS without credentials, query, or fragment.
//!
//! Install kinds and their update paths:
//! - **Managed** (`~/.godspeed/app/<ver>` + `current` symlink — the curl|sh
//!   installer and the Linux tarball's `install.sh`): download the headless
//!   tarball into a new versioned dir, flip the symlink, then restart the
//!   service (daemon) or relaunch (desktop). Same flow the installer script
//!   performs, natively.
//! - **MacApp** (running out of an app bundle): download the app tarball,
//!   verify it (sha256, code signature, Team ID, notarization, version), swap
//!   the bundle directory, relaunch. Driven by the UI.
//! - **WindowsPortable** (the Windows installer or portable zip — both carry
//!   `zeron-update.json`): swap the executable in place. Driven by the UI.
//! - **Unmanaged** (source builds, hand-copied binaries): report only — the
//!   UI's advisory strip links to [`RELEASES_PAGE`].
//!
//! Desktop installs download as soon as a release is seen and install either
//! on "Restart to update" or when the app next quits, so an app nobody
//! restarts deliberately still lands on the new version at its next launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

#[cfg(windows)]
pub mod windows;

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Successful checks repeat this often (plus one shortly after launch and on
/// window activation once due). The feed is a sub-kilobyte document on
/// GitHub; six hours keeps a long-running app reasonably current without
/// polling GitHub from every install all day.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Failed checks (offline boot, captive portal, transient feed error) back off
/// through these delays, then stay at the last one.
const CHECK_RETRY: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
];
/// Check deadlines are wall-clock times: a monotonic sleep stops counting
/// while the machine sleeps, so a laptop closed overnight used to push its
/// next check out by the whole night. The loop re-reads the clock this often.
const SCHEDULE_TICK: Duration = Duration::from_secs(60);
/// The engine's first check waits out engine boot (room joins, doc re-sync).
const ENGINE_INITIAL_DELAY: Duration = Duration::from_secs(20);
/// The desktop checker only needs the window on screen first.
const DESKTOP_INITIAL_DELAY: Duration = Duration::from_secs(2);
/// While an auto-apply is deferred behind active sessions, re-probe idleness
/// this often.
const IDLE_RECHECK: Duration = Duration::from_secs(5 * 60);
/// A staged binary must answer `--version` within this window.
const STAGED_VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// Release metadata is tiny. Bound both its buffered size and total transfer
/// time so a compromised or misconfigured public feed cannot hold a checker
/// forever or make every local install buffer an unbounded response.
const RELEASE_METADATA_MAX_BYTES: usize = 1024 * 1024;
const RELEASE_METADATA_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// `{feed}/manifest.json` — written by `scripts/release-manifest.py` and
/// uploaded with every release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Artifact file name → metadata. Empty for pre-manifest releases resolved
    /// via `latest.txt` — downloads then skip checksum verification (with a log).
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    #[serde(default)]
    pub sha256: Option<String>,
    /// Exact byte size. Optional for older manifests; when present a download
    /// that is longer (or ends shorter) is rejected before hashing finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Artifact-name platform pair matching the packaging scripts. Unsupported
/// updater targets retain their real OS name rather than impersonating Linux.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

fn managed_updates_supported(os: &str) -> bool {
    matches!(os, "linux" | "macos")
}

fn require_managed_update_platform() -> anyhow::Result<()> {
    if !managed_updates_supported(std::env::consts::OS) {
        bail!(
            "managed updates are not supported on {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

fn require_mac_app_update_platform() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        bail!(
            "macOS app updates are not supported on {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

/// `godspeed-<ver>-<os>-<arch>.tar.gz` — the headless/CLI tarball (Linux CI builds).
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("godspeed-{version}-{os}-{arch}.tar.gz")
}

/// `godspeed-<ver>-macos-<arch>-app.tar.gz` — the macOS app update payload.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    format!("godspeed-{version}-macos-{arch}-app.tar.gz")
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    let nums: Vec<u64> = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (!nums.is_empty()).then_some(nums)
}

/// Strictly-newer dotted-numeric compare (`0.1.10` > `0.1.9` > `0.1`).
/// Unparseable versions never count as newer — a garbage `latest.txt` must not
/// trigger an update loop.
pub fn version_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Fetch the newest release metadata: `manifest.json`, falling back to
/// `latest.txt` (version only, no checksums) for pre-manifest releases.
pub async fn fetch_latest(feed: &str) -> anyhow::Result<Manifest> {
    let base = release_base(feed)?;
    let client = http_client()?;
    let manifest_url = format!("{base}/manifest.json");
    match fetch_release_metadata(&client, &manifest_url).await {
        Ok(response) if response.status.is_success() => {
            let manifest: Manifest =
                serde_json::from_slice(&response.body).context("parsing manifest.json")?;
            if manifest.version.trim().is_empty() {
                bail!("manifest.json has an empty version");
            }
            return Ok(manifest);
        }
        Ok(response) => {
            tracing::debug!(status = %response.status, "manifest.json unavailable; trying latest.txt")
        }
        Err(err) => tracing::debug!(error = %err, "manifest.json fetch failed; trying latest.txt"),
    }
    let latest_url = format!("{base}/latest.txt");
    let response = fetch_release_metadata(&client, &latest_url)
        .await
        .context("fetching latest.txt")?;
    anyhow::ensure!(
        response.status.is_success(),
        "fetching latest.txt: HTTP {}",
        response.status
    );
    let version = std::str::from_utf8(&response.body)
        .context("reading latest.txt as UTF-8")?
        .trim()
        .to_string();
    if version.is_empty() {
        bail!("latest.txt is empty");
    }
    Ok(Manifest {
        version,
        files: BTreeMap::new(),
    })
}

#[derive(Debug)]
struct ReleaseMetadataResponse {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

async fn fetch_release_metadata(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<ReleaseMetadataResponse> {
    fetch_release_metadata_with_limits(
        client,
        url,
        RELEASE_METADATA_TIMEOUT,
        RELEASE_METADATA_MAX_BYTES,
    )
    .await
}

async fn fetch_release_metadata_with_limits(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<ReleaseMetadataResponse> {
    let request = async {
        let response = client
            .get(url)
            // Intermediaries must revalidate: a proxy holding an old
            // manifest is exactly the "prompt appears late" failure.
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        let status = response.status();
        if !status.is_success() {
            return Ok(ReleaseMetadataResponse {
                status,
                body: Vec::new(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            bail!("release metadata exceeds {max_bytes} bytes");
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading release metadata")?;
            anyhow::ensure!(
                body.len().saturating_add(chunk.len()) <= max_bytes,
                "release metadata exceeds {max_bytes} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(ReleaseMetadataResponse { status, body })
    };
    tokio::time::timeout(timeout, request)
        .await
        .with_context(|| format!("fetching {url} exceeded the metadata deadline"))?
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    http_client_with_timeouts(Duration::from_secs(15), Duration::from_secs(30))
}

fn http_client_with_timeouts(connect: Duration, read: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect)
        // Inactivity timeout, not a total download cap: slow progressing
        // updates remain viable on constrained links.
        .read_timeout(read)
        .user_agent(concat!("godspeed/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many update redirects");
            }
            if attempt.previous().iter().any(|url| url.scheme() == "https")
                && attempt.url().scheme() != "https"
            {
                return attempt.error("update redirect would downgrade HTTPS");
            }
            attempt.follow()
        }))
        .build()
        .context("building http client")
}

fn validate_release_override(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim()).context("invalid update feed URL")?;
    anyhow::ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "update feed must use HTTPS"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "update feed must be a base URL without credentials, query, or fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The project's GitHub releases page — the advisory update strip opens this
/// for unmanaged installs (source builds, hand-copied binaries), where no
/// updater flow exists to drive.
pub const RELEASES_PAGE: &str = "https://github.com/ShriD5/godspeed-code/releases";

/// The newest release's page — the download destination offered when this
/// installation cannot replace itself.
pub const LATEST_RELEASE_PAGE: &str = "https://github.com/ShriD5/godspeed-code/releases/latest";

/// The release feed every install checks unless `GODSPEED_RELEASES_URL` (or a
/// Windows install's `zeron-update.json`) says otherwise. GitHub answers
/// `latest/download/<asset>` with a redirect to the newest published
/// release's asset; the update client follows it (HTTPS only, at most ten
/// hops) to GitHub's asset CDN.
pub const DEFAULT_FEED_URL: &str =
    "https://github.com/ShriD5/godspeed-code/releases/latest/download";

/// The feed base in effect: the env override, then a Windows install's own
/// configuration, then `feed` (normally [`DEFAULT_FEED_URL`]; tests pass a
/// local server).
fn release_base(feed: &str) -> anyhow::Result<String> {
    if let Ok(url) = std::env::var("GODSPEED_RELEASES_URL")
        && !url.trim().is_empty()
    {
        return validate_release_override(&url);
    }
    #[cfg(windows)]
    if let Some(url) = windows::release_url()? {
        return Ok(url.trim_end_matches('/').to_owned());
    }
    Ok(feed.trim_end_matches('/').to_owned())
}

/// Where `file` of release `version` downloads from. A GitHub
/// `…/releases/latest/download` base is pinned to the manifest's own release
/// (`…/releases/download/v<version>`): "latest" can move between reading the
/// manifest and fetching the artifact, and the older artifact would then 404.
/// `version` is validated as dotted digits before it gets here.
fn artifact_url(base: &str, version: &str, file: &str) -> String {
    const LATEST: &str = "/releases/latest/download";
    match base.strip_suffix(LATEST) {
        Some(repo) if repo.starts_with("https://github.com/") => {
            format!("{repo}/releases/download/v{version}/{file}")
        }
        _ => format!("{base}/{file}"),
    }
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.godspeed/app/<ver>/godspeed` behind the `current` symlink
    /// (curl|sh installer, the Linux tarball's `install.sh`, or a previous
    /// `godspeed update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Windows installer or portable package with an explicit update-feed
    /// configuration.
    #[cfg(windows)]
    WindowsPortable { directory: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

/// Why an installation that normally updates itself cannot right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateBlocker {
    /// macOS App Translocation ran the app from a randomized read-only path
    /// (launched from Downloads or a disk image without being moved first).
    Translocated,
    /// Running straight from the mounted disk image.
    DiskImage,
    /// The directory holding the installation is not writable by this user.
    NotWritable(PathBuf),
}

impl std::fmt::Display for UpdateBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Translocated | Self::DiskImage => f.write_str(
                "Godspeed is running from a temporary, read-only location. Move Godspeed to your \
                 Applications folder and reopen it to turn on updates.",
            ),
            Self::NotWritable(dir) => write!(
                f,
                "Godspeed doesn't have permission to replace itself in {}.",
                dir.display()
            ),
        }
    }
}

impl InstallKind {
    /// Whether the desktop app downloads and installs updates itself.
    pub fn supports_desktop_update(&self) -> bool {
        match self {
            Self::MacApp { .. } => true,
            // The desktop path relaunches a GUI binary; on Linux that binary
            // is the managed install itself.
            Self::Managed { .. } => cfg!(target_os = "linux"),
            #[cfg(windows)]
            Self::WindowsPortable { .. } => true,
            Self::Unmanaged => false,
        }
    }

    /// Why this installation cannot replace itself, when it can't. Checked
    /// before downloading so the UI can explain the fix instead of failing
    /// after a wasted download with a raw rename error.
    pub fn desktop_update_blocker(&self) -> Option<UpdateBlocker> {
        match self {
            Self::MacApp { bundle } => mac_bundle_blocker(bundle, dir_writable),
            Self::Managed { app_root } => {
                (!dir_writable(app_root)).then(|| UpdateBlocker::NotWritable(app_root.clone()))
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                (!dir_writable(directory)).then(|| UpdateBlocker::NotWritable(directory.clone()))
            }
            Self::Unmanaged => None,
        }
    }

    pub async fn stage_desktop(
        &self,
        feed: &str,
        manifest: &Manifest,
        data_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        match self {
            Self::MacApp { .. } => stage_mac_app(feed, manifest, data_dir).await,
            Self::Managed { app_root } if self.supports_desktop_update() => {
                stage_headless(feed, manifest, app_root).await
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                windows::stage(feed, manifest, directory).await
            }
            _ => bail!("this installation does not support desktop updates"),
        }
    }

    /// Install a staged update. With `relaunch`, the new version starts once
    /// this process exits ("Restart to update"); without it the update takes
    /// effect at the next launch ("install on quit"). Either way the caller
    /// must quit after this succeeds.
    pub fn apply_desktop(&self, staged: &Path, relaunch: bool) -> anyhow::Result<()> {
        match self {
            Self::MacApp { bundle } => {
                apply_mac_app(staged, bundle)?;
                // The swap copied the staged bundle; the cache is spent.
                if let Some(version_dir) = staged.parent() {
                    let _ = std::fs::remove_dir_all(version_dir);
                }
                if relaunch {
                    relaunch_after_exit(Path::new("/usr/bin/open"), bundle);
                }
                Ok(())
            }
            Self::Managed { app_root } if self.supports_desktop_update() => {
                let version = staged
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("staged install has no version directory")?;
                apply_headless(app_root, version)?;
                if relaunch {
                    let binary = app_root.join("current").join("godspeed");
                    relaunch_after_exit(&binary, Path::new(""));
                }
                Ok(())
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => windows::apply(staged, directory, relaunch),
            _ => bail!("this installation does not support desktop updates"),
        }
    }
}

fn mac_bundle_blocker(bundle: &Path, writable: impl Fn(&Path) -> bool) -> Option<UpdateBlocker> {
    if bundle
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        return Some(UpdateBlocker::Translocated);
    }
    let parent = bundle.parent()?;
    if writable(parent) {
        return None;
    }
    if bundle.starts_with("/Volumes") {
        return Some(UpdateBlocker::DiskImage);
    }
    Some(UpdateBlocker::NotWritable(parent.to_path_buf()))
}

/// Probe by creating (and removing) a file: permission bits alone miss
/// read-only mounts, ACLs, and sandboxed locations.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".godspeed-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            true
        }
        // A leftover probe from a crashed run still proves writability.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    detect_install_from_for_os(exe, home, std::env::consts::OS)
}

fn detect_install_from_for_os(exe: &Path, home: Option<&Path>, os: &str) -> InstallKind {
    #[cfg(windows)]
    if os == "windows" && windows::is_managed(exe) {
        return InstallKind::WindowsPortable {
            directory: exe.parent().unwrap().to_owned(),
        };
    }
    // Never interpret a coincidental Windows `%HOME%\.godspeed\app` layout as
    // the Unix symlink-managed installation.
    if !managed_updates_supported(os) {
        return InstallKind::Unmanaged;
    }
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        let app_root = home.join(".godspeed").join("app");
        if exe.starts_with(&app_root) {
            return InstallKind::Managed { app_root };
        }
    }
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app")
            && exe.starts_with(ancestor.join("Contents").join("MacOS"))
        {
            return InstallKind::MacApp {
                bundle: ancestor.to_path_buf(),
            };
        }
    }
    InstallKind::Unmanaged
}

/// The version currently installed on disk for `kind`, read without executing
/// anything. A long-running process compares it with its own version to notice
/// that something else (the desktop app, `godspeed update`, a re-run installer)
/// has already replaced its binary.
pub fn installed_version(kind: &InstallKind) -> Option<String> {
    let version = match kind {
        InstallKind::Managed { app_root } => std::fs::read_link(app_root.join("current"))
            .ok()?
            .file_name()?
            .to_str()?
            .to_owned(),
        InstallKind::MacApp { bundle } => bundle_short_version(
            &std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?,
        )?,
        _ => return None,
    };
    parse_version(&version).map(|_| version)
}

/// `CFBundleShortVersionString` from an XML property list (the packaging
/// template; codesign leaves the format alone).
fn bundle_short_version(plist: &str) -> Option<String> {
    let after_key = plist
        .split("<key>CFBundleShortVersionString</key>")
        .nth(1)?;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let end = value.find("</string>")?;
    Some(value[..end].trim().to_owned())
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream `file` of the manifest's release to `dest` (see [`artifact_url`]),
/// verifying the manifest sha256 and size when present. Writes through a
/// `.partial` sidecar so an interrupted or rejected download never leaves a
/// plausible-looking artifact behind.
pub async fn download_release_file(
    feed: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = artifact_url(&release_base(feed)?, &manifest.version, file);
    let meta = manifest.files.get(file);
    let expected = meta.and_then(|m| m.sha256.as_deref());
    let expected_size = meta.and_then(|m| m.size);
    if expected.is_none() {
        tracing::warn!(
            file,
            "no checksum in release metadata; skipping verification"
        );
    }
    let partial = dest.with_extension("partial");
    let resp = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    if let (Some(expected), Some(advertised)) = (expected_size, resp.content_length()) {
        anyhow::ensure!(
            advertised == expected,
            "size mismatch for {file}: expected {expected} bytes, server sent {advertised}"
        );
    }
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let result = async {
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading download stream")?;
            written = written.saturating_add(chunk.len() as u64);
            if let Some(expected) = expected_size {
                anyhow::ensure!(
                    written <= expected,
                    "size mismatch for {file}: more than the expected {expected} bytes"
                );
            }
            hasher.update(&chunk);
            out.write_all(&chunk).await.context("writing download")?;
        }
        out.flush().await.context("flushing download")?;
        if let Some(expected) = expected_size {
            anyhow::ensure!(
                written == expected,
                "size mismatch for {file}: expected {expected} bytes, got {written}"
            );
        }
        if let Some(expected) = expected {
            let actual = format!("{:x}", hasher.finalize());
            anyhow::ensure!(
                actual.eq_ignore_ascii_case(expected.trim()),
                "checksum mismatch for {file}: expected {expected}, got {actual}"
            );
        }
        anyhow::Ok(())
    }
    .await;
    drop(out);
    if let Err(err) = result {
        tokio::fs::remove_file(&partial).await.ok();
        return Err(err);
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Run a freshly unpacked binary's `--version` before it can replace anything:
/// proves the payload is complete, executable on this machine, and the release
/// the manifest promised.
async fn verify_staged_binary(binary: &Path, version: &str) -> anyhow::Result<()> {
    let output = tokio::time::timeout(
        STAGED_VERSION_TIMEOUT,
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("staged binary version check timed out")?
    .with_context(|| format!("running {} --version", binary.display()))?;
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    anyhow::ensure!(
        output.status.success() && reported == format!("godspeed {version}"),
        "staged binary reported {reported:?} (exit {}), expected \"godspeed {version}\"",
        output.status
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path and Linux desktop
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>` (idempotent —
/// an already-staged version is reused). Returns the versioned dir.
pub async fn stage_headless(
    feed: &str,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_managed_update_platform()?;
    let version = &manifest.version;
    anyhow::ensure!(
        parse_version(version).is_some(),
        "invalid release version {version:?}"
    );
    let dest = app_root.join(version);
    if dest.join("godspeed").exists() {
        return Ok(dest);
    }
    let file = headless_artifact(version);
    let stage = app_root.join(format!(".stage-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let result = async {
        let tarball = stage.join(&file);
        download_release_file(feed, manifest, &file, &tarball).await?;
        let unpacked = stage.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        // Tarball root is the versioned stage dir (see scripts/package-linux.sh);
        // strip it exactly as install.sh does.
        run(
            "tar",
            &[
                "-xzf",
                &tarball.to_string_lossy(),
                "-C",
                &unpacked.to_string_lossy(),
                "--strip-components=1",
            ],
        )?;
        if !unpacked.join("godspeed").is_file() {
            bail!("tarball {file} did not contain a godspeed binary");
        }
        verify_staged_binary(&unpacked.join("godspeed"), version).await?;
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // Lost a race with another stager — the staged copy is equivalent.
            Err(_) if dest.join("godspeed").exists() => {}
            Err(err) => {
                return Err(err).with_context(|| format!("moving {} into place", dest.display()));
            }
        }
        Ok(dest.clone())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !target.join("godspeed").exists() {
            bail!("{} is not a staged install", target.display());
        }
        let tmp = app_root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp).context("creating current symlink")?;
        std::fs::rename(&tmp, app_root.join("current")).context("swapping current symlink")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (app_root, version);
        require_managed_update_platform()?;
        unreachable!("supported managed-update platforms are Unix")
    }
}

/// Whether this process is the engine service [`restart_service`] manages:
/// systemd places it in the `godspeed.service` cgroup; launchd names the job in
/// `XPC_SERVICE_NAME`.
pub fn running_as_installed_service() -> bool {
    if cfg!(target_os = "macos") {
        std::env::var("XPC_SERVICE_NAME").is_ok_and(|label| label == "site.shrithan.godspeed")
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/self/cgroup")
            .is_ok_and(|cgroups| in_zeron_service_cgroup(&cgroups))
    } else {
        false
    }
}

fn in_zeron_service_cgroup(cgroups: &str) -> bool {
    cgroups
        .lines()
        .filter_map(|line| line.rsplit(':').next())
        .any(|path| path.split('/').any(|part| part == "godspeed.service"))
}

/// Restart the installed engine service (the same units `godspeed daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary. Only queues the restart: the caller may be the
/// service itself, which must stay responsive to the stop signal that follows.
pub fn restart_service() -> anyhow::Result<()> {
    require_managed_update_platform()?;
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{uid}/site.shrithan.godspeed")],
        )
    } else {
        run(
            "systemctl",
            &["--user", "--no-block", "restart", "godspeed.service"],
        )
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Apple Developer Team ID official Godspeed releases are signed with. A
/// staged update must carry the running app's Team ID; this is the fallback
/// when the running app has none (an ad-hoc local package), so such a build
/// still only accepts official releases.
pub const RELEASE_TEAM_ID: &str = "QPUV967B5U";

/// Developer ID Application certificate issued by Apple to `team`: Apple's
/// anchor, the Developer ID intermediate (…6.2.6), the Developer ID leaf
/// marker (…6.1.13), and the team in the leaf's OU.
fn developer_id_requirement(team: &str) -> String {
    format!(
        "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
         and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
         and certificate leaf[subject.OU] = \"{team}\""
    )
}

/// Apple Team IDs are ten uppercase letters or digits. Checked before a team
/// is spliced into a code requirement.
fn valid_team_id(team: &str) -> bool {
    team.len() == 10
        && team
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `TeamIdentifier=…` from `codesign -dv` output (it prints to stderr);
/// `not set` (ad-hoc or platform binaries) reads as `None`.
fn parse_team_identifier(display: &str) -> Option<String> {
    display
        .lines()
        .find_map(|line| line.trim().strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_owned)
}

/// The team a staged update must be signed by: the running app's, else
/// [`RELEASE_TEAM_ID`]. Pure.
fn expected_update_team(running: Option<&str>) -> &str {
    running.unwrap_or(RELEASE_TEAM_ID)
}

/// Refuse a staged app whose signing team differs from `expected`. Pure.
fn check_update_team(expected: &str, staged: Option<&str>) -> anyhow::Result<()> {
    match staged {
        Some(team) if team == expected => Ok(()),
        Some(team) => bail!(
            "the update is signed by team {team}, but this copy of Godspeed is signed by {expected}"
        ),
        None => bail!("the update has no Developer ID team (expected {expected})"),
    }
}

/// Whether `staged` may replace the running app: strictly newer than the
/// running version, and not older than what is already installed on disk
/// (another updater may have got there first). Never downgrades. Pure.
fn check_install_version(
    staged: &str,
    running: &str,
    installed: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        version_newer(staged, running),
        "refusing to install {staged:?} over {running}: not a newer version"
    );
    if let Some(installed) = installed {
        anyhow::ensure!(
            !version_newer(installed, staged),
            "refusing to install {staged} over the already installed {installed}"
        );
    }
    Ok(())
}

/// The signing team of the code at `path` (`None` when unsigned or ad-hoc).
fn code_team_id(path: &Path) -> anyhow::Result<Option<String>> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=2"])
        .arg(path)
        .output()
        .context("running codesign -dv")?;
    // Unsigned code makes `codesign -dv` fail; that is "no team", not an error.
    Ok(parse_team_identifier(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

/// The running app bundle's signing team, if it runs from a signed bundle.
fn running_app_team_id() -> Option<String> {
    match detect_install() {
        InstallKind::MacApp { bundle } => code_team_id(&bundle).ok().flatten(),
        _ => None,
    }
}

/// Gate between a downloaded bundle and anything that executes or installs
/// it: the whole bundle's signature must be intact (`codesign --verify --deep
/// --strict`), from a Developer ID certificate of the running app's team,
/// and, with `notarized`, accepted by Gatekeeper as notarized (`spctl`; the
/// ticket is stapled, so this works offline). Logs and fails otherwise.
pub fn verify_mac_app_signature(
    staged: &Path,
    running_team: Option<&str>,
    notarized: bool,
) -> anyhow::Result<()> {
    require_mac_app_update_platform()?;
    let result = (|| {
        let expected = expected_update_team(running_team);
        anyhow::ensure!(
            valid_team_id(expected),
            "the running app's Team ID {expected:?} is malformed"
        );
        let staged_str = staged.to_string_lossy();
        let requirement = format!("-R={}", developer_id_requirement(expected));
        run(
            "/usr/bin/codesign",
            &["--verify", "--deep", "--strict", &requirement, &staged_str],
        )
        .context("the update's code signature is invalid or not from Godspeed's developer")?;
        check_update_team(expected, code_team_id(staged)?.as_deref())?;
        if notarized {
            run(
                "/usr/sbin/spctl",
                &["--assess", "--type", "execute", &staged_str],
            )
            .context("Gatekeeper rejected the update (not notarized)")?;
        }
        anyhow::Ok(())
    })();
    if let Err(err) = &result {
        tracing::error!(staged = %staged.display(), error = %format!("{err:#}"), "refusing update: verification failed");
    }
    result
}

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/Godspeed.app`
/// (idempotent). The bundle is unpacked beside its final name and renamed in
/// only after it passes, in order: the manifest's sha256 and size (required),
/// [`verify_mac_app_signature`] (signature, Team ID, notarization) — before
/// anything from it runs — and its binary answering with the expected
/// version. An interrupted unpack can never be mistaken for a staged update,
/// and an older or equal release is never staged. Returns the staged bundle.
pub async fn stage_mac_app(
    feed: &str,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_mac_app_update_platform()?;
    let version = &manifest.version;
    anyhow::ensure!(
        parse_version(version).is_some(),
        "invalid release version {version:?}"
    );
    anyhow::ensure!(
        version_newer(version, current_version()),
        "refusing to stage {version}: not newer than the running {}",
        current_version()
    );
    let file = mac_app_artifact(version);
    anyhow::ensure!(
        manifest
            .files
            .get(&file)
            .and_then(|meta| meta.sha256.as_deref())
            .is_some_and(|sha| !sha.trim().is_empty()),
        "the release manifest has no checksum for {file}"
    );
    let running_team = running_app_team_id();
    let updates = data_dir.join("updates");
    let dir = updates.join(version);
    let staged = dir.join("Godspeed.app");
    let staged_binary = staged.join("Contents/MacOS/godspeed");
    if staged_binary.exists()
        && verify_mac_app_signature(&staged, running_team.as_deref(), true).is_ok()
        && verify_staged_binary(&staged_binary, version).await.is_ok()
    {
        return Ok(staged);
    }
    prune_stale_stages(&updates, version);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tarball = dir.join(&file);
    if let Err(err) = download_release_file(feed, manifest, &file, &tarball).await {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    let unpack = dir.join(".unpack");
    std::fs::create_dir_all(&unpack)?;
    let unpacked = run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &unpack.to_string_lossy(),
        ],
    )
    .map(|()| unpack.join("Godspeed.app"));
    std::fs::remove_file(&tarball).ok();
    let unpacked = match unpacked {
        Ok(unpacked) => unpacked,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(err);
        }
    };
    let unpacked_binary = unpacked.join("Contents/MacOS/godspeed");
    if !unpacked_binary.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("app tarball {file} did not contain Godspeed.app");
    }
    // Verify the signature before executing anything from the bundle.
    if let Err(err) = verify_mac_app_signature(&unpacked, running_team.as_deref(), true) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    if let Err(err) = verify_staged_binary(&unpacked_binary, version).await {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    std::fs::rename(&unpacked, &staged)
        .with_context(|| format!("moving {} into place", staged.display()))?;
    let _ = std::fs::remove_dir_all(&unpack);
    Ok(staged)
}

/// Staged bundles of other versions are dead weight (each is a whole app):
/// drop them whenever a new version stages.
fn prune_stale_stages(updates: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(updates) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Swap the installed bundle for the staged one: `ditto` the staged copy next to
/// the target (metadata-preserving, cross-volume safe), then two renames — the
/// old bundle is restored if the second rename fails.
///
/// The staged bundle is re-verified first (it sat in a user-writable cache
/// since staging): its version must be newer than the running one and not
/// older than the installed one, and its signature and Team ID must still
/// match the installed app's. Anything else is refused and logged.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<()> {
    require_mac_app_update_platform()?;
    let staged_version = std::fs::read_to_string(staged.join("Contents/Info.plist"))
        .ok()
        .as_deref()
        .and_then(bundle_short_version)
        .context("the staged update has no readable version")?;
    let installed = installed_version(&InstallKind::MacApp {
        bundle: bundle.to_path_buf(),
    });
    if let Err(err) = check_install_version(&staged_version, current_version(), installed.as_deref())
    {
        tracing::error!(error = %format!("{err:#}"), "refusing update");
        return Err(err);
    }
    let installed_team = code_team_id(bundle).ok().flatten();
    verify_mac_app_signature(staged, installed_team.as_deref(), false)?;
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    let name = bundle
        .file_name()
        .context("app bundle has no name")?
        .to_string_lossy();
    let pid = std::process::id();
    let fresh = parent.join(format!(".{name}.new-{pid}"));
    let old = parent.join(format!(".{name}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&fresh);
    if let Err(err) = run(
        "ditto",
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    ) {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(bundle, &old) {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("moving the current app aside");
    }
    if let Err(err) = std::fs::rename(&fresh, bundle) {
        let _ = std::fs::rename(&old, bundle);
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("installing the new app bundle");
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Detached relauncher: waits for THIS process to exit, then runs `program`
/// (with `argument` unless empty). Launching before exit would race the
/// single-instance engine lock and the IPC port. Paths travel as positional
/// parameters, never spliced into the script. The caller quits after this.
pub fn relaunch_after_exit(program: &Path, argument: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let pid = std::process::id().to_string();
        let script = r#"while /bin/kill -0 "$1" 2>/dev/null; do sleep 0.2; done
if [ -n "$3" ]; then exec "$2" "$3"; else exec "$2"; fi"#;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .arg("godspeed-relaunch")
            .arg(&pid)
            .arg(program)
            .arg(argument)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = command.spawn() {
            tracing::error!(error = %err, "failed to spawn the relauncher");
        }
    }
    #[cfg(not(unix))]
    let _ = (program, argument);
}

// ---------------------------------------------------------------------------
// Background checker
// ---------------------------------------------------------------------------

/// What the checker reports (the engine's `UpdateStatus` stream, the desktop
/// strip). Version facts only — download/apply progress is owned by whoever
/// drives the update (UI or CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            checked_at: None,
            error: None,
        }
    }
}

/// `GODSPEED_AUTO_UPDATE=1|true|yes` — headless daemons apply updates themselves.
fn auto_update_enabled() -> bool {
    std::env::var("GODSPEED_AUTO_UPDATE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// `GODSPEED_AUTO_UPDATE=0|false|no` — the desktop app then only reports: no
/// background download and no install on quit. Unset means on.
pub fn desktop_auto_update_enabled() -> bool {
    std::env::var("GODSPEED_AUTO_UPDATE")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Who runs the checker — decides what it may do beyond reporting.
enum Role {
    /// The engine: managed installs with `GODSPEED_AUTO_UPDATE` apply themselves,
    /// and a service daemon restarts into a binary someone else installed.
    Engine { quiescent: Option<QuiescentCheck> },
    /// The desktop app: report only — downloading and installing are UI
    /// decisions (see `crates/ui/src/app_update.rs`).
    Desktop,
}

/// Background release checker: polls the release feed every six hours on a wall-clock
/// schedule (so sleep/wake cannot stretch it), backs off on failure, and
/// publishes [`UpdateStatus`] over a watch channel. In the engine, managed
/// installs with `GODSPEED_AUTO_UPDATE` set stage + apply + service restart on
/// their own — but only in a quiet window: while `quiescent` reports activity,
/// the apply defers and re-probes every [`IDLE_RECHECK`].
#[derive(Clone)]
pub struct Updater {
    feed: String,
    role: Arc<Role>,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    /// Wakes the loop; `forced` says whether the wake demands a check or only
    /// a fresh look at the wall clock.
    wake_tx: Arc<watch::Sender<u64>>,
    forced: Arc<AtomicBool>,
    /// Scheduled (background) checks; off leaves only forced checks
    /// (Settings → General "Check for updates automatically").
    scheduled: Arc<AtomicBool>,
    /// Set by `godspeed headless`: this process is the installed service and may
    /// restart itself into a newer installed binary.
    service: Arc<AtomicBool>,
    /// Flips to true exactly once; the check loop selects against it so
    /// cancellation lands at any await point (no tokio-util in this crate).
    shutdown_tx: Arc<watch::Sender<bool>>,
    check_task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Updater {
    /// Spawn the engine's check loop (must run on a tokio runtime).
    pub fn spawn(feed: String, quiescent: Option<QuiescentCheck>) -> Self {
        Self::spawn_role(feed, Role::Engine { quiescent })
    }

    /// Spawn the desktop app's report-only check loop (must run on a tokio
    /// runtime).
    pub fn spawn_desktop(feed: String) -> Self {
        Self::spawn_role(feed, Role::Desktop)
    }

    fn spawn_role(feed: String, role: Role) -> Self {
        let initial_delay = match role {
            Role::Engine { .. } => ENGINE_INITIAL_DELAY,
            Role::Desktop => DESKTOP_INITIAL_DELAY,
        };
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let (wake_tx, _) = watch::channel(0);
        let (shutdown_tx, _) = watch::channel(false);
        let updater = Self {
            feed,
            role: Arc::new(role),
            status_tx: Arc::new(status_tx),
            wake_tx: Arc::new(wake_tx),
            forced: Arc::new(AtomicBool::new(false)),
            scheduled: Arc::new(AtomicBool::new(true)),
            service: Arc::new(AtomicBool::new(false)),
            shutdown_tx: Arc::new(shutdown_tx),
            check_task: Arc::new(std::sync::Mutex::new(None)),
        };
        // Subscribe before spawning so an immediate `check_now` cannot land
        // before the background task first polls and be lost behind the
        // initial delay.
        let wakes = updater.wake_tx.subscribe();
        let for_loop = updater.clone();
        let task = tokio::spawn(async move { for_loop.check_loop(wakes, initial_delay).await });
        *updater.check_task.lock().unwrap() = Some(task);
        updater
    }

    /// Stop the check loop and wait for it to exit — a replaced runtime must
    /// not keep polling the release feed (or auto-applying) in the background.
    /// Idempotent, and callable from any clone.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        let task = self
            .check_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    /// Check immediately, for example when authentication recovers after the
    /// process started offline.
    pub fn check_now(&self) {
        self.forced.store(true, Ordering::SeqCst);
        self.wake_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Turn scheduled checks on or off. Off, only [`Self::check_now`] and
    /// [`Self::check`] reach the feed; turning them back on checks at once if
    /// one is due.
    pub fn set_scheduled_checks(&self, enabled: bool) {
        if self.scheduled.swap(enabled, Ordering::SeqCst) != enabled && enabled {
            self.poke();
        }
    }

    pub fn scheduled_checks(&self) -> bool {
        self.scheduled.load(Ordering::SeqCst)
    }

    /// Re-read the wall clock now (window activation, system wake): checks
    /// only if one is due, so frequent pokes cost nothing.
    pub fn poke(&self) {
        self.wake_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Called by `godspeed headless`: when this process is the installed engine
    /// service and something else installs a newer binary, restart into it at
    /// the next quiet moment. A hand-started `godspeed headless` is left alone —
    /// restarting the service unit would start a second engine beside it.
    pub fn restart_when_superseded(&self) {
        if !running_as_installed_service() {
            return;
        }
        self.service.store(true, Ordering::SeqCst);
        self.poke();
    }

    /// One user-requested check, awaited: publishes the result like a
    /// scheduled check and returns it.
    pub async fn check(&self) -> anyhow::Result<UpdateStatus> {
        match fetch_latest(&self.feed).await {
            Ok(manifest) => {
                self.publish(manifest);
                Ok(self.status_tx.borrow().clone())
            }
            Err(err) => {
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                Err(err)
            }
        }
    }

    fn quiescent_now(&self) -> bool {
        match &*self.role {
            Role::Engine { quiescent } => quiescent.as_ref().is_none_or(|check| check()),
            Role::Desktop => true,
        }
    }

    async fn check_loop(&self, mut wakes: watch::Receiver<u64>, initial_delay: Duration) {
        let mut shutdown = self.shutdown_tx.subscribe();
        // Shutdown must cut the loop at ANY await point — including mid
        // `check_once()` / `auto_apply_when_idle()` HTTP — so the whole body
        // races the flag rather than checking it between iterations.
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => {}
            _ = async {
                tokio::select! {
                    _ = tokio::time::sleep(initial_delay) => {}
                    _ = wakes.changed() => {}
                }
                let mut schedule = Schedule::default();
                let mut superseded = SupersededRestart::default();
                loop {
                    let forced = self.forced.swap(false, Ordering::SeqCst);
                    let scheduled = self.scheduled.load(Ordering::SeqCst);
                    if forced || (scheduled && schedule.due(SystemTime::now())) {
                        let ok = self.check_once().await;
                        schedule.record(SystemTime::now(), ok);
                        if ok
                            && self.status_tx.borrow().update_available
                            && matches!(*self.role, Role::Engine { .. })
                            && auto_update_enabled()
                            && let InstallKind::Managed { .. } = detect_install()
                        {
                            self.auto_apply_when_idle().await;
                        }
                    }
                    if self.service.load(Ordering::SeqCst) {
                        superseded.poll(|| self.quiescent_now());
                    }
                    // Paused: wakes (check_now, re-enabling) still cut the
                    // sleep short, so idle at the tick instead of spinning.
                    let wait = if scheduled {
                        schedule.wait(SystemTime::now())
                    } else {
                        SCHEDULE_TICK
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = wakes.changed() => {}
                    }
                }
            } => {}
        }
    }

    /// Sessions must never die to an update: pre-stage the download now
    /// (harmless while busy), wait for a quiet window (no live runs, no open
    /// terminals), then apply — which re-fetches the manifest (so a long defer
    /// lands on whatever is newest) and reuses the staged dir, keeping the
    /// idle→restart gap to well under a second.
    async fn auto_apply_when_idle(&self) {
        if let InstallKind::Managed { app_root } = detect_install() {
            match fetch_latest(&self.feed).await {
                Ok(manifest) if version_newer(&manifest.version, current_version()) => {
                    if let Err(err) = stage_headless(&self.feed, &manifest, &app_root).await {
                        tracing::warn!(error = %err, "auto-update staging failed");
                        return;
                    }
                }
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update staging fetch failed");
                    return;
                }
            }
        }
        let mut deferred = false;
        loop {
            while !self.quiescent_now() {
                if !deferred {
                    deferred = true;
                    tracing::info!("auto-update deferred: sessions or terminals active");
                }
                tokio::time::sleep(IDLE_RECHECK).await;
            }
            // `apply_inner` fetches and stages again so a long defer lands on
            // the newest release. Re-check quiescence immediately before the
            // symlink swap: work can begin while that network/disk I/O runs.
            match self.apply_inner(true).await {
                Ok(Some(version)) => {
                    tracing::info!(%version, "auto-update applied; service restarting");
                    return;
                }
                Ok(None) => {
                    if !deferred {
                        deferred = true;
                        tracing::info!(
                            "auto-update deferred: activity began while staging the update"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update failed");
                    return;
                }
            }
        }
    }

    fn publish(&self, manifest: Manifest) {
        let status = UpdateStatus {
            current_version: current_version().to_string(),
            update_available: version_newer(&manifest.version, current_version()),
            latest_version: Some(manifest.version),
            checked_at: Some(now_ms()),
            error: None,
        };
        if status.update_available {
            tracing::info!(
                latest = status.latest_version.as_deref().unwrap_or(""),
                current = %status.current_version,
                "update available"
            );
        }
        self.status_tx.send_replace(status);
    }

    /// One check; returns false on fetch failure (retry sooner).
    async fn check_once(&self) -> bool {
        match fetch_latest(&self.feed).await {
            Ok(manifest) => {
                self.publish(manifest);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "update check failed");
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                false
            }
        }
    }

    /// Stage + apply the newest release on THIS device (managed installs only),
    /// then restart the service after a short delay so the caller's RPC reply
    /// flushes before systemd/launchd kills this process.
    pub async fn apply(&self) -> anyhow::Result<String> {
        self.apply_inner(false)
            .await?
            .ok_or_else(|| anyhow::anyhow!("update became busy before apply"))
    }

    /// `require_quiescent` is used by automatic updates. It deliberately checks
    /// after all network and staging I/O and directly before the destructive
    /// swap/restart boundary; `None` tells the caller to wait and try again.
    async fn apply_inner(&self, require_quiescent: bool) -> anyhow::Result<Option<String>> {
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest = fetch_latest(&self.feed).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        stage_headless(&self.feed, &manifest, &app_root).await?;
        if require_quiescent && !self.quiescent_now() {
            return Ok(None);
        }
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(Some(manifest.version))
    }
}

/// Wall-clock check deadlines with failure backoff.
#[derive(Debug, Default)]
struct Schedule {
    /// `None` until the first check: due immediately.
    next_due: Option<SystemTime>,
    failures: usize,
}

impl Schedule {
    fn due(&self, now: SystemTime) -> bool {
        match self.next_due {
            None => true,
            Some(due) => match due.duration_since(now) {
                Err(_) => true,
                // A deadline further out than any interval means the clock
                // jumped backwards; don't wait for it to catch up.
                Ok(remaining) => remaining.is_zero() || remaining > CHECK_INTERVAL,
            },
        }
    }

    fn record(&mut self, now: SystemTime, ok: bool) {
        let delay = if ok {
            self.failures = 0;
            CHECK_INTERVAL
        } else {
            let delay = CHECK_RETRY[self.failures.min(CHECK_RETRY.len() - 1)];
            self.failures = self.failures.saturating_add(1);
            delay
        };
        self.next_due = Some(now + delay);
    }

    /// Monotonic sleep until the next look at the clock.
    fn wait(&self, now: SystemTime) -> Duration {
        self.next_due
            .and_then(|due| due.duration_since(now).ok())
            .unwrap_or(Duration::ZERO)
            .clamp(Duration::from_secs(1), SCHEDULE_TICK)
    }
}

/// Restarts a service daemon into a newer binary that is already installed
/// (the desktop app swapped the bundle, `godspeed update` flipped the symlink).
/// Waits for quiescence, and tries at most once per installed version so a
/// broken service manager cannot turn into a restart loop.
#[derive(Debug, Default)]
struct SupersededRestart {
    attempted: Option<String>,
    deferred_logged: bool,
}

impl SupersededRestart {
    fn poll(&mut self, quiescent: impl Fn() -> bool) {
        let Some(installed) = installed_version(&detect_install()) else {
            return;
        };
        self.poll_installed(&installed, current_version(), quiescent, restart_service);
    }

    fn poll_installed(
        &mut self,
        installed: &str,
        running: &str,
        quiescent: impl Fn() -> bool,
        restart: impl FnOnce() -> anyhow::Result<()>,
    ) {
        if !version_newer(installed, running) || self.attempted.as_deref() == Some(installed) {
            return;
        }
        if !quiescent() {
            if !self.deferred_logged {
                self.deferred_logged = true;
                tracing::info!(
                    installed,
                    running,
                    "newer version installed; restart deferred: sessions or terminals active"
                );
            }
            return;
        }
        self.attempted = Some(installed.to_owned());
        tracing::info!(
            installed,
            running,
            "newer version installed; restarting service"
        );
        if let Err(err) = restart() {
            tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_update_headers_and_body_time_out_but_progressing_body_survives() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for stall_body in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                if stall_body {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
            let client =
                http_client_with_timeouts(Duration::from_millis(200), Duration::from_millis(100))
                    .unwrap();
            let request = async {
                client
                    .get(format!("http://{address}"))
                    .send()
                    .await?
                    .bytes()
                    .await
            };
            let error = tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .expect("bounded read")
                .unwrap_err();
            assert!(error.is_timeout());
            server.abort();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..10 {
                socket.write_all(b"x").await.unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let client =
            http_client_with_timeouts(Duration::from_secs(1), Duration::from_millis(200)).unwrap();
        assert_eq!(
            client
                .get(format!("http://{address}"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .len(),
            10
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn release_metadata_has_size_and_total_time_limits() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Reject an advertised oversized body before buffering it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n01234567890")
                .await
                .unwrap();
        });
        let client =
            http_client_with_timeouts(Duration::from_secs(1), Duration::from_secs(1)).unwrap();
        let error = fetch_release_metadata_with_limits(
            &client,
            &format!("http://{address}"),
            Duration::from_secs(1),
            10,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds 10 bytes"));
        server.await.unwrap();

        // Enforce the same cap when Content-Length is absent.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n01234567890")
                .await
                .unwrap();
        });
        let error = fetch_release_metadata_with_limits(
            &client,
            &format!("http://{address}"),
            Duration::from_secs(1),
            10,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds 10 bytes"));
        server.await.unwrap();

        // Progressing bytes stay below the client's inactivity timeout but
        // must still obey the metadata operation's total deadline.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..10 {
                if socket.write_all(b"x").await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        let error = fetch_release_metadata_with_limits(
            &client,
            &format!("http://{address}"),
            Duration::from_millis(80),
            100,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("metadata deadline"));
        server.abort();
    }

    #[tokio::test]
    async fn stalled_update_tls_handshake_has_a_connect_deadline() {
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let client =
            http_client_with_timeouts(Duration::from_millis(100), Duration::from_secs(30)).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("https://{address}")).send(),
        )
        .await
        .expect("bounded TLS handshake")
        .unwrap_err();
        assert!(error.is_timeout());
        server.abort();
    }

    #[test]
    fn update_feed_override_requires_an_https_base_url() {
        assert_eq!(
            validate_release_override(" https://example.com/releases/ ").unwrap(),
            "https://example.com/releases"
        );
        for url in [
            "http://example.com/releases",
            "file:///tmp/update",
            "https://user:password@example.com",
            "https://example.com?feed=x",
            "https://example.com/#fragment",
        ] {
            assert!(validate_release_override(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn version_compare() {
        assert!(version_newer("0.1.1", "0.1.0"));
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.1.1", "0.1.0"));
        assert!(version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.1"));
        // Garbage never counts as newer.
        assert!(!version_newer("", "0.1.0"));
        assert!(!version_newer("nightly", "0.1.0"));
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/home/u/.godspeed/app/0.1.1/godspeed"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.godspeed/app")
            }
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/Applications/Godspeed.app/Contents/MacOS/godspeed"),
                Some(Path::new("/Users/u")),
                "macos",
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Godspeed.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from_for_os(Path::new("/tmp/foo.app/godspeed"), None, "macos"),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/src/target/release/godspeed"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Unmanaged
        );
    }

    #[test]
    fn artifact_names_match_packaging() {
        let (os, arch) = platform_key();
        assert!(headless_artifact("0.2.0").starts_with("godspeed-0.2.0-"));
        assert_eq!(
            headless_artifact("0.2.0"),
            format!("godspeed-0.2.0-{os}-{arch}.tar.gz")
        );
        assert!(mac_app_artifact("0.2.0").ends_with("-app.tar.gz"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_is_not_misclassified_as_linux() {
        assert_eq!(platform_key().0, "windows");
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_is_always_unmanaged() {
        assert_eq!(
            detect_install_from(
                Path::new(r"C:\Users\u\.godspeed\app\0.2.0\godspeed.exe"),
                Some(Path::new(r"C:\Users\u")),
            ),
            InstallKind::Unmanaged
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_rejects_platform_specific_updates_before_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            version: "9.9.9".into(),
            files: BTreeMap::new(),
        };
        let app_root = tmp.path().join("app");
        let data_dir = tmp.path().join("data");

        let managed_err = stage_headless("http://127.0.0.1:1", &manifest, &app_root)
            .await
            .unwrap_err();
        assert!(managed_err.to_string().contains("not supported on windows"));
        assert!(!app_root.exists(), "managed staging must not touch disk");

        let mac_err = stage_mac_app("http://127.0.0.1:1", &manifest, &data_dir)
            .await
            .unwrap_err();
        assert!(mac_err.to_string().contains("not supported on windows"));
        assert!(!data_dir.exists(), "macOS staging must not touch disk");

        assert!(
            apply_headless(&app_root, &manifest.version)
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
        assert!(
            apply_mac_app(&data_dir.join("Godspeed.app"), &data_dir.join("Installed.app"))
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
        assert!(
            restart_service()
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
    }

    #[test]
    fn manifest_parses_with_and_without_files() {
        let full: Manifest = serde_json::from_str(
            r#"{"version":"0.1.1","files":{"godspeed-0.1.1-linux-x86_64.tar.gz":{"sha256":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(full.version, "0.1.1");
        assert_eq!(
            full.files["godspeed-0.1.1-linux-x86_64.tar.gz"]
                .sha256
                .as_deref(),
            Some("abc")
        );
        let bare: Manifest = serde_json::from_str(r#"{"version":"0.1.1"}"#).unwrap();
        assert!(bare.files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn headless_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("godspeed"), ver).unwrap();
        }
        apply_headless(&app_root, "0.1.0").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.0")
        );
        // Swap over an existing symlink.
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
    }
    #[test]
    fn schedule_is_wall_clock_with_failure_backoff() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut schedule = Schedule::default();
        assert!(schedule.due(start), "the first check is due immediately");

        schedule.record(start, true);
        assert!(!schedule.due(start + Duration::from_secs(59 * 60)));
        // A machine that slept through the deadline is due the moment it
        // looks at the clock again — no monotonic remainder to wait out.
        assert!(schedule.due(start + Duration::from_secs(9 * 60 * 60)));
        assert_eq!(schedule.wait(start), SCHEDULE_TICK);
        // A clock that jumped backwards does not postpone checks.
        assert!(schedule.due(start - Duration::from_secs(24 * 60 * 60)));

        let mut backoff = Vec::new();
        for _ in 0..6 {
            schedule.record(start, false);
            backoff.push(schedule.next_due.unwrap().duration_since(start).unwrap());
        }
        assert_eq!(
            backoff,
            [
                CHECK_RETRY[0],
                CHECK_RETRY[1],
                CHECK_RETRY[2],
                CHECK_RETRY[3],
                CHECK_RETRY[3],
                CHECK_RETRY[3]
            ]
        );
        schedule.record(start, true);
        assert_eq!(schedule.failures, 0);
    }

    #[test]
    fn superseded_service_restarts_once_when_quiet() {
        let restarts = std::cell::Cell::new(0);
        let restart = || {
            restarts.set(restarts.get() + 1);
            Ok(())
        };
        let mut superseded = SupersededRestart::default();
        // Same or older installed versions never restart.
        superseded.poll_installed("0.2.0", "0.2.0", || true, restart);
        superseded.poll_installed("0.1.9", "0.2.0", || true, restart);
        assert_eq!(restarts.get(), 0);
        // Busy: deferred, not attempted.
        superseded.poll_installed("0.2.1", "0.2.0", || false, restart);
        assert_eq!(restarts.get(), 0);
        // Quiet: exactly one attempt per installed version.
        superseded.poll_installed("0.2.1", "0.2.0", || true, restart);
        superseded.poll_installed("0.2.1", "0.2.0", || true, restart);
        assert_eq!(restarts.get(), 1);
        superseded.poll_installed("0.2.2", "0.2.0", || true, restart);
        assert_eq!(restarts.get(), 2);
    }

    #[test]
    fn systemd_service_cgroup_is_recognized() {
        assert!(in_zeron_service_cgroup(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/godspeed.service\n"
        ));
        assert!(in_zeron_service_cgroup(
            "12:pids:/user.slice/user@1000.service/godspeed.service\n1:name=systemd:/x\n"
        ));
        assert!(!in_zeron_service_cgroup(
            "0::/user.slice/user-1000.slice/session-3.scope\n"
        ));
        assert!(!in_zeron_service_cgroup(
            "0::/user.slice/user@1000.service/app.slice/godspeed.service.d\n"
        ));
    }

    #[test]
    fn bundle_version_reads_the_packaged_plist() {
        let template = include_str!("../../../dist/macos/Info.plist");
        let plist = template.replace("__VERSION__", "0.3.1");
        assert_eq!(bundle_short_version(&plist).as_deref(), Some("0.3.1"));
        assert_eq!(bundle_short_version("<plist></plist>"), None);
    }

    #[cfg(unix)]
    #[test]
    fn installed_version_follows_the_current_symlink_and_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        std::fs::create_dir_all(app_root.join("0.4.0")).unwrap();
        std::fs::write(app_root.join("0.4.0").join("godspeed"), "").unwrap();
        let managed = InstallKind::Managed {
            app_root: app_root.clone(),
        };
        assert_eq!(installed_version(&managed), None, "no current link yet");
        apply_headless(&app_root, "0.4.0").unwrap();
        assert_eq!(installed_version(&managed).as_deref(), Some("0.4.0"));

        let bundle = tmp.path().join("Godspeed.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            include_str!("../../../dist/macos/Info.plist").replace("__VERSION__", "0.4.1"),
        )
        .unwrap();
        assert_eq!(
            installed_version(&InstallKind::MacApp { bundle }).as_deref(),
            Some("0.4.1")
        );
        assert_eq!(installed_version(&InstallKind::Unmanaged), None);
    }

    #[test]
    fn mac_bundles_that_cannot_replace_themselves_are_explained() {
        let writable = |_: &Path| true;
        let read_only = |_: &Path| false;
        assert_eq!(
            mac_bundle_blocker(Path::new("/Applications/Godspeed.app"), writable),
            None
        );
        assert_eq!(
            mac_bundle_blocker(
                Path::new("/private/var/folders/x/T/AppTranslocation/ABC/d/Godspeed.app"),
                writable
            ),
            Some(UpdateBlocker::Translocated)
        );
        assert_eq!(
            mac_bundle_blocker(Path::new("/Volumes/Godspeed/Godspeed.app"), read_only),
            Some(UpdateBlocker::DiskImage)
        );
        // An external drive the user can write to updates in place.
        assert_eq!(
            mac_bundle_blocker(Path::new("/Volumes/Work/Godspeed.app"), writable),
            None
        );
        assert_eq!(
            mac_bundle_blocker(Path::new("/Applications/Godspeed.app"), read_only),
            Some(UpdateBlocker::NotWritable(PathBuf::from("/Applications")))
        );
    }

    #[test]
    fn writability_probe_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_writable(tmp.path()));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        assert!(!dir_writable(&tmp.path().join("missing")));
    }

    /// Serves `body` for every request until aborted.
    async fn serve_forever(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0; 4096];
                    let _ = socket.read(&mut request).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (base, server)
    }

    #[tokio::test]
    async fn desktop_checker_reports_a_newer_release_on_demand() {
        let (base, server) = serve_forever(br#"{"version":"999.0.0","files":{}}"#.to_vec()).await;
        let updater = Updater::spawn_desktop(base);
        let status = updater.check().await.unwrap();
        assert!(status.update_available);
        assert_eq!(status.latest_version.as_deref(), Some("999.0.0"));
        assert_eq!(status.current_version, current_version());
        assert!(status.checked_at.is_some());
        assert_eq!(*updater.watch().borrow(), status);
        updater.shutdown().await;
        server.abort();
    }

    /// A headless tarball whose `godspeed` reports `reported` from `--version`.
    #[cfg(unix)]
    fn fake_headless_tarball(dir: &Path, reported: &str) -> Vec<u8> {
        use std::os::unix::fs::PermissionsExt as _;
        let root = dir.join("godspeed-pkg");
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("godspeed");
        std::fs::write(&binary, format!("#!/bin/sh\necho \"godspeed {reported}\"\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tarball = dir.join("pkg.tar.gz");
        run(
            "tar",
            &[
                "-czf",
                &tarball.to_string_lossy(),
                "-C",
                &dir.to_string_lossy(),
                "godspeed-pkg",
            ],
        )
        .unwrap();
        std::fs::read(tarball).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn headless_staging_rejects_a_binary_that_is_not_the_promised_release() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let manifest = |bytes: &[u8]| Manifest {
            version: "9.9.9".into(),
            files: [(
                headless_artifact("9.9.9"),
                FileMeta {
                    sha256: Some(format!("{:x}", Sha256::digest(bytes))),
                    size: Some(bytes.len() as u64),
                },
            )]
            .into(),
        };

        let wrong = fake_headless_tarball(&tmp.path().join("wrong"), "1.0.0");
        let (base, server) = serve_forever(wrong.clone()).await;
        let error = stage_headless(&base, &manifest(&wrong), &app_root)
            .await
            .unwrap_err();
        server.abort();
        assert!(
            format!("{error:#}").contains("expected \"godspeed 9.9.9\""),
            "unexpected error: {error:#}"
        );
        assert!(!app_root.join("9.9.9").exists());
        assert_eq!(std::fs::read_dir(&app_root).unwrap().count(), 0);

        let right = fake_headless_tarball(&tmp.path().join("right"), "9.9.9");
        let (base, server) = serve_forever(right.clone()).await;
        let staged = stage_headless(&base, &manifest(&right), &app_root)
            .await
            .unwrap();
        server.abort();
        assert_eq!(staged, app_root.join("9.9.9"));
        assert!(staged.join("godspeed").is_file());
    }
}
