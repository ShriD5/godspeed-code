//! Provider instances on this device (Settings → Providers → Instances):
//! `{data_dir}/provider-instances.json` plus the OS keychain.
//!
//! The file holds only non-secret configuration. On save, every env value
//! that is marked secret or [`looks_secret`] moves into the keychain (service
//! [`KEYCHAIN_SERVICE`], account `<harness>/<instance>/<VAR>`) and the stored
//! var keeps `secret: true` with no value; nothing the engine serializes —
//! the file, RPC replies, descriptors, logs — ever carries it. Secrets are
//! read back only when a run resolves the instance.
//!
//! Migration is additive: a device without the file has no named instances,
//! and every chat without an instance id keeps running on the provider's
//! default instance exactly as before.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use zeron_proto::provider_instance::{instance_env, looks_secret};
use zeron_proto::{HarnessId, ProviderInstance, ProviderInstanceSummary};

pub const KEYCHAIN_SERVICE: &str = "Godspeed Provider Instances";

/// Where instance secrets live. Production uses the OS keychain; tests an
/// in-memory map.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, value: &str) -> Result<(), String>;
    fn delete(&self, account: &str);
}

/// The OS keychain: macOS Keychain (`/usr/bin/security`), the Secret Service
/// on Linux (`secret-tool`). Other platforms refuse secret values rather
/// than fall back to plain storage.
pub struct KeychainSecrets;

impl KeychainSecrets {
    fn run(program: &str, args: &[&str], stdin: Option<&str>) -> Option<std::process::Output> {
        use std::io::Write;
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(input.as_bytes());
        }
        child.wait_with_output().ok()
    }

    fn secret_tool() -> Option<PathBuf> {
        ["/usr/bin/secret-tool", "/usr/local/bin/secret-tool"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }
}

impl SecretStore for KeychainSecrets {
    fn get(&self, account: &str) -> Option<String> {
        let out = if cfg!(target_os = "macos") {
            // Absolute path: a PATH-planted `security` must never see secrets.
            Self::run(
                "/usr/bin/security",
                &["find-generic-password", "-a", account, "-s", KEYCHAIN_SERVICE, "-w"],
                None,
            )?
        } else if cfg!(target_os = "linux") {
            let tool = Self::secret_tool()?;
            Self::run(
                tool.to_str()?,
                &["lookup", "service", KEYCHAIN_SERVICE, "account", account],
                None,
            )?
        } else {
            return None;
        };
        if !out.status.success() {
            return None;
        }
        let value = String::from_utf8(out.stdout).ok()?;
        Some(value.strip_suffix('\n').unwrap_or(&value).to_string())
    }

    fn set(&self, account: &str, value: &str) -> Result<(), String> {
        let out = if cfg!(target_os = "macos") {
            Self::run(
                "/usr/bin/security",
                &[
                    "add-generic-password",
                    "-U",
                    "-a",
                    account,
                    "-s",
                    KEYCHAIN_SERVICE,
                    "-w",
                    value,
                ],
                None,
            )
        } else if cfg!(target_os = "linux") {
            let tool = Self::secret_tool()
                .ok_or("no OS keychain (secret-tool) is available for secret values")?;
            Self::run(
                tool.to_str().ok_or("secret-tool path is not UTF-8")?,
                &[
                    "store",
                    "--label",
                    KEYCHAIN_SERVICE,
                    "service",
                    KEYCHAIN_SERVICE,
                    "account",
                    account,
                ],
                Some(value),
            )
        } else {
            return Err("secret env values need an OS keychain, unavailable on this platform".into());
        };
        match out {
            Some(out) if out.status.success() => Ok(()),
            // Never echo the command or its output: either could hold the value.
            _ => Err("the OS keychain refused to store the secret".into()),
        }
    }

