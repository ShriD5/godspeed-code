//! Always-on agent RPCs (`crate::agents`): definitions, run-now, webhook
//! tokens and the run inbox. Device-scoped, so all of them forward on
//! `targetDeviceId` to the engine that hosts the agents.

use super::*;
use zeron_proto::agents::{AgentDraft, AgentRunFilter};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_AGENTS
            | methods::WATCH_AGENTS
            | methods::CREATE_AGENT
            | methods::UPDATE_AGENT
            | methods::DELETE_AGENT
            | methods::SET_AGENT_ENABLED
            | methods::RUN_AGENT_NOW
            | methods::ROTATE_AGENT_WEBHOOK
            | methods::LIST_AGENT_RUNS
            | methods::MARK_AGENT_RUNS
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentIdParams {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAgentParams {
    draft: AgentDraft,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAgentParams {
    id: String,
    draft: AgentDraft,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetAgentEnabledParams {
    id: String,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarkAgentRunsParams {
    ids: Vec<String>,
    #[serde(default)]
    read: Option<bool>,
    #[serde(default)]
    archived: Option<bool>,
}

impl EngineRpc {
    pub(super) async fn agents_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let agents = self
            .agents
            .as_ref()
            .ok_or_else(|| RpcError::Failed("always-on agents are unavailable".into()))?;
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match method {
            methods::LIST_AGENTS => RpcReply::value(&agents.list().map_err(failed)?),
            methods::WATCH_AGENTS => {
                let stream = futures::stream::unfold(
                    (agents.watch(), agents.clone(), false),
                    |(mut rx, agents, emitted)| async move {
                        if emitted {
                            rx.changed().await.ok()?;
                        }
                        rx.borrow_and_update();
                        let snapshot = match agents.snapshot() {
                            Ok(snapshot) => serde_json::to_value(snapshot).ok()?,
                            Err(err) => {
                                tracing::warn!(%err, "WatchAgents: snapshot failed");
                                return None;
                            }
                        };
                        Some((snapshot, (rx, agents, true)))
                    },
                );
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::CREATE_AGENT => {
                let p: CreateAgentParams = parse_params(params)?;
                RpcReply::value(&agents.create(p.draft).map_err(failed)?)
            }
            methods::UPDATE_AGENT => {
                let p: UpdateAgentParams = parse_params(params)?;
                RpcReply::value(&agents.update(&p.id, p.draft).map_err(failed)?)
            }
            methods::DELETE_AGENT => {
                let p: AgentIdParams = parse_params(params)?;
                let deleted = agents.delete(&p.id).map_err(failed)?;
                RpcReply::value(&serde_json::json!({ "deleted": deleted }))
            }
            methods::SET_AGENT_ENABLED => {
                let p: SetAgentEnabledParams = parse_params(params)?;
                RpcReply::value(&agents.set_enabled(&p.id, p.enabled).map_err(failed)?)
            }
            methods::RUN_AGENT_NOW => {
                let p: AgentIdParams = parse_params(params)?;
                RpcReply::value(&agents.run_now(&p.id).await.map_err(failed)?)
            }
            methods::ROTATE_AGENT_WEBHOOK => {
                let p: AgentIdParams = parse_params(params)?;
                RpcReply::value(&agents.rotate_webhook(&p.id).map_err(failed)?)
            }
            methods::LIST_AGENT_RUNS => {
                let filter: AgentRunFilter = if params.is_null() {
                    AgentRunFilter::default()
                } else {
                    parse_params(params)?
                };
                RpcReply::value(&agents.list_runs(&filter).map_err(failed)?)
            }
            methods::MARK_AGENT_RUNS => {
                let p: MarkAgentRunsParams = parse_params(params)?;
                let changed = agents
                    .mark_runs(&p.ids, p.read, p.archived)
                    .map_err(failed)?;
                RpcReply::value(&serde_json::json!({ "changed": changed }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
