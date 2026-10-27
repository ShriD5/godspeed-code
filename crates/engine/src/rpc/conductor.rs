//! Conductor RPCs (`crate::conductor`). Forwarded on `targetDeviceId` to the
//! device that holds the conductor's folder.

use super::*;
use zeron_proto::{CreateConductorParams, UpdateConductorParams};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::CREATE_CONDUCTOR
            | methods::LIST_CONDUCTORS
            | methods::UPDATE_CONDUCTOR
            | methods::RESCAN_CONDUCTOR
            | methods::GET_CONDUCTOR_NOTES
            | methods::SET_CONDUCTOR_NOTES
            | methods::SET_CONDUCTOR_INSTRUCTIONS
            | methods::GET_CONDUCTOR_TREE
            | methods::GET_CONDUCTOR_MEMBERSHIP
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConductorIdParams {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetNotesParams {
    id: String,
    notes: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetInstructionsParams {
    id: String,
    instructions: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MembershipParams {
    chat_id: String,
}

impl EngineRpc {
    pub(super) fn conductor_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        let ws = &self.workspace;
        match method {
            methods::CREATE_CONDUCTOR => {
                let p: CreateConductorParams = parse_params(params)?;
                RpcReply::value(&ws.create_conductor(p).map_err(failed)?)
            }
            methods::LIST_CONDUCTORS => RpcReply::value(&ws.conductors().map_err(failed)?),
            methods::UPDATE_CONDUCTOR => {
                let p: UpdateConductorParams = parse_params(params)?;
                RpcReply::value(&ws.update_conductor(p).map_err(failed)?)
            }
            methods::RESCAN_CONDUCTOR => {
                let p: ConductorIdParams = parse_params(params)?;
                RpcReply::value(&ws.rescan_conductor(&p.id).map_err(failed)?)
            }
            methods::GET_CONDUCTOR_NOTES => {
                let p: ConductorIdParams = parse_params(params)?;
                let conductor = ws
                    .conductor(&p.id)
                    .map_err(failed)?
                    .ok_or_else(|| RpcError::Failed(format!("no such conductor: {}", p.id)))?;
                RpcReply::value(&serde_json::json!({ "notes": conductor.notes }))
            }
            methods::SET_CONDUCTOR_NOTES => {
                let p: SetNotesParams = parse_params(params)?;
                ws.set_conductor_notes(&p.id, &p.notes).map_err(failed)?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SET_CONDUCTOR_INSTRUCTIONS => {
                let p: SetInstructionsParams = parse_params(params)?;
                let conductor = ws
                    .conductor(&p.id)
                    .map_err(failed)?
                    .ok_or_else(|| RpcError::Failed(format!("no such conductor: {}", p.id)))?;
                let changed = ws
                    .set_conductor_instructions(&p.id, &p.instructions)
                    .map_err(failed)?;
                if changed {
                    let notice = zeron_proto::conductor::instructions_changed_notice(
                        p.instructions.trim(),
                    );
                    if let Err(error) =
                        self.doc_host
                            .queue_message(&conductor.main_chat_id, &notice, Vec::new())
                    {
                        tracing::warn!(
                            conductor = %p.id,
                            error = %error,
                            "could not tell the conductor its instructions changed"
                        );
                    }
                }
                RpcReply::value(&serde_json::json!({ "changed": changed }))
            }
            methods::GET_CONDUCTOR_TREE => {
                let p: ConductorIdParams = parse_params(params)?;
                RpcReply::value(&ws.conductor_tree(&p.id).map_err(failed)?)
            }
            methods::GET_CONDUCTOR_MEMBERSHIP => {
                let p: MembershipParams = parse_params(params)?;
                RpcReply::value(&ws.conductor_membership(&p.chat_id).map_err(failed)?)
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
