//! The public ACP agent registry (<https://agentclientprotocol.com>): parse
//! and validate its index, pick an install route for this device, and
//! install, list and uninstall agents in Godspeed's managed adapters dir.
//!
//! Registry content is UNTRUSTED data. The index is size-bounded and every
//! agent is validated field by field the way T3 Code's
//! `AcpRegistrySupport` does (bounded ids, names, versions and arguments;
//! HTTPS-only URLs without credentials; exact-version npm packages); an agent
//! that fails validation is dropped, never partially used. On top of T3's
//! rules, nothing runs unless it is pinned to a checksum:
//!
//! - **binary** builds install only when the registry pins their SHA-256;
//!   the download is hashed while it streams and a mismatch discards it.
//!   T3 accepts a binary without a digest — Godspeed refuses it.
//! - **npm** packages install at their exact registry version with
//!   `--ignore-scripts` (no lifecycle script ever runs), and the installed
//!   tarball's integrity (npm's lockfile `integrity`) must equal the sha512
//!   npm publishes for that exact version.
//! - **uvx** (Python) agents are listed but not installable: there is no
//!   checksum-pinned uv route yet.
//!
//! Layout: `<adapters>/acp-registry/<agent-id>/<version>/`, built in a
//! `.tmp-*` sibling and renamed into place only after the entry resolves
//! and the receipt (`godspeed-agent.json`) plus the shared OK marker are
//! written — the same contract as [`crate::archive_install`].

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use zeron_proto::{AcpAgentId, AcpDistributionKind, HarnessId};

use crate::HarnessError;
use crate::adapter_install::OK_MARKER;

/// The index T3 Code reads (`ACP_REGISTRY_URL`).
pub const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
pub const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
pub const MAX_AGENTS: usize = 512;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 50_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const RECEIPT: &str = "godspeed-agent.json";
const NPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

