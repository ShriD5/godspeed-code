//! One named provider instance as a [`Harness`]: the provider's driver
//! launched from the instance's binary, with the instance's environment
//! (config dir, extra variables, keychain-held secrets) applied to every run
//! and its custom models offered beside the provider's catalog.
//!
//! Precedence, highest first:
//! - **binary**: the instance's `binary_path` → the provider's env override
//!   (`ANTIGRAVITY_ACP_EXECUTABLE`, …) → PATH / install-dir detection;
//! - **env**: the instance's config dir (as the provider's own variable) →
//!   the instance's extra env vars → the chat's worktree env → the engine's
//!   own process env.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use zeron_proto::{AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode};

use crate::{Harness, HarnessError, ModelCatalog, RunControls};

/// Build a built-in provider's driver, optionally from an explicit binary.
/// ACP drivers also take `env` for their discovery processes. `None` for the
/// mock and for registry agents (the engine builds those from their install).
pub fn build_driver(
    id: HarnessId,
    executable: Option<PathBuf>,
    env: BTreeMap<String, String>,
) -> Option<Arc<dyn Harness>> {
    macro_rules! with_exe {
        ($harness:expr) => {{
            let harness = $harness;
            match executable {
                Some(path) => Arc::new(harness.with_executable(path)) as Arc<dyn Harness>,
                None => Arc::new(harness) as Arc<dyn Harness>,
            }
        }};
    }
    let acp = |harness: crate::AcpHarness| {
        let harness = harness.with_env(env.clone());
        match executable.clone() {
            Some(path) => Arc::new(harness.with_executable(path)) as Arc<dyn Harness>,
            None => Arc::new(harness) as Arc<dyn Harness>,
        }
    };
    Some(match id {
        HarnessId::ClaudeCode => with_exe!(crate::ClaudeHarness::new()),
        HarnessId::Codex => with_exe!(crate::CodexHarness::new()),
        HarnessId::Cursor => with_exe!(crate::CursorHarness::new()),
        HarnessId::Pi => with_exe!(crate::PiHarness::new()),
        HarnessId::Muse => with_exe!(crate::MuseHarness::new()),
        HarnessId::Opencode => with_exe!(crate::OpencodeHarness::new()),
        HarnessId::Devin => acp(crate::AcpHarness::devin()),
        HarnessId::Grok => acp(crate::AcpHarness::grok()),
        HarnessId::Hermes => acp(crate::AcpHarness::hermes()),
        HarnessId::Antigravity => acp(crate::AcpHarness::antigravity()),
        HarnessId::Mock | HarnessId::Acp(_) => return None,
    })
}

/// A provider instance wrapped around its driver.
pub struct InstanceHarness {
    inner: Arc<dyn Harness>,
    env: BTreeMap<String, String>,
    default_model: Option<String>,
    custom_models: Vec<String>,
}

impl InstanceHarness {
    pub fn new(
        inner: Arc<dyn Harness>,
        env: BTreeMap<String, String>,
        default_model: Option<String>,
        custom_models: Vec<String>,
    ) -> Self {
        Self {
            inner,
            env,
            default_model,
            custom_models,
        }
    }

    fn apply(&self, mut request: RunRequest) -> RunRequest {
        for (key, value) in &self.env {
            request.env.insert(key.clone(), value.clone());
        }
        request
    }

    fn decorate(&self, models: Vec<Model>) -> Vec<Model> {
        instance_models(models, self.default_model.as_deref(), &self.custom_models)
    }
}

/// The instance's model list: the provider's catalog plus its custom ids
/// (deduplicated), with the instance's default model first.
pub fn instance_models(
    mut models: Vec<Model>,
    default_model: Option<&str>,
    custom_models: &[String],
) -> Vec<Model> {
    let ladder = models
        .first()
        .map(|m| m.reasoning_levels.clone())
        .unwrap_or_default();
    for id in custom_models
        .iter()
        .map(String::as_str)
        .chain(default_model)
    {
        if !models.iter().any(|m| m.id == id) {
            models.push(Model {
                id: id.to_string(),
                label: id.to_string(),
                description: Some("Custom model".into()),
                reasoning_levels: ladder.clone(),
                options: Vec::new(),
            });
        }
    }
    if let Some(default) = default_model
        && let Some(index) = models.iter().position(|m| m.id == default)
    {
        let row = models.remove(index);
        models.insert(0, row);
    }
    models
}

