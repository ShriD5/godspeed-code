//! Provider instances: several named setups of one agent on one device —
//! different accounts, config/home dirs, env vars, binaries and custom
//! models (T3 Code's `ProviderInstanceRegistry`).
//!
//! The provider's DEFAULT instance is implicit: a chat or run with no
//! instance id (every chat from before instances existed) runs exactly as
//! before. Named instances are additive, stored per device by the engine
//! (`{data_dir}/provider-instances.json`). Env values that look secret are
//! moved into the OS keychain on save; the stored and listed shapes never
//! carry them — [`ProviderInstanceEnvVar::value`] is `None` for a secret.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// Longest accepted instance id / display name.
pub const INSTANCE_ID_MAX_LEN: usize = 64;
pub const INSTANCE_NAME_MAX_LEN: usize = 80;
/// Extra env vars one instance may carry.
pub const INSTANCE_MAX_ENV: usize = 64;
/// Custom model ids one instance may carry.
pub const INSTANCE_MAX_CUSTOM_MODELS: usize = 64;

/// One extra environment variable of an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceEnvVar {
    pub key: String,
    /// The plain value. `None` on a secret: its value lives in the OS
    /// keychain and never appears in settings, wire payloads or logs. On a
    /// save, `None` with `secret: true` keeps the stored secret unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub secret: bool,
}

/// One named instance of a provider, as stored and as Settings edits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstance {
    /// Stable id (`work`, `personal-2`); unique per device. Chats record it.
    pub id: String,
    pub harness: HarnessId,
    /// What pickers and receipts show (`Claude Code · Work` → `Work`).
    pub name: String,
    /// Absolute path of the agent binary to launch instead of the
    /// detected one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// The agent's config/home dir for this instance, exported as the
    /// provider's own variable (see [`config_dir_env`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<ProviderInstanceEnvVar>,
    /// The model a new chat on this instance starts on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Model ids the instance offers beyond the provider's catalog (a proxy's
    /// or gateway's own models).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
    /// The account this instance signs in as (a label; the login itself
    /// lives in the instance's config dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// What a harness descriptor tells pickers about one instance: never env
/// values, paths or secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceSummary {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl From<&ProviderInstance> for ProviderInstanceSummary {
    fn from(instance: &ProviderInstance) -> Self {
        Self {
            id: instance.id.clone(),
            name: instance.name.clone(),
            default_model: instance.default_model.clone(),
            custom_models: instance.custom_models.clone(),
            account: instance.account.clone(),
        }
    }
}

impl ProviderInstance {
    /// Structural validation of an instance a client asked to save.
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_instance_id(&self.id) {
            return Err(format!(
                "instance id must be 1-{INSTANCE_ID_MAX_LEN} lowercase letters, digits, '-' or '_'"
            ));
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > INSTANCE_NAME_MAX_LEN {
            return Err(format!(
                "instance name must be 1-{INSTANCE_NAME_MAX_LEN} characters"
            ));
        }
        if self.harness == HarnessId::Mock {
            return Err("the mock harness has no instances".into());
        }
        for (field, path) in [
            ("binary path", &self.binary_path),
            ("config dir", &self.config_dir),
        ] {
            if let Some(path) = path
                && !is_absolute_path(path)
            {
                return Err(format!("{field} must be an absolute path"));
            }
        }
        if self.config_dir.is_some() && config_dir_env(self.harness).is_none() {
            return Err(format!(
                "{} has no known config-dir variable; set its env vars instead",
                self.harness
            ));
        }
        if self.env.len() > INSTANCE_MAX_ENV {
            return Err(format!("at most {INSTANCE_MAX_ENV} env vars"));
        }
        let mut seen = std::collections::HashSet::new();
        for var in &self.env {
            if !is_valid_env_key(&var.key) {
                return Err(format!("invalid env var name {:?}", var.key));
            }
            if is_reserved_env_key(&var.key) {
                return Err(format!("{} cannot be overridden by an instance", var.key));
            }
            if !seen.insert(var.key.as_str()) {
                return Err(format!("env var {} is listed twice", var.key));
            }
            if var.value.as_deref().is_some_and(|v| v.contains('\0')) {
                return Err(format!("env var {} contains a NUL byte", var.key));
            }
        }
        if self.custom_models.len() > INSTANCE_MAX_CUSTOM_MODELS {
            return Err(format!("at most {INSTANCE_MAX_CUSTOM_MODELS} custom models"));
        }
        if self
            .custom_models
            .iter()
            .chain(self.default_model.iter())
            .any(|model| model.trim().is_empty() || model.len() > 256)
        {
            return Err("model ids must be 1-256 characters".into());
        }
        Ok(())
    }
}

