//! Settings → Providers → "More agents": the public ACP agent registry,
//! cached under `{data_dir}/acp-registry/`, with one-click install, update
//! and uninstall into the managed adapters dir
//! (`<adapters>/acp-registry/<agent>/<version>`). Installed agents register
//! as [`HarnessId::Acp`] harnesses driven by the generic ACP driver.
//!
//! The index is untrusted data: [`zeron_harness::acp_registry`] validates it
//! and refuses anything that is not pinned to a checksum. Listing refreshes
//! the cached index when it is older than five minutes; installing always
//! refreshes first, so an install never acts on a stale pin.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use zeron_harness::acp_registry::{
    self as registry, InstallPlan, InstalledAgent, RegistryAgent, RegistryIndex,
};
use zeron_proto::{AcpRegistryAgentInfo, AcpRegistrySnapshot};

const MAX_AGE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CacheMeta {
    fetched_at: Option<i64>,
    /// `id@version` → bytes.
    sizes: HashMap<String, u64>,
}

struct Cached {
    index: Option<RegistryIndex>,
    meta: CacheMeta,
}

pub struct AcpAgents {
    cache_dir: PathBuf,
    install_root: PathBuf,
    registry_url: String,
    cached: Mutex<Cached>,
    last_error: Mutex<Option<String>>,
    installing: Mutex<HashSet<String>>,
    refresh: tokio::sync::Mutex<()>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl AcpAgents {
    /// `cache_dir` holds the cached index; `install_root` the installs.
    pub fn new(cache_dir: PathBuf, install_root: PathBuf) -> Self {
        let index = std::fs::read(cache_dir.join("registry.json"))
            .ok()
            .and_then(|bytes| registry::parse_index(&bytes).ok());
        let meta = std::fs::read(cache_dir.join("meta.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            cache_dir,
            install_root,
            registry_url: registry::REGISTRY_URL.to_string(),
            cached: Mutex::new(Cached { index, meta }),
            last_error: Mutex::new(None),
            installing: Mutex::new(HashSet::new()),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    pub fn install_root(&self) -> &Path {
        &self.install_root
    }

    pub fn installed(&self) -> Vec<InstalledAgent> {
        registry::installed_agents(&self.install_root)
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, Cached> {
        self.cached.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fresh(&self) -> bool {
        let cached = self.cached();
        cached.index.is_some()
            && cached
                .meta
                .fetched_at
                .is_some_and(|at| now_ms() - at < MAX_AGE.as_millis() as i64)
    }

    /// Fetch, validate and cache the index (and missing install sizes).
    async fn refresh(&self) -> Result<(), String> {
        let _guard = self.refresh.lock().await;
        let bytes = registry::fetch_index_bytes(&self.registry_url)
            .await
            .map_err(|e| e.to_string())?;
        let index = registry::parse_index(&bytes).map_err(|e| e.to_string())?;
        let _ = std::fs::create_dir_all(&self.cache_dir);
        let _ = write_atomic(&self.cache_dir.join("registry.json"), &bytes);
        let known: HashMap<String, u64> = self.cached().meta.sizes.clone();
        let target = registry::platform_target();
        let wanted: Vec<(String, InstallPlan)> = index
            .agents
            .iter()
            .filter(|agent| registry::built_in_equivalent(&agent.id).is_none())
            .filter_map(|agent| {
                let key = format!("{}@{}", agent.id, agent.version);
                (!known.contains_key(&key))
                    .then(|| registry::plan_install(agent, target).ok().map(|p| (key, p)))
                    .flatten()
            })
            .collect();
        let sizes: Vec<(String, u64)> = futures::stream::iter(wanted)
            .map(|(key, plan)| async move {
                let size = match &plan {
                    InstallPlan::Binary { archive, .. } => registry::archive_size(archive).await,
                    InstallPlan::Npm { name, version, .. } => registry::npm_version_meta(name, version)
                        .await
                        .ok()
                        .and_then(|meta| meta.unpacked_size),
                };
                size.map(|size| (key, size))
            })
            .buffer_unordered(8)
            .filter_map(|entry| async move { entry })
            .collect()
            .await;
        let meta_json = {
            let mut cached = self.cached();
            let live: HashSet<String> = index
                .agents
                .iter()
                .map(|a| format!("{}@{}", a.id, a.version))
                .collect();
            cached.meta.sizes.retain(|key, _| live.contains(key));
            cached.meta.sizes.extend(sizes);
            cached.meta.fetched_at = Some(now_ms());
            cached.index = Some(index);
            serde_json::to_vec(&cached.meta).unwrap_or_default()
        };
        let _ = write_atomic(&self.cache_dir.join("meta.json"), &meta_json);
        Ok(())
    }

    /// The "More agents" list, refreshing a stale cache first.
    pub async fn snapshot(&self, force: bool) -> AcpRegistrySnapshot {
        if force || !self.fresh() {
            let outcome = self.refresh().await;
            *self.last_error.lock().unwrap_or_else(PoisonError::into_inner) = outcome.err();
        }
        self.build_snapshot()
    }

    fn build_snapshot(&self) -> AcpRegistrySnapshot {
        let installed: HashMap<String, String> = self
            .installed()
            .into_iter()
            .map(|i| (i.receipt.agent_id, i.receipt.version))
            .collect();
        let npm = zeron_harness::acp_registry::npm_available();
        let cached = self.cached();
        let agents = cached
            .index
            .as_ref()
            .map(|index| {
                agent_infos(
                    index,
                    registry::platform_target(),
                    &installed,
                    &cached.meta.sizes,
                    npm,
                )
            })
            .unwrap_or_default();
        AcpRegistrySnapshot {
            agents,
            fetched_at: cached.meta.fetched_at,
            error: self
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// Install (or update to) the registry's current version of `agent_id`.
    pub async fn install(&self, agent_id: &str) -> Result<InstalledAgent, String> {
        if !self
            .installing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(agent_id.to_string())
        {
            return Err("already installing".into());
        }
        let result = self.install_inner(agent_id).await;
        self.installing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(agent_id);
        result
    }

    async fn install_inner(&self, agent_id: &str) -> Result<InstalledAgent, String> {
        self.refresh().await?;
        let agent: RegistryAgent = self
            .cached()
            .index
            .as_ref()
            .and_then(|index| index.agents.iter().find(|a| a.id == agent_id).cloned())
            .ok_or_else(|| format!("{agent_id} is not in the ACP registry"))?;
        if let Some(harness) = registry::built_in_equivalent(&agent.id) {
            return Err(format!(
                "{} is already built in as {harness}; enable it in Settings → Providers",
                agent.name
            ));
        }
        let plan = registry::plan_install(&agent, registry::platform_target())?;
        registry::install(
            &self.install_root,
            &agent,
            &plan,
            &registry::HttpsDownloader,
        )
        .await
        .map_err(|e| e.to_string())
    }

    pub fn uninstall(&self, agent_id: &str) -> Result<bool, String> {
        registry::uninstall(&self.install_root, agent_id).map_err(|e| e.to_string())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

/// One row per registry agent, as the listing device sees it.
pub fn agent_infos(
    index: &RegistryIndex,
    target: Option<&str>,
    installed: &HashMap<String, String>,
    sizes: &HashMap<String, u64>,
    npm_available: bool,
) -> Vec<AcpRegistryAgentInfo> {
    let mut rows: Vec<AcpRegistryAgentInfo> = index
        .agents
        .iter()
        .map(|agent| {
            let built_in = registry::built_in_equivalent(&agent.id);
            let plan = registry::plan_install(agent, target);
            let unavailable = match (&plan, built_in) {
                (_, Some(harness)) => Some(format!("Built in as {harness}")),
                (Err(reason), None) => Some(reason.clone()),
                (Ok(InstallPlan::Npm { .. }), None) if !npm_available => {
                    Some("Needs npm (Node.js) on this device".into())
                }
                (Ok(_), None) => None,
            };
            AcpRegistryAgentInfo {
                id: agent.id.clone(),
                name: agent.name.clone(),
                version: agent.version.clone(),
                description: agent.description.clone(),
                publisher: (!agent.authors.is_empty()).then(|| agent.authors.join(", ")),
                website: agent.website.clone().or_else(|| agent.repository.clone()),
                license: agent.license.clone(),
                distribution: plan.as_ref().ok().map(InstallPlan::kind),
                install_size: sizes.get(&format!("{}@{}", agent.id, agent.version)).copied(),
                unavailable,
                installed_version: installed.get(&agent.id).cloned(),
                built_in,
            }
        })
        .collect();
    // Installed first, then installable, then the rest; alphabetical within.
    rows.sort_by(|a, b| {
        let rank = |row: &AcpRegistryAgentInfo| {
            (
                row.installed_version.is_none(),
                row.unavailable.is_some(),
                row.name.to_lowercase(),
            )
        };
        rank(a).cmp(&rank(b))
    });
    rows
}

/// The ACP driver for an installed registry agent.
pub fn harness_for(installed: &InstalledAgent) -> Option<zeron_harness::AcpHarness> {
    let id = installed.agent_id()?;
    Some(zeron_harness::AcpHarness::registry_agent(
        zeron_harness::RegistryAgentLaunch {
            id,
            name: installed.receipt.name.clone(),
            program: installed.entry_path(),
            args: installed.receipt.args.clone(),
            env: installed.receipt.env.clone(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../harness/tests/fixtures/acp_registry.json");

    #[test]
    fn rows_describe_publisher_size_and_why_an_agent_is_unavailable() {
        let index = registry::parse_index(FIXTURE.as_bytes()).unwrap();
        let installed = HashMap::from([("kimi".to_string(), "1.0.0".to_string())]);
        let sizes = HashMap::from([("gemini@0.66.0".to_string(), 98_815_455)]);
        let rows = agent_infos(&index, Some("darwin-aarch64"), &installed, &sizes, true);
        assert_eq!(rows[0].id, "kimi", "installed rows lead");
        assert!(rows[0].update_available());
        let gemini = rows.iter().find(|r| r.id == "gemini").unwrap();
        assert_eq!(gemini.publisher.as_deref(), Some("Google"));
        assert_eq!(gemini.install_size, Some(98_815_455));
        assert_eq!(gemini.distribution, Some(zeron_proto::AcpDistributionKind::Npm));
        assert!(gemini.unavailable.is_none());
        let unpinned = rows.iter().find(|r| r.id == "unpinned-binary").unwrap();
        assert!(unpinned.unavailable.as_deref().unwrap().contains("checksum"));
        let without_npm = agent_infos(&index, Some("darwin-aarch64"), &installed, &sizes, false);
        let droid = without_npm.iter().find(|r| r.id == "factory-droid").unwrap();
        assert_eq!(droid.unavailable.as_deref(), Some("Needs npm (Node.js) on this device"));
    }

    #[test]
    fn built_in_duplicates_are_listed_but_not_installable() {
        let index = registry::parse_index(
            br#"{"version":"1.0.0","agents":[{"id":"codex-acp","name":"Codex","version":"2.3.0",
                "description":"","distribution":{"npx":{"package":"@agentclientprotocol/codex-acp@2.3.0"}}}]}"#,
        )
        .unwrap();
        let rows = agent_infos(&index, Some("darwin-aarch64"), &HashMap::new(), &HashMap::new(), true);
        assert_eq!(rows[0].built_in, Some(zeron_proto::HarnessId::Codex));
        assert!(rows[0].unavailable.is_some());
    }

    #[test]
    fn a_cached_index_survives_restarts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("registry.json"), FIXTURE).unwrap();
        let agents = AcpAgents::new(dir.path().to_path_buf(), dir.path().join("installs"));
        let snapshot = agents.build_snapshot();
        assert!(snapshot.agents.iter().any(|a| a.id == "factory-droid"));
        assert!(agents.installed().is_empty());
    }
}