#[async_trait]
impl Harness for InstanceHarness {
    fn id(&self) -> HarnessId {
        self.inner.id()
    }
    fn display_name(&self) -> &str {
        self.inner.display_name()
    }
    fn supports_steering(&self) -> bool {
        self.inner.supports_steering()
    }
    fn steering_mode(&self) -> SteeringMode {
        self.inner.steering_mode()
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        self.inner.reasoning_levels()
    }
    fn installed(&self) -> bool {
        self.inner.installed()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.inner.executable_path()
    }
    fn deterministic_turn_end(&self) -> bool {
        self.inner.deterministic_turn_end()
    }
    fn authoritative_prompt_end(&self) -> bool {
        self.inner.authoritative_prompt_end()
    }
    fn stops_turn_in_place(&self) -> bool {
        self.inner.stops_turn_in_place()
    }
    fn reconfigures_in_place(&self, live: &RunRequest, next: &RunRequest) -> bool {
        self.inner.reconfigures_in_place(live, next)
    }
    fn same_runtime(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.instance == next.instance && self.inner.same_runtime(live, next)
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.inner.models().await.map(|models| self.decorate(models))
    }
    fn model_context(&self) -> Result<Option<crate::ModelContext>, HarnessError> {
        self.inner.model_context()
    }
    fn fallback_models(&self) -> Vec<Model> {
        self.decorate(self.inner.fallback_models())
    }
    async fn model_catalog(&self, force: bool) -> Result<ModelCatalog, HarnessError> {
        let catalog = self.inner.model_catalog(force).await?;
        Ok(ModelCatalog {
            models: self.decorate(catalog.models),
            source: catalog.source,
        })
    }
    async fn commands(&self) -> Result<Vec<zeron_proto::SlashCommand>, HarnessError> {
        self.inner.commands().await
    }
    async fn commands_for(
        &self,
        cwd: &std::path::Path,
    ) -> Result<Vec<zeron_proto::SlashCommand>, HarnessError> {
        self.inner.commands_for(cwd).await
    }
    async fn skills(
        &self,
        cwd: &std::path::Path,
    ) -> Result<Option<Vec<zeron_proto::invocation::Skill>>, HarnessError> {
        self.inner.skills(cwd).await
    }
    async fn run_title(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.inner.run_title(self.apply(request), controls).await
    }
    async fn start_idle(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.inner.start_idle(self.apply(request), controls).await
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.inner.run(self.apply(request), controls).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            label: id.into(),
            description: None,
            reasoning_levels: vec![ReasoningLevel::Low, ReasoningLevel::High],
            options: vec![],
        }
    }

    #[test]
    fn custom_models_join_the_catalog_and_the_default_leads() {
        let rows = instance_models(
            vec![model("opus"), model("sonnet")],
            Some("sonnet"),
            &["proxy/opus".into(), "opus".into()],
        );
        let ids: Vec<&str> = rows.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["sonnet", "opus", "proxy/opus"]);
        assert_eq!(rows[2].description.as_deref(), Some("Custom model"));
        assert_eq!(rows[2].reasoning_levels, model("x").reasoning_levels);
        let unknown_default = instance_models(vec![model("opus")], Some("gateway-x"), &[]);
        assert_eq!(unknown_default[0].id, "gateway-x");
    }

    #[test]
    fn instance_env_overrides_worktree_env_on_runs() {
        let harness = InstanceHarness::new(
            Arc::new(crate::mock::MockHarness { script: vec![] }),
            [("CODEX_HOME".to_string(), "/instance".to_string())].into(),
            None,
            vec![],
        );
        let mut request: RunRequest = serde_json::from_value(serde_json::json!({
            "prompt": "hi",
            "model": null,
            "reasoning": null,
            "cwd": "/tmp",
            "sandbox": "read-only",
            "resume": null,
            "env": {"CODEX_HOME": "/worktree", "PORT": "4000"},
        }))
        .unwrap();
        request = harness.apply(request);
        assert_eq!(request.env["CODEX_HOME"], "/instance");
        assert_eq!(request.env["PORT"], "4000");
    }

    #[test]
    fn instances_never_share_a_live_runtime() {
        let harness = InstanceHarness::new(
            Arc::new(crate::mock::MockHarness { script: vec![] }),
            BTreeMap::new(),
            None,
            vec![],
        );
        let base: RunRequest = serde_json::from_value(serde_json::json!({
            "prompt": "hi", "model": null, "reasoning": null, "cwd": "/tmp",
            "sandbox": "read-only", "resume": null,
        }))
        .unwrap();
        let mut other = base.clone();
        other.instance = Some("work".into());
        assert!(harness.same_runtime(&base, &base));
        assert!(!harness.same_runtime(&base, &other));
    }
}