pub fn is_valid_instance_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= INSTANCE_ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

/// A slug for a new instance's id, from its display name.
pub fn instance_id_from_name(name: &str) -> String {
    let mut id = String::new();
    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            id.push(ch);
        } else if !id.ends_with('-') && !id.is_empty() {
            id.push('-');
        }
        if id.len() >= INSTANCE_ID_MAX_LEN {
            break;
        }
    }
    let id = id.trim_end_matches('-').to_string();
    if id.is_empty() { "instance".into() } else { id }
}

fn is_absolute_path(path: &str) -> bool {
    std::path::Path::new(path).is_absolute()
        || (path.len() >= 3 && path.as_bytes()[1] == b':' && matches!(path.as_bytes()[2], b'\\' | b'/'))
}

pub fn is_valid_env_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Variables an instance must not replace: the engine's own process
/// plumbing and the loader hooks that would let a settings edit inject code
/// into every spawned agent.
pub fn is_reserved_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    matches!(upper.as_str(), "PATH" | "HOME" | "USERPROFILE" | "NODE_OPTIONS")
        || upper.starts_with("GODSPEED_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("LD_")
}

/// The variable a provider reads its config/home dir from, when it has one.
pub fn config_dir_env(harness: HarnessId) -> Option<&'static str> {
    Some(match harness {
        HarnessId::ClaudeCode => "CLAUDE_CONFIG_DIR",
        HarnessId::Codex => "CODEX_HOME",
        HarnessId::Grok => "GROK_HOME",
        HarnessId::Hermes => "HERMES_HOME",
        HarnessId::Pi => "PI_CODING_AGENT_DIR",
        HarnessId::Antigravity => "GEMINI_HOME",
        HarnessId::Acp(agent) => match agent.as_str() {
            "gemini" => "GEMINI_CLI_HOME",
            "github-copilot-cli" => "COPILOT_HOME",
            _ => return None,
        },
        HarnessId::Cursor
        | HarnessId::Devin
        | HarnessId::Fx
        | HarnessId::Muse
        | HarnessId::Opencode
        | HarnessId::Mock => return None,
    })
}

/// Whether an env value should live in the keychain rather than settings:
/// a credential-shaped name (`*_TOKEN`, `*_API_KEY`, `*SECRET*`,
/// `*PASSWORD*`, …) or a credential-shaped value (well-known token
/// prefixes, JWTs, long unbroken high-entropy strings).
pub fn looks_secret(key: &str, value: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    const NAME_MARKERS: [&str; 10] = [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "PRIVATE",
        "CREDENTIAL",
        "COOKIE",
    ];
    if NAME_MARKERS.iter().any(|marker| upper.contains(marker))
        || upper.ends_with("_KEY")
        || upper.ends_with("_AUTH")
        || upper.ends_with("_PAT")
    {
        return true;
    }
    let value = value.trim();
    const VALUE_PREFIXES: [&str; 12] = [
        "sk-", "sk_", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "AKIA",
        "AIza", "glpat-",
    ];
    if VALUE_PREFIXES.iter().any(|prefix| value.starts_with(prefix)) {
        return true;
    }
    if value.starts_with("eyJ") && value.matches('.').count() == 2 {
        return true;
    }
    // A long unbroken token mixing letters and digits, not a path or URL.
    value.len() >= 32
        && !value.contains(['/', '\\', ' ', ':'])
        && value.bytes().any(|b| b.is_ascii_digit())
        && value.bytes().any(|b| b.is_ascii_alphabetic())
}