    fn delete(&self, account: &str) {
        if cfg!(target_os = "macos") {
            let _ = Self::run(
                "/usr/bin/security",
                &["delete-generic-password", "-a", account, "-s", KEYCHAIN_SERVICE],
                None,
            );
        } else if cfg!(target_os = "linux")
            && let Some(tool) = Self::secret_tool()
            && let Some(tool) = tool.to_str()
        {
            let _ = Self::run(
                tool,
                &["clear", "service", KEYCHAIN_SERVICE, "account", account],
                None,
            );
        }
    }
}

/// In-memory secrets, for tests and isolated engines.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, account: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(account)
            .cloned()
    }
    fn set(&self, account: &str, value: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(account.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, account: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(account);
    }
}

fn secret_account(harness: HarnessId, instance: &str, key: &str) -> String {
    format!("{harness}/{instance}/{key}")
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct InstancesFile {
    version: u32,
    instances: Vec<ProviderInstance>,
}

/// One instance resolved for a launch: its binary and full environment
/// (secrets included — never serialize or log this).
#[derive(Clone)]
pub struct ResolvedInstance {
    pub instance: ProviderInstance,
    pub executable: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

impl ResolvedInstance {
    /// Identity of everything a cached driver was built from (never shown).
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(&self.instance).unwrap_or_default());
        for (key, value) in &self.env {
            digest.update(key.as_bytes());
            digest.update([0]);
            digest.update(value.as_bytes());
            digest.update([0]);
        }
        format!("{:x}", digest.finalize())
    }
}

pub struct ProviderInstances {
    path: Option<PathBuf>,
    instances: Mutex<Vec<ProviderInstance>>,
    secrets: Arc<dyn SecretStore>,
}

impl Default for ProviderInstances {
    fn default() -> Self {
        Self::in_memory(Arc::new(MemorySecrets::default()))
    }
}