// ── Index ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryIndex {
    pub version: String,
    pub agents: Vec<RegistryAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub binary: BTreeMap<String, BinaryTarget>,
    pub npx: Option<PackageDistribution>,
    pub uvx: Option<PackageDistribution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTarget {
    pub archive: String,
    /// The command's path inside the archive, as normalized segments.
    pub cmd: Vec<String>,
    pub sha256: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDistribution {
    pub package: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawIndex {
    version: String,
    agents: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackage {
    package: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinary {
    archive: String,
    cmd: String,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawDistribution {
    #[serde(default)]
    binary: Option<BTreeMap<String, RawBinary>>,
    #[serde(default)]
    npx: Option<RawPackage>,
    #[serde(default)]
    uvx: Option<RawPackage>,
}

#[derive(Deserialize)]
struct RawAgent {
    id: String,
    name: String,
    version: String,
    description: String,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    distribution: RawDistribution,
}

/// Parse and validate a registry index. The envelope must be well formed;
/// individual agents that fail validation are dropped.
pub fn parse_index(bytes: &[u8]) -> Result<RegistryIndex, HarnessError> {
    if bytes.len() > MAX_REGISTRY_BYTES {
        return Err(HarnessError::Install(format!(
            "the ACP registry index exceeds {} KiB",
            MAX_REGISTRY_BYTES / 1024
        )));
    }
    let raw: RawIndex = serde_json::from_slice(bytes)
        .map_err(|error| HarnessError::Install(format!("invalid ACP registry index: {error}")))?;
    if !is_bounded_version(&raw.version) {
        return Err(HarnessError::Install(
            "invalid ACP registry index version".into(),
        ));
    }
    if raw.agents.len() > MAX_AGENTS {
        return Err(HarnessError::Install(format!(
            "the ACP registry lists more than {MAX_AGENTS} agents"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    let agents = raw
        .agents
        .into_iter()
        .filter_map(|value| validate_agent(value).ok())
        .filter(|agent| seen.insert(agent.id.clone()))
        .collect();
    Ok(RegistryIndex {
        version: raw.version,
        agents,
    })
}

fn validate_agent(value: serde_json::Value) -> Result<RegistryAgent, String> {
    let raw: RawAgent = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if !zeron_proto::agent::is_valid_acp_agent_id(&raw.id) {
        return Err("id".into());
    }
    let name = raw.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 160 {
        return Err("name".into());
    }
    if !is_bounded_version(&raw.version) {
        return Err("version".into());
    }
    if raw.description.chars().count() > 1024 {
        return Err("description".into());
    }
    if raw.authors.len() > 16 || raw.authors.iter().any(|a| a.chars().count() > 256) {
        return Err("authors".into());
    }
    if raw.license.as_ref().is_some_and(|l| l.chars().count() > 128) {
        return Err("license".into());
    }
    for url in [&raw.website, &raw.repository, &raw.icon]
        .into_iter()
        .flatten()
    {
        if !is_https_url(url) {
            return Err("url".into());
        }
    }
    let mut binary = BTreeMap::new();
    for (target, raw_target) in raw.distribution.binary.unwrap_or_default() {
        if target.len() > 64 {
            return Err("target".into());
        }
        if !is_https_url(&raw_target.archive) {
            return Err("archive url".into());
        }
        let cmd = normalize_command_path(&raw_target.cmd).ok_or("cmd")?;
        let sha256 = match raw_target.sha256 {
            Some(digest) if is_sha256(&digest) => Some(digest.to_ascii_lowercase()),
            Some(_) => return Err("sha256".into()),
            None => None,
        };
        validate_args(&raw_target.args)?;
        validate_env(&raw_target.env)?;
        binary.insert(
            target,
            BinaryTarget {
                archive: raw_target.archive,
                cmd,
                sha256,
                args: raw_target.args,
                env: raw_target.env,
            },
        );
    }
    let package = |raw: Option<RawPackage>, exact: fn(&str) -> bool| {
        raw.map(|raw| {
            if raw.package.len() > 256 || !exact(&raw.package) {
                return Err("package".to_string());
            }
            validate_args(&raw.args)?;
            validate_env(&raw.env)?;
            Ok(PackageDistribution {
                package: raw.package,
                args: raw.args,
                env: raw.env,
            })
        })
        .transpose()
    };
    let npx = package(raw.distribution.npx, is_exact_npm_spec)?;
    let uvx = package(raw.distribution.uvx, is_exact_uv_spec)?;
    if binary.is_empty() && npx.is_none() && uvx.is_none() {
        return Err("no distribution".into());
    }
    Ok(RegistryAgent {
        id: raw.id,
        name,
        version: raw.version,
        description: raw.description,
        authors: raw.authors,
        license: raw.license,
        website: raw.website,
        repository: raw.repository,
        binary,
        npx,
        uvx,
    })
}

/// Versions become directory names: `^[A-Za-z0-9][A-Za-z0-9._+-]*$`, ≤128.
fn is_bounded_version(version: &str) -> bool {
    let bytes = version.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_https_url(url: &str) -> bool {
    url.len() <= 2048
        && reqwest::Url::parse(url)
            .is_ok_and(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none() && u.host_str().is_some())
}

fn validate_args(args: &[String]) -> Result<(), String> {
    if args.len() > 64 || args.iter().any(|a| a.len() > 1024 || a.contains('\0')) {
        return Err("args".into());
    }
    Ok(())
}

/// Registry env is untrusted: only well-formed names, never the loader or
/// process-plumbing variables an agent launch must not inherit from data.
fn validate_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    if env.len() > 64 {
        return Err("env".into());
    }
    for (key, value) in env {
        if !zeron_proto::provider_instance::is_valid_env_key(key)
            || zeron_proto::provider_instance::is_reserved_env_key(key)
            || value.len() > 1024
            || value.contains('\0')
        {
            return Err("env".into());
        }
    }
    Ok(())
}

fn exact_version(version: &str) -> bool {
    let version = version.strip_prefix('v').unwrap_or(version);
    let (core, _) = version.split_once(['-', '+']).unwrap_or((version, ""));
    let mut parts = core.split('.');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    numeric(parts.next())
        && numeric(parts.next())
        && numeric(parts.next())
        && parts.next().is_none()
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

fn is_package_name(name: &str) -> bool {
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            && !part.starts_with('.')
    };
    match name.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, rest)| valid(scope) && valid(rest) && !rest.contains('/')),
        None => valid(name) && !name.contains('/'),
    }
}

/// `name@1.2.3` / `@scope/name@1.2.3` with an exact semver.
pub fn is_exact_npm_spec(spec: &str) -> bool {
    split_npm_spec(spec).is_some()
}

/// Split an exact npm spec at its last `@`.
pub fn split_npm_spec(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@').filter(|at| *at > 0)?;
    let (name, version) = (&spec[..at], &spec[at + 1..]);
    (is_package_name(name) && exact_version(version)).then_some((name, version))
}

fn is_exact_uv_spec(spec: &str) -> bool {
    let (name, version) = match spec.rfind("==") {
        Some(at) => (&spec[..at], &spec[at + 2..]),
        None => match spec.rfind('@') {
            Some(at) if at > 0 => (&spec[..at], &spec[at + 1..]),
            _ => return false,
        },
    };
    is_package_name(name) && exact_version(version)
}

/// `./bin/agent` → `["bin", "agent"]`; absolute paths, `..`, empty or
/// backslash-separated tricks are rejected.
fn normalize_command_path(cmd: &str) -> Option<Vec<String>> {
    if cmd.len() > 1024 || cmd.contains('\0') || cmd.contains('\\') {
        return None;
    }
    let mut segments = Vec::new();
    for component in Path::new(cmd).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => segments.push(part.to_str()?.to_string()),
            _ => return None,
        }
    }
    (!segments.is_empty()).then_some(segments)
}

// ── Install routes ─────────────────────────────────────────────────────────

/// The registry's platform key for this build (`darwin-aarch64`, …).
pub fn platform_target() -> Option<&'static str> {
    Some(if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-aarch64"
    } else {
        return None;
    })
}

fn target_candidates(target: &str) -> Vec<&str> {
    // Windows on ARM emulates x86_64 builds when no native one exists.
    if target == "windows-aarch64" {
        vec!["windows-aarch64", "windows-x86_64"]
    } else {
        vec![target]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
    TarBz2,
    /// The download IS the executable.
    Raw,
}

fn archive_format(url: &str) -> ArchiveFormat {
    let path = reqwest::Url::parse(url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_default();
    if path.ends_with(".zip") {
        ArchiveFormat::Zip
    } else if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        ArchiveFormat::TarGz
    } else if path.ends_with(".tar.bz2") || path.ends_with(".tbz2") {
        ArchiveFormat::TarBz2
    } else {
        ArchiveFormat::Raw
    }
}

/// How one agent installs on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlan {
    Binary {
        archive: String,
        sha256: String,
        format: ArchiveFormat,
        cmd: Vec<String>,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Npm {
        name: String,
        version: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

impl InstallPlan {
    pub fn kind(&self) -> AcpDistributionKind {
        match self {
            InstallPlan::Binary { .. } => AcpDistributionKind::Binary,
            InstallPlan::Npm { .. } => AcpDistributionKind::Npm,
        }
    }
}

/// Registry entries for agents Godspeed already drives natively; they are
/// listed but never installed a second time.
pub fn built_in_equivalent(id: &str) -> Option<HarnessId> {
    Some(match id {
        "claude-acp" => HarnessId::ClaudeCode,
        "codex-acp" => HarnessId::Codex,
        "cursor" => HarnessId::Cursor,
        "devin" => HarnessId::Devin,
        "grok-build" => HarnessId::Grok,
        "antigravity-acp" => HarnessId::Antigravity,
        "opencode" | "opencode-v2" => HarnessId::Opencode,
        "pi-acp" => HarnessId::Pi,
        _ => return None,
    })
}

/// Pick this platform's install route, or say why there is none. A binary
/// build pinned by SHA-256 wins; otherwise the exact-version npm package.
pub fn plan_install(agent: &RegistryAgent, target: Option<&str>) -> Result<InstallPlan, String> {
    let mut unpinned_binary = false;
    let mut unsupported_format = false;
    if let Some(target) = target {
        for candidate in target_candidates(target) {
            let Some(build) = agent.binary.get(candidate) else {
                continue;
            };
            let Some(sha256) = &build.sha256 else {
                unpinned_binary = true;
                continue;
            };
            let format = archive_format(&build.archive);
            if format == ArchiveFormat::Raw && build.cmd.len() != 1 {
                unsupported_format = true;
                continue;
            }
            return Ok(InstallPlan::Binary {
                archive: build.archive.clone(),
                sha256: sha256.clone(),
                format,
                cmd: build.cmd.clone(),
                args: build.args.clone(),
                env: build.env.clone(),
            });
        }
    }
    if let Some(npx) = &agent.npx
        && let Some((name, version)) = split_npm_spec(&npx.package)
    {
        return Ok(InstallPlan::Npm {
            name: name.to_string(),
            version: version.trim_start_matches('v').to_string(),
            args: npx.args.clone(),
            env: npx.env.clone(),
        });
    }
    Err(if unpinned_binary {
        "The registry pins no checksum for this platform's build".into()
    } else if unsupported_format {
        "This platform's build uses an unsupported archive layout".into()
    } else if agent.uvx.is_some() && agent.binary.is_empty() {
        "Python (uvx) agents are not supported yet".into()
    } else {
        "No build for this platform".into()
    })
}

// ── Receipts and installed agents ──────────────────────────────────────────

/// What a completed install records, and everything a launch needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReceipt {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub kind: AcpDistributionKind,
    /// The entry, relative to the install dir (a native binary or a JS bin).
    pub entry: Vec<String>,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `sha256:<hex>` (binary) or npm's `sha512-…` integrity.
    pub integrity: String,
    pub installed_at: i64,
}

/// A completed install on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledAgent {
    pub receipt: InstallReceipt,
    pub dir: PathBuf,
}

impl InstalledAgent {
    pub fn entry_path(&self) -> PathBuf {
        self.receipt
            .entry
            .iter()
            .fold(self.dir.clone(), |path, part| path.join(part))
    }

    pub fn agent_id(&self) -> Option<AcpAgentId> {
        AcpAgentId::new(&self.receipt.agent_id).ok()
    }
}

/// `<adapters>/acp-registry`.
pub fn default_root() -> Option<PathBuf> {
    crate::adapter_install::adapters_root().map(|root| root.join("acp-registry"))
}

/// Every completed install under `root`, newest version per agent.
pub fn installed_agents(root: &Path) -> Vec<InstalledAgent> {
    let Ok(agents) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for agent in agents.flatten() {
        let Ok(name) = agent.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || !zeron_proto::agent::is_valid_acp_agent_id(&name) {
            continue;
        }
        let newest = std::fs::read_dir(agent.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|version| read_install(&version.path(), &name))
            .max_by_key(|installed| installed.receipt.installed_at);
        out.extend(newest);
    }
    out.sort_by(|a, b| a.receipt.agent_id.cmp(&b.receipt.agent_id));
    out
}

/// The completed install of one agent, if any.
pub fn installed_agent(root: &Path, agent_id: &str) -> Option<InstalledAgent> {
    installed_agents(root)
        .into_iter()
        .find(|installed| installed.receipt.agent_id == agent_id)
}

fn read_install(dir: &Path, agent_id: &str) -> Option<InstalledAgent> {
    if !dir.join(OK_MARKER).is_file() {
        return None;
    }
    let receipt: InstallReceipt =
        serde_json::from_slice(&std::fs::read(dir.join(RECEIPT)).ok()?).ok()?;
    if receipt.agent_id != agent_id
        || dir.file_name()?.to_str()? != receipt.version
        || receipt.entry.iter().any(|part| {
            part.is_empty() || part == ".." || part.contains(['/', '\\']) || part == "."
        })
    {
        return None;
    }
    let installed = InstalledAgent {
        receipt,
        dir: dir.to_path_buf(),
    };
    installed.entry_path().is_file().then_some(installed)
}

/// Remove every installed version of an agent.
pub fn uninstall(root: &Path, agent_id: &str) -> Result<bool, HarnessError> {
    if !zeron_proto::agent::is_valid_acp_agent_id(agent_id) {
        return Err(HarnessError::Install(format!("invalid agent id {agent_id:?}")));
    }
    let dir = root.join(agent_id);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// Remove every version of `agent_id` except `keep`.
fn prune_other_versions(root: &Path, agent_id: &str, keep: &str) {
    let Ok(entries) = std::fs::read_dir(root.join(agent_id)) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str() != Some(keep) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The program, arguments and env that launch an installed agent over ACP.
pub fn launch_command(
    installed: &InstalledAgent,
) -> Result<(PathBuf, Vec<String>, BTreeMap<String, String>), HarnessError> {
    let entry = installed.entry_path();
    let (program, mut args) = match installed.receipt.kind {
        AcpDistributionKind::Binary => (entry, Vec::new()),
        AcpDistributionKind::Npm => crate::adapter_install::launch_for_entry(&entry)?,
    };
    args.extend(installed.receipt.args.iter().cloned());
    Ok((program, args, installed.receipt.env.clone()))
}

// ── Installing ─────────────────────────────────────────────────────────────

/// Fetches an artifact into a file, returning its SHA-256 (hex). Production
/// streams over HTTPS; tests serve local bytes.
#[async_trait::async_trait]
pub trait Downloader: Send + Sync {
    async fn download(&self, url: &str, dest: &Path, max_bytes: u64) -> Result<String, HarnessError>;
}

/// HTTPS streaming downloader with a stall bound and a size cap.
pub struct HttpsDownloader;

#[async_trait::async_trait]
impl Downloader for HttpsDownloader {
    async fn download(&self, url: &str, dest: &Path, max_bytes: u64) -> Result<String, HarnessError> {
        if !is_https_url(url) {
            return Err(HarnessError::Install(format!("refusing non-HTTPS download {url}")));
        }
        let failed =
            |detail: String| HarnessError::Install(format!("download of {url} failed: {detail}"));
        let client = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| failed(e.to_string()))?;
        let response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| failed(e.to_string()))?;
        if response.content_length().is_some_and(|len| len > max_bytes) {
            return Err(failed("the archive exceeds the size limit".into()));
        }
        let mut body = response.bytes_stream();
        let mut file = tokio::fs::File::create(dest).await?;
        let mut digest = Sha256::new();
        let mut received = 0_u64;
        loop {
            let chunk = tokio::time::timeout(STALL_TIMEOUT, body.next())
                .await
                .map_err(|_| failed(format!("no data for {}s", STALL_TIMEOUT.as_secs())))?;
            match chunk {
                Some(Ok(bytes)) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > max_bytes {
                        return Err(failed("the archive exceeds the size limit".into()));
                    }
                    digest.update(&bytes);
                    file.write_all(&bytes).await?;
                }
                Some(Err(e)) => return Err(failed(e.to_string())),
                None => break,
            }
        }
        file.flush().await?;
        Ok(format!("{:x}", digest.finalize()))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Install `agent` into `root` along `plan`, replacing older versions once
/// the new one is complete. Returns the completed install.
pub async fn install(
    root: &Path,
    agent: &RegistryAgent,
    plan: &InstallPlan,
    downloader: &dyn Downloader,
) -> Result<InstalledAgent, HarnessError> {
    if !zeron_proto::agent::is_valid_acp_agent_id(&agent.id) || !is_bounded_version(&agent.version)
    {
        return Err(HarnessError::Install("invalid registry agent".into()));
    }
    let _guard = crate::adapter_install::install_lock().lock().await;
    let final_dir = root.join(&agent.id).join(&agent.version);
    if let Some(existing) = read_install(&final_dir, &agent.id) {
        prune_other_versions(root, &agent.id, &agent.version);
        return Ok(existing);
    }
    std::fs::create_dir_all(root)?;
    let tmp = root.join(format!(
        ".tmp-{}-{}-{}",
        agent.id,
        agent.version,
        uuid::Uuid::new_v4().simple()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let built = match plan {
        InstallPlan::Binary { .. } => install_binary(&tmp, agent, plan, downloader).await,
        InstallPlan::Npm { .. } => install_npm(&tmp, agent, plan, downloader).await,
    };
    let receipt = match built {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(error);
        }
    };
    let commit = (|| -> Result<(), HarnessError> {
        std::fs::write(
            tmp.join(RECEIPT),
            serde_json::to_vec_pretty(&receipt).map_err(|e| HarnessError::Install(e.to_string()))?,
        )?;
        std::fs::write(tmp.join(OK_MARKER), format!("{}\n", receipt.integrity))?;
        if let Some(parent) = final_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if final_dir.exists() {
            std::fs::remove_dir_all(&final_dir)?;
        }
        std::fs::rename(&tmp, &final_dir)?;
        Ok(())
    })();
    if let Err(error) = commit {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(error);
    }
    prune_other_versions(root, &agent.id, &agent.version);
    read_install(&final_dir, &agent.id).ok_or_else(|| {
        HarnessError::Install(format!("{} installed but its entry did not resolve", agent.name))
    })
}

async fn install_binary(
    dir: &Path,
    agent: &RegistryAgent,
    plan: &InstallPlan,
    downloader: &dyn Downloader,
) -> Result<InstallReceipt, HarnessError> {
    let InstallPlan::Binary {
        archive,
        sha256,
        format,
        cmd,
        args,
        env,
    } = plan
    else {
        unreachable!("binary plan");
    };
    let download = dir.join(".download");
    let actual = downloader
        .download(archive, &download, MAX_ARCHIVE_BYTES)
        .await?;
    if !actual.eq_ignore_ascii_case(sha256) {
        return Err(HarnessError::Install(format!(
            "{} checksum mismatch: the registry pins SHA-256 {sha256}, the download is {actual}",
            agent.name
        )));
    }
    let (download_c, dir_c, format, cmd_c) = (download.clone(), dir.to_path_buf(), *format, cmd.clone());
    tokio::task::spawn_blocking(move || -> Result<(), HarnessError> {
        match format {
            ArchiveFormat::Zip => extract_zip(&download_c, &dir_c),
            ArchiveFormat::TarGz => extract_tar(
                flate2::read::GzDecoder::new(std::fs::File::open(&download_c)?),
                &dir_c,
            ),
            ArchiveFormat::TarBz2 => extract_tar(
                bzip2::read::BzDecoder::new(std::fs::File::open(&download_c)?),
                &dir_c,
            ),
            ArchiveFormat::Raw => {
                std::fs::rename(&download_c, dir_c.join(&cmd_c[0]))?;
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| HarnessError::Install(format!("extraction task failed: {e}")))??;
    let _ = std::fs::remove_file(&download);
    let entry = cmd.iter().fold(dir.to_path_buf(), |path, part| path.join(part));
    let meta = std::fs::symlink_metadata(&entry).map_err(|_| {
        HarnessError::Install(format!(
            "the {} archive has no {}",
            agent.name,
            cmd.join("/")
        ))
    })?;
    if !meta.is_file() {
        return Err(HarnessError::Install(format!(
            "{} in the {} archive is not a regular file",
            cmd.join("/"),
            agent.name
        )));
    }
    mark_executable(&entry)?;
    Ok(InstallReceipt {
        agent_id: agent.id.clone(),
        name: agent.name.clone(),
        version: agent.version.clone(),
        kind: AcpDistributionKind::Binary,
        entry: cmd.clone(),
        args: args.clone(),
        env: env.clone(),
        integrity: format!("sha256:{}", sha256.to_ascii_lowercase()),
        installed_at: now_ms(),
    })
}

fn checked_output(dest: &Path, name: &Path) -> Result<PathBuf, HarnessError> {
    let mut out = dest.to_path_buf();
    let mut depth = 0;
    for component in name.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                depth += 1;
            }
            Component::CurDir => {}
            _ => {
                return Err(HarnessError::Install(format!(
                    "archive entry {} escapes the install dir",
                    name.display()
                )));
            }
        }
    }
    if depth == 0 {
        return Ok(dest.to_path_buf());
    }
    Ok(out)
}

/// Extract a tar stream: regular files and directories only (links, devices
/// and FIFOs are refused), every path confined to `dest`, bounded in entry
/// count and expanded size.
pub fn extract_tar(reader: impl Read, dest: &Path) -> Result<(), HarnessError> {
    let mut archive = tar::Archive::new(reader);
    let mut total = 0_u64;
    let invalid = |e: std::io::Error| HarnessError::Install(format!("invalid tar archive: {e}"));
    for (count, entry) in archive.entries().map_err(invalid)?.enumerate() {
        if count >= MAX_ARCHIVE_ENTRIES {
            return Err(HarnessError::Install("archive has too many entries".into()));
        }
        let mut entry = entry.map_err(invalid)?;
        let kind = entry.header().entry_type();
        let path = entry.path().map_err(invalid)?.into_owned();
        let out = checked_output(dest, &path)?;
        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() || kind.is_gnu_longname()
        {
            continue;
        }
        if !kind.is_file() {
            // Symlinks and hard links could point the entry outside `dest`.
            return Err(HarnessError::Install(format!(
                "archive entry {} is a link or special file",
                path.display()
            )));
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_EXTRACTED_BYTES {
            return Err(HarnessError::Install("archive expands beyond the size limit".into()));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mode = entry.header().mode().unwrap_or(0o644);
        let mut file = std::fs::File::create(&out)?;
        std::io::copy(&mut (&mut entry).take(size), &mut file)?;
        file.flush()?;
        #[cfg(unix)]
        if mode & 0o111 != 0 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    Ok(())
}

fn extract_zip(archive_path: &Path, dest: &Path) -> Result<(), HarnessError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| HarnessError::Install(format!("invalid zip archive: {e}")))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(HarnessError::Install("archive has too many entries".into()));
    }
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| HarnessError::Install(format!("invalid zip entry: {e}")))?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| HarnessError::Install("archive contains an unsafe entry path".into()))?;
        let out = checked_output(dest, &name)?;
        if let Some(mode) = entry.unix_mode()
            && !matches!(mode & 0o170000, 0 | 0o040000 | 0o100000)
        {
            return Err(HarnessError::Install(format!(
                "archive entry {} is a link or special file",
                name.display()
            )));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err(HarnessError::Install("archive expands beyond the size limit".into()));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        let mut file = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut file)?;
        file.flush()?;
        if executable {
            mark_executable(&out)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Whether npm resolves on this device (the registry's npm route needs it).
pub fn npm_available() -> bool {
    crate::adapter_install::find_npm().is_some()
}

/// npm's published metadata for one exact version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmVersionMeta {
    pub integrity: String,
    pub unpacked_size: Option<u64>,
}

fn npm_metadata_url(name: &str, version: &str) -> String {
    format!(
        "https://registry.npmjs.org/{}/{}",
        name.replacen('/', "%2f", 1),
        version
    )
}

/// Fetch npm's `dist.integrity` (sha512) and unpacked size for `name@version`.
pub async fn npm_version_meta(name: &str, version: &str) -> Result<NpmVersionMeta, HarnessError> {
    let failed = |detail: String| {
        HarnessError::Install(format!("npm metadata for {name}@{version}: {detail}"))
    };
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| failed(e.to_string()))?;
    let response = client
        .get(npm_metadata_url(name, version))
        .header("accept", "application/json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| failed(e.to_string()))?;
    let bytes = response.bytes().await.map_err(|e| failed(e.to_string()))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(failed("metadata too large".into()));
    }
    parse_npm_version_meta(&bytes, name, version).map_err(failed)
}

pub fn parse_npm_version_meta(
    bytes: &[u8],
    name: &str,
    version: &str,
) -> Result<NpmVersionMeta, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if value.get("name").and_then(|v| v.as_str()) != Some(name)
        || value.get("version").and_then(|v| v.as_str()) != Some(version)
    {
        return Err("metadata names a different package".into());
    }
    let dist = value.get("dist").ok_or("no dist")?;
    let integrity = dist
        .get("integrity")
        .and_then(|v| v.as_str())
        .filter(|i| i.starts_with("sha512-"))
        .ok_or("no sha512 integrity")?
        .to_string();
    Ok(NpmVersionMeta {
        integrity,
        unpacked_size: dist.get("unpackedSize").and_then(|v| v.as_u64()),
    })
}

/// The integrity npm recorded for the installed package (its hidden
/// lockfile), which must equal the published sha512.
pub fn installed_npm_integrity(prefix: &Path, name: &str) -> Option<String> {
    let lock: serde_json::Value = serde_json::from_slice(
        &std::fs::read(prefix.join("node_modules").join(".package-lock.json")).ok()?,
    )
    .ok()?;
    lock.get("packages")?
        .get(format!("node_modules/{name}"))?
        .get("integrity")?
        .as_str()
        .map(str::to_owned)
}

/// The bin entry of an npm package installed under `prefix`, relative to it.
pub fn npm_bin_entry(prefix: &Path, name: &str, agent_id: &str) -> Option<Vec<String>> {
    let pkg_dir = name
        .split('/')
        .fold(prefix.join("node_modules"), |path, part| path.join(part));
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pkg_dir.join("package.json")).ok()?).ok()?;
    let base = name.rsplit('/').next().unwrap_or(name);
    let rel = match manifest.get("bin")? {
        serde_json::Value::String(entry) => entry.clone(),
        serde_json::Value::Object(map) => {
            let pick = if map.len() == 1 {
                map.values().next()
            } else {
                map.get(base)
                    .or_else(|| map.get(agent_id))
                    .or_else(|| map.get(base.trim_end_matches("-acp")))
            };
            pick?.as_str()?.to_string()
        }
        _ => return None,
    };
    let rel_parts = normalize_command_path(&rel)?;
    let mut parts: Vec<String> = vec!["node_modules".into()];
    parts.extend(name.split('/').map(str::to_owned));
    parts.extend(rel_parts);
    let path = parts.iter().fold(prefix.to_path_buf(), |p, s| p.join(s));
    path.is_file().then_some(parts)
}

async fn install_npm(
    dir: &Path,
    agent: &RegistryAgent,
    plan: &InstallPlan,
    _downloader: &dyn Downloader,
) -> Result<InstallReceipt, HarnessError> {
    let InstallPlan::Npm {
        name,
        version,
        args,
        env,
    } = plan
    else {
        unreachable!("npm plan");
    };
    let published = npm_version_meta(name, version).await?;
    let npm = crate::adapter_install::find_npm().ok_or_else(|| {
        HarnessError::NotInstalled(format!(
            "npm (required to install {}; searched PATH, the login shell's PATH, and \
             fnm/nvm/volta/pnpm/bun install dirs)",
            agent.name
        ))
    })?;
    let cache = dir
        .parent()
        .map(|root| root.join(".npm-cache"))
        .unwrap_or_else(|| dir.join(".npm-cache"));
    crate::adapter_install::npm_install_exact(&npm, name, version, dir, &cache, &agent.name, true)
        .await?;
    let installed = installed_npm_integrity(dir, name).ok_or_else(|| {
        HarnessError::Install(format!(
            "npm installed {name}@{version} but recorded no integrity for it"
        ))
    })?;
    if installed != published.integrity {
        return Err(HarnessError::Install(format!(
            "{name}@{version} integrity mismatch: npm publishes {}, the install has {installed}",
            published.integrity
        )));
    }
    let entry = npm_bin_entry(dir, name, &agent.id).ok_or_else(|| {
        HarnessError::Install(format!("{name}@{version} has no runnable bin entry"))
    })?;
    Ok(InstallReceipt {
        agent_id: agent.id.clone(),
        name: agent.name.clone(),
        version: agent.version.clone(),
        kind: AcpDistributionKind::Npm,
        entry,
        args: args.clone(),
        env: env.clone(),
        integrity: published.integrity,
        installed_at: now_ms(),
    })
}

/// Fetch the registry index over HTTPS (bounded, 30 s).
pub async fn fetch_index_bytes(url: &str) -> Result<Vec<u8>, HarnessError> {
    if !is_https_url(url) {
        return Err(HarnessError::Install("the registry URL must be HTTPS".into()));
    }
    let failed = |detail: String| HarnessError::Install(format!("ACP registry fetch failed: {detail}"));
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| failed(e.to_string()))?;
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| failed(e.to_string()))?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_REGISTRY_BYTES as u64)
    {
        return Err(failed("index too large".into()));
    }
    let mut body = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| failed(e.to_string()))?;
        if out.len() + chunk.len() > MAX_REGISTRY_BYTES {
            return Err(failed("index too large".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Best-effort download size of a binary build (`HEAD` Content-Length).
pub async fn archive_size(url: &str) -> Option<u64> {
    if !is_https_url(url) {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let response = client.head(url).send().await.ok()?.error_for_status().ok()?;
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/acp_registry.json");

    fn agent<'a>(index: &'a RegistryIndex, id: &str) -> &'a RegistryAgent {
        index.agents.iter().find(|a| a.id == id).unwrap()
    }

    #[test]
    fn parses_the_fixture_and_drops_invalid_agents() {
        let index = parse_index(FIXTURE.as_bytes()).unwrap();
        let ids: Vec<&str> = index.agents.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"factory-droid"));
        assert!(ids.contains(&"github-copilot-cli"));
        assert!(ids.contains(&"gemini"));
        assert!(ids.contains(&"kimi"));
        for dropped in [
            "bad-http-archive",
            "bad-floating-npm",
            "bad-path-escape",
            "bad-env-loader",
            "Bad-Id",
            "bad-sha",
        ] {
            assert!(!ids.contains(&dropped), "{dropped} should be rejected");
        }
        let kimi = agent(&index, "kimi");
        assert_eq!(kimi.binary["darwin-aarch64"].cmd, vec!["kimi".to_string()]);
    }

    #[test]
    fn oversized_and_malformed_indexes_are_rejected() {
        assert!(parse_index(b"{\"agents\":[]}").is_err());
        assert!(parse_index(&vec![b' '; MAX_REGISTRY_BYTES + 1]).is_err());
        assert!(parse_index(b"{\"version\":\"../x\",\"agents\":[]}").is_err());
    }

    #[test]
    fn plans_prefer_pinned_binaries_then_npm() {
        let index = parse_index(FIXTURE.as_bytes()).unwrap();
        let kimi = plan_install(agent(&index, "kimi"), Some("darwin-aarch64")).unwrap();
        assert!(matches!(kimi, InstallPlan::Binary { format: ArchiveFormat::TarGz, .. }));
        assert_eq!(
            plan_install(agent(&index, "kimi"), Some("darwin-x86_64")).unwrap_err(),
            "No build for this platform"
        );
        let droid = plan_install(agent(&index, "factory-droid"), Some("darwin-aarch64")).unwrap();
        assert_eq!(
            droid,
            InstallPlan::Npm {
                name: "droid".into(),
                version: "0.251.0".into(),
                args: vec!["exec".into(), "--output-format".into(), "acp-daemon".into()],
                env: [
                    ("DROID_DISABLE_AUTO_UPDATE".to_string(), "true".to_string()),
                    ("FACTORY_DROID_AUTO_UPDATE_ENABLED".to_string(), "false".to_string()),
                ]
                .into(),
            }
        );
        let copilot = plan_install(agent(&index, "github-copilot-cli"), Some("linux-x86_64")).unwrap();
        assert!(matches!(copilot, InstallPlan::Npm { ref name, .. } if name == "@github/copilot"));
        assert_eq!(
            plan_install(agent(&index, "unpinned-binary"), Some("darwin-aarch64")).unwrap_err(),
            "The registry pins no checksum for this platform's build"
        );
        assert_eq!(
            plan_install(agent(&index, "python-only"), Some("darwin-aarch64")).unwrap_err(),
            "Python (uvx) agents are not supported yet"
        );
        // Windows on ARM falls back to the x86_64 build.
        assert!(plan_install(agent(&index, "kimi"), Some("windows-aarch64")).is_ok());
    }

    #[test]
    fn npm_specs_must_be_exact() {
        assert_eq!(split_npm_spec("@google/gemini-cli@0.66.0"), Some(("@google/gemini-cli", "0.66.0")));
        assert_eq!(split_npm_spec("droid@0.251.0"), Some(("droid", "0.251.0")));
        assert!(split_npm_spec("droid@latest").is_none());
        assert!(split_npm_spec("droid@^1.2.3").is_none());
        assert!(split_npm_spec("droid").is_none());
        assert!(split_npm_spec("@scope/../x@1.0.0").is_none());
    }

    struct LocalFile(PathBuf);

    #[async_trait::async_trait]
    impl Downloader for LocalFile {
        async fn download(&self, _url: &str, dest: &Path, _max: u64) -> Result<String, HarnessError> {
            let bytes = std::fs::read(&self.0)?;
            std::fs::write(dest, &bytes)?;
            Ok(format!("{:x}", Sha256::digest(&bytes)))
        }
    }

    fn tar_gz_with(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, body, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, path, *body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn binary_agent(sha256: &str) -> (RegistryAgent, InstallPlan) {
        let agent = RegistryAgent {
            id: "fixture-agent".into(),
            name: "Fixture Agent".into(),
            version: "1.2.3".into(),
            description: String::new(),
            authors: vec![],
            license: None,
            website: None,
            repository: None,
            binary: BTreeMap::new(),
            npx: None,
            uvx: None,
        };
        let plan = InstallPlan::Binary {
            archive: "https://example.invalid/agent.tar.gz".into(),
            sha256: sha256.into(),
            format: ArchiveFormat::TarGz,
            cmd: vec!["bin".into(), "agent".into()],
            args: vec!["acp".into()],
            env: [("AGENT_NO_UPDATE".to_string(), "1".to_string())].into(),
        };
        (agent, plan)
    }

    #[tokio::test]
    async fn install_verifies_the_checksum_then_uninstall_removes_everything() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = scratch.path().join("agent.tar.gz");
        std::fs::write(
            &archive,
            tar_gz_with(&[("bin/agent", b"#!/bin/sh\necho acp\n", 0o755), ("README", b"hi", 0o644)]),
        )
        .unwrap();
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&archive).unwrap()));
        let root = scratch.path().join("acp-registry");
        let downloader = LocalFile(archive.clone());

        // A wrong pin never lands anything.
        let (agent, bad_plan) = binary_agent(&"0".repeat(64));
        let error = install(&root, &agent, &bad_plan, &downloader).await.unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert!(installed_agents(&root).is_empty());
        assert!(std::fs::read_dir(&root).unwrap().next().is_none(), "tmp dir cleaned");

        let (agent, plan) = binary_agent(&digest);
        let installed = install(&root, &agent, &plan, &downloader).await.unwrap();
        assert_eq!(installed.receipt.version, "1.2.3");
        assert_eq!(installed.receipt.integrity, format!("sha256:{digest}"));
        let (program, args, env) = launch_command(&installed).unwrap();
        assert_eq!(program, root.join("fixture-agent/1.2.3/bin/agent"));
        assert_eq!(args, vec!["acp".to_string()]);
        assert_eq!(env["AGENT_NO_UPDATE"], "1");
        assert_eq!(installed_agents(&root).len(), 1);

        // An update replaces the old version once the new one is complete.
        let (mut newer, plan) = binary_agent(&digest);
        newer.version = "1.3.0".into();
        install(&root, &newer, &plan, &downloader).await.unwrap();
        let listed = installed_agents(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].receipt.version, "1.3.0");
        assert!(!root.join("fixture-agent/1.2.3").exists());

        assert!(uninstall(&root, "fixture-agent").unwrap());
        assert!(installed_agents(&root).is_empty());
        assert!(!uninstall(&root, "fixture-agent").unwrap());
        assert!(uninstall(&root, "../escape").is_err());
    }

    #[tokio::test]
    async fn archives_with_links_or_escaping_paths_are_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "bin/agent", "/etc/passwd")
            .unwrap();
        let bytes = builder.into_inner().unwrap();
        let error = extract_tar(&bytes[..], scratch.path()).unwrap_err();
        assert!(error.to_string().contains("link"), "{error}");
        assert!(checked_output(scratch.path(), Path::new("../x")).is_err());
        assert!(checked_output(scratch.path(), Path::new("/abs")).is_err());
    }

    #[test]
    fn npm_integrity_and_bin_resolution() {
        let scratch = tempfile::tempdir().unwrap();
        let prefix = scratch.path();
        let pkg = prefix.join("node_modules/@github/copilot");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@github/copilot","version":"1.0.105","bin":{"copilot":"npm-loader.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("npm-loader.js"), "").unwrap();
        std::fs::write(
            prefix.join("node_modules/.package-lock.json"),
            r#"{"packages":{"node_modules/@github/copilot":{"integrity":"sha512-abc"}}}"#,
        )
        .unwrap();
        assert_eq!(
            installed_npm_integrity(prefix, "@github/copilot").as_deref(),
            Some("sha512-abc")
        );
        assert_eq!(
            npm_bin_entry(prefix, "@github/copilot", "github-copilot-cli").unwrap(),
            ["node_modules", "@github", "copilot", "npm-loader.js"]
        );
        let meta = parse_npm_version_meta(
            br#"{"name":"droid","version":"0.251.0","dist":{"integrity":"sha512-xyz","unpackedSize":25819}}"#,
            "droid",
            "0.251.0",
        )
        .unwrap();
        assert_eq!(meta.integrity, "sha512-xyz");
        assert_eq!(meta.unpacked_size, Some(25819));
        assert!(parse_npm_version_meta(br#"{"name":"evil","version":"0.251.0","dist":{"integrity":"sha512-x"}}"#, "droid", "0.251.0").is_err());
        assert!(parse_npm_version_meta(br#"{"name":"droid","version":"0.251.0","dist":{"integrity":"sha1-x"}}"#, "droid", "0.251.0").is_err());
    }
}