/// The environment an instance contributes to its agent processes, in
/// precedence order: the instance's extra env vars, then its config dir
/// exported as the provider's own variable — the dedicated field wins over
/// an extra var of the same name. `secrets` resolves keychain-held values;
/// a secret it cannot resolve is left out rather than sent empty.
pub fn instance_env(
    instance: &ProviderInstance,
    secrets: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for var in &instance.env {
        let value = if var.secret {
            secrets(&var.key)
        } else {
            var.value.clone()
        };
        if let Some(value) = value {
            env.insert(var.key.clone(), value);
        }
    }
    if let (Some(dir), Some(name)) = (&instance.config_dir, config_dir_env(instance.harness)) {
        env.insert(name.to_string(), dir.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(rel: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{}", rel.replace('/', "\\"))
        } else {
            format!("/{rel}")
        }
    }

    fn instance() -> ProviderInstance {
        ProviderInstance {
            id: "work".into(),
            harness: HarnessId::ClaudeCode,
            name: "Work".into(),
            binary_path: None,
            config_dir: Some(abs("tmp/claude-work")),
            env: vec![
                ProviderInstanceEnvVar {
                    key: "CLAUDE_CONFIG_DIR".into(),
                    value: Some("/tmp/loses".into()),
                    secret: false,
                },
                ProviderInstanceEnvVar {
                    key: "ANTHROPIC_API_KEY".into(),
                    value: None,
                    secret: true,
                },
                ProviderInstanceEnvVar {
                    key: "HTTPS_PROXY".into(),
                    value: Some("http://proxy:8080".into()),
                    secret: false,
                },
            ],
            default_model: Some("claude-opus".into()),
            custom_models: vec!["proxy/opus".into()],
            account: Some("work@example.com".into()),
        }
    }

    #[test]
    fn config_dir_wins_over_a_same_named_env_var_and_secrets_resolve_late() {
        let env = instance_env(&instance(), |key| {
            (key == "ANTHROPIC_API_KEY").then(|| "sk-resolved".to_string())
        });
        assert_eq!(env["CLAUDE_CONFIG_DIR"], abs("tmp/claude-work"));
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-resolved");
        assert_eq!(env["HTTPS_PROXY"], "http://proxy:8080");
        let unresolved = instance_env(&instance(), |_| None);
        assert!(!unresolved.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn codex_config_dir_exports_codex_home() {
        let mut codex = instance();
        codex.harness = HarnessId::Codex;
        codex.env.clear();
        codex.config_dir = Some(abs("tmp/codex-b"));
        assert_eq!(instance_env(&codex, |_| None)["CODEX_HOME"], abs("tmp/codex-b"));
    }

    #[test]
    fn secret_heuristics() {
        assert!(looks_secret("OPENAI_API_KEY", "x"));
        assert!(looks_secret("GITHUB_TOKEN", "x"));
        assert!(looks_secret("ANYTHING", "sk-ant-abc"));
        assert!(looks_secret("ANYTHING", "ghp_abcdef"));
        assert!(looks_secret("X", "eyJhbGciOi.eyJzdWIiOiIx.c2ln"));
        assert!(looks_secret("X", "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7"));
        assert!(!looks_secret("HTTPS_PROXY", "http://proxy:8080"));
        assert!(!looks_secret("CLAUDE_CONFIG_DIR", "/Users/me/.claude-work"));
        assert!(!looks_secret("MODEL", "claude-opus-4"));
    }

    #[test]
    fn validation_rejects_reserved_and_malformed_input() {
        let ok = instance();
        assert!(ok.validate().is_ok());
        let mut reserved = ok.clone();
        reserved.env.push(ProviderInstanceEnvVar {
            key: "DYLD_INSERT_LIBRARIES".into(),
            value: Some("/x".into()),
            secret: false,
        });
        assert!(reserved.validate().is_err());
        let mut relative = ok.clone();
        relative.binary_path = Some("bin/claude".into());
        assert!(relative.validate().is_err());
        let mut bad_id = ok.clone();
        bad_id.id = "Work Account".into();
        assert!(bad_id.validate().is_err());
        let mut cursor_dir = ok.clone();
        cursor_dir.harness = HarnessId::Cursor;
        assert!(cursor_dir.validate().is_err());
        assert_eq!(instance_id_from_name("  Work (Corp) #2 "), "work-corp-2");
    }

    #[test]
    fn stored_shape_has_no_secret_values() {
        let json = serde_json::to_string(&instance()).unwrap();
        assert!(!json.contains("sk-"));
        let summary = serde_json::to_string(&ProviderInstanceSummary::from(&instance())).unwrap();
        assert!(!summary.contains("ANTHROPIC_API_KEY"));
        assert!(!summary.contains("claude-work"));
    }
}