impl ProviderInstances {
    /// No file, no instances (bare registries, tests).
    pub fn in_memory(secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            path: None,
            instances: Mutex::new(Vec::new()),
            secrets,
        }
    }

    /// Load `{data_dir}/provider-instances.json`. A missing or unreadable
    /// file means no named instances; invalid entries are skipped.
    pub fn load(data_dir: &Path, secrets: Arc<dyn SecretStore>) -> Self {
        let path = data_dir.join("provider-instances.json");
        let file: InstancesFile = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let instances = file
            .instances
            .into_iter()
            .filter(|instance| instance.validate().is_ok())
            .filter(|instance| seen.insert((instance.harness, instance.id.clone())))
            .collect();
        Self {
            path: Some(path),
            instances: Mutex::new(instances),
            secrets,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProviderInstance>> {
        self.instances.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every instance, secrets withheld.
    pub fn list(&self) -> Vec<ProviderInstance> {
        self.lock().clone()
    }

    pub fn summaries(&self, harness: HarnessId) -> Vec<ProviderInstanceSummary> {
        self.lock()
            .iter()
            .filter(|instance| instance.harness == harness)
            .map(ProviderInstanceSummary::from)
            .collect()
    }

    pub fn get(&self, harness: HarnessId, id: &str) -> Option<ProviderInstance> {
        self.lock()
            .iter()
            .find(|instance| instance.harness == harness && instance.id == id)
            .cloned()
    }

    /// Create or replace an instance. Secret values move to the keychain;
    /// a secret var sent without a value keeps its stored secret.
    pub fn save(&self, mut instance: ProviderInstance) -> Result<ProviderInstance, String> {
        instance.name = instance.name.trim().to_string();
        instance.custom_models.retain(|model| !model.trim().is_empty());
        instance.custom_models.dedup();
        instance.default_model = instance.default_model.filter(|m| !m.trim().is_empty());
        instance.binary_path = instance.binary_path.filter(|p| !p.trim().is_empty());
        instance.config_dir = instance.config_dir.filter(|p| !p.trim().is_empty());
        instance.account = instance.account.filter(|a| !a.trim().is_empty());
        instance.validate()?;
        let previous = self.get(instance.harness, &instance.id);
        let account = |key: &str| secret_account(instance.harness, &instance.id, key);
        for var in &mut instance.env {
            match var.value.take() {
                Some(value) if var.secret || looks_secret(&var.key, &value) => {
                    self.secrets.set(&account(&var.key), &value)?;
                    var.secret = true;
                }
                Some(value) => {
                    if previous
                        .as_ref()
                        .is_some_and(|p| p.env.iter().any(|v| v.key == var.key && v.secret))
                    {
                        self.secrets.delete(&account(&var.key));
                    }
                    var.value = Some(value);
                    var.secret = false;
                }
                None if var.secret => {
                    if self.secrets.get(&account(&var.key)).is_none() {
                        return Err(format!("enter a value for {}", var.key));
                    }
                }
                None => return Err(format!("enter a value for {}", var.key)),
            }
        }
        if let Some(previous) = &previous {
            for old in previous.env.iter().filter(|v| v.secret) {
                if !instance.env.iter().any(|v| v.key == old.key) {
                    self.secrets.delete(&account(&old.key));
                }
            }
        }
        {
            let mut instances = self.lock();
            match instances
                .iter_mut()
                .find(|i| i.harness == instance.harness && i.id == instance.id)
            {
                Some(slot) => *slot = instance.clone(),
                None => instances.push(instance.clone()),
            }
        }
        self.persist()?;
        Ok(instance)
    }

    /// Remove an instance and its keychain entries.
    pub fn delete(&self, harness: HarnessId, id: &str) -> Result<bool, String> {
        let removed = {
            let mut instances = self.lock();
            let index = instances
                .iter()
                .position(|i| i.harness == harness && i.id == id);
            index.map(|index| instances.remove(index))
        };
        let Some(removed) = removed else {
            return Ok(false);
        };
        for var in removed.env.iter().filter(|v| v.secret) {
            self.secrets.delete(&secret_account(harness, id, &var.key));
        }
        self.persist()?;
        Ok(true)
    }

    /// The instance as a launch needs it, keychain secrets included.
    pub fn resolve(&self, harness: HarnessId, id: &str) -> Option<ResolvedInstance> {
        let instance = self.get(harness, id)?;
        let env = instance_env(&instance, |key| {
            self.secrets.get(&secret_account(harness, id, key))
        });
        Some(ResolvedInstance {
            executable: instance.binary_path.as_ref().map(PathBuf::from),
            env,
            instance,
        })
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = InstancesFile {
            version: 1,
            instances: self.list(),
        };
        // By construction no secret var carries a value; keep it that way
        // even if a future edit forgets to strip one.
        debug_assert!(
            file.instances
                .iter()
                .flat_map(|i| &i.env)
                .all(|v| !v.secret || v.value.is_none())
        );
        let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::ProviderInstanceEnvVar;

    fn var(key: &str, value: Option<&str>, secret: bool) -> ProviderInstanceEnvVar {
        ProviderInstanceEnvVar {
            key: key.into(),
            value: value.map(str::to_owned),
            secret,
        }
    }

    fn work() -> ProviderInstance {
        ProviderInstance {
            id: "work".into(),
            harness: HarnessId::ClaudeCode,
            name: " Work ".into(),
            binary_path: Some("/opt/claude-work/bin/claude".into()),
            config_dir: Some("/tmp/claude-work".into()),
            env: vec![
                var("ANTHROPIC_BASE_URL", Some("https://proxy.example/v1"), false),
                var("ANTHROPIC_AUTH_TOKEN", Some("sk-ant-supersecret-123"), false),
                var("CLAUDE_CONFIG_DIR", Some("/tmp/ignored"), false),
            ],
            default_model: Some("claude-opus".into()),
            custom_models: vec!["gateway/opus".into(), " ".into()],
            account: Some("me@work.example".into()),
        }
    }

    #[test]
    fn secrets_never_reach_the_file_or_the_listing_and_resolve_for_runs() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let store = ProviderInstances::load(dir.path(), secrets.clone());
        let saved = store.save(work()).unwrap();
        assert_eq!(saved.name, "Work");
        assert_eq!(saved.custom_models, vec!["gateway/opus".to_string()]);
        let token = saved.env.iter().find(|v| v.key == "ANTHROPIC_AUTH_TOKEN").unwrap();
        assert!(token.secret && token.value.is_none());

        let file = std::fs::read_to_string(dir.path().join("provider-instances.json")).unwrap();
        assert!(!file.contains("supersecret"), "{file}");
        assert!(file.contains("https://proxy.example/v1"));
        let listed = serde_json::to_string(&store.list()).unwrap();
        assert!(!listed.contains("supersecret"));
        let summaries = serde_json::to_string(&store.summaries(HarnessId::ClaudeCode)).unwrap();
        assert!(!summaries.contains("supersecret") && !summaries.contains("proxy.example"));
        assert_eq!(
            secrets.get("claude-code/work/ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("sk-ant-supersecret-123")
        );

        // Precedence: binary override, config dir over a same-named var,
        // secrets resolved from the keychain only at launch.
        let resolved = store.resolve(HarnessId::ClaudeCode, "work").unwrap();
        assert_eq!(
            resolved.executable,
            Some(PathBuf::from("/opt/claude-work/bin/claude"))
        );
        assert_eq!(resolved.env["CLAUDE_CONFIG_DIR"], "/tmp/claude-work");
        assert_eq!(resolved.env["ANTHROPIC_AUTH_TOKEN"], "sk-ant-supersecret-123");
        assert_eq!(resolved.env["ANTHROPIC_BASE_URL"], "https://proxy.example/v1");
        assert!(store.resolve(HarnessId::Codex, "work").is_none());

        // Reload from disk: the same instance, still without the value.
        let reloaded = ProviderInstances::load(dir.path(), secrets.clone());
        assert_eq!(reloaded.list(), store.list());

        // Re-saving the secret without a value keeps it; dropping it deletes it.
        let mut edit = reloaded.get(HarnessId::ClaudeCode, "work").unwrap();
        edit.name = "Work 2".into();
        reloaded.save(edit.clone()).unwrap();
        assert!(secrets.get("claude-code/work/ANTHROPIC_AUTH_TOKEN").is_some());
        edit.env.retain(|v| v.key != "ANTHROPIC_AUTH_TOKEN");
        reloaded.save(edit).unwrap();
        assert!(secrets.get("claude-code/work/ANTHROPIC_AUTH_TOKEN").is_none());

        assert!(reloaded.delete(HarnessId::ClaudeCode, "work").unwrap());
        assert!(reloaded.list().is_empty());
    }

    #[test]
    fn a_device_without_the_file_has_only_default_instances() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProviderInstances::load(dir.path(), Arc::new(MemorySecrets::default()));
        assert!(store.list().is_empty());
        assert!(store.resolve(HarnessId::ClaudeCode, "anything").is_none());
        // A corrupt file is the same as none, and invalid rows are skipped.
        std::fs::write(dir.path().join("provider-instances.json"), "{not json").unwrap();
        assert!(ProviderInstances::load(dir.path(), Arc::new(MemorySecrets::default()))
            .list()
            .is_empty());
        std::fs::write(
            dir.path().join("provider-instances.json"),
            r#"{"version":1,"instances":[
                {"id":"ok","harness":"codex","name":"OK","configDir":"/tmp/codex-ok"},
                {"id":"Bad Id","harness":"codex","name":"Bad"},
                {"id":"ok","harness":"codex","name":"Duplicate"}
            ]}"#,
        )
        .unwrap();
        let store = ProviderInstances::load(dir.path(), Arc::new(MemorySecrets::default()));
        assert_eq!(store.list().len(), 1);
        assert_eq!(
            store.resolve(HarnessId::Codex, "ok").unwrap().env["CODEX_HOME"],
            "/tmp/codex-ok"
        );
    }

    #[test]
    fn a_secret_without_a_stored_value_is_refused() {
        let store = ProviderInstances::default();
        let mut instance = work();
        instance.env = vec![var("OPENAI_API_KEY", None, true)];
        assert!(store.save(instance).is_err());
    }
}
