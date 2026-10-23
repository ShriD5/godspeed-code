//! Conductor rows (`zeron_proto::conductor`): a synced row kind naming a main
//! chat, a folder, the repo spaces found in it, an agent config and notes.
//! Chats carry no conductor field; membership is derived here by walking
//! `parent_chat_id` up to a conductor's main chat.

use super::*;
use zeron_proto::{Conductor, ConductorMembership, UpdateConductorParams};

/// Longest parent chain the membership walk follows. Far above
/// `MAX_CONDUCTOR_DEPTH`; it only exists to stop a parent cycle.
const MAX_WALK_HOPS: usize = 16;

fn conductor_fields(conductor: &Conductor) -> BTreeMap<String, Value> {
    match serde_json::to_value(conductor) {
        Ok(Value::Object(map)) => {
            let mut set: BTreeMap<String, Value> = map.into_iter().collect();
            // Upserts only set the keys present, so a cleared option must be
            // written explicitly or an older value would survive.
            set.entry("purpose".into()).or_insert(Value::Null);
            set.entry("config".into()).or_insert(Value::Null);
            set
        }
        _ => BTreeMap::new(),
    }
}

impl RegistryDoc {
    pub fn upsert_conductor(&mut self, conductor: &Conductor) -> Result<(), DocError> {
        if conductor.id.is_empty() || conductor.main_chat_id.is_empty() {
            return Err(DocError::Schema(
                "A conductor needs an id and a main chat".into(),
            ));
        }
        self.write(
            KIND_CONDUCTORS,
            &conductor.id.clone(),
            OpKind::Upsert,
            conductor_fields(conductor),
        );
        Ok(())
    }

    pub fn conductor(&self, id: &str) -> Option<Conductor> {
        self.overlay_row(KIND_CONDUCTORS, id)
            .and_then(|row| row_to::<Conductor>(&row))
    }

    /// Every live conductor, oldest first.
    pub fn conductors(&self) -> Vec<Conductor> {
        let mut all: Vec<Conductor> = self.read_kind(KIND_CONDUCTORS);
        all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        all
    }

    /// Apply the set fields of `patch`. `false` when the conductor is gone.
    pub fn update_conductor(&mut self, patch: &UpdateConductorParams) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CONDUCTORS, &patch.id) {
            return Ok(false);
        }
        let mut set = BTreeMap::new();
        if let Some(name) = patch.name.as_deref() {
            let name = name.trim();
            if name.is_empty() {
                return Err(DocError::Schema("A conductor needs a name".into()));
            }
            set.insert("name".into(), json!(name));
        }
        if let Some(purpose) = patch.purpose.as_deref() {
            let purpose = purpose.trim();
            set.insert(
                "purpose".into(),
                if purpose.is_empty() {
                    Value::Null
                } else {
                    json!(purpose)
                },
            );
        }
        if let Some(config) = &patch.config {
            set.insert(
                "config".into(),
                serde_json::to_value(config).map_err(|e| DocError::Schema(e.to_string()))?,
            );
        }
        if let Some(archived) = patch.archived {
            set.insert("archived".into(), json!(archived));
        }
        if !set.is_empty() {
            self.write(KIND_CONDUCTORS, &patch.id, OpKind::Update, set);
        }
        Ok(true)
    }

    pub fn set_conductor_notes(&mut self, id: &str, notes: &str) -> Result<bool, DocError> {
        if notes.len() > zeron_proto::MAX_NOTES_BYTES {
            return Err(DocError::Schema(format!(
                "Notes are over the {} KB limit",
                zeron_proto::MAX_NOTES_BYTES / 1024
            )));
        }
        if !self.row_exists(KIND_CONDUCTORS, id) {
            return Ok(false);
        }
        self.write(
            KIND_CONDUCTORS,
            id,
            OpKind::Update,
            fields([("notes", json!(notes))]),
        );
        Ok(true)
    }

    pub fn set_conductor_spaces(
        &mut self,
        id: &str,
        space_ids: &[String],
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CONDUCTORS, id) {
            return Ok(false);
        }
        self.write(
            KIND_CONDUCTORS,
            id,
            OpKind::Update,
            fields([("spaceIds", json!(space_ids))]),
        );
        Ok(true)
    }

    /// Where `chat_id` sits in a conductor: depth 0 for a main chat, 1 for a
    /// sub-agent it started, and so on. `None` for chats outside every
    /// conductor, including chats whose parent chain dangles or loops.
    pub fn conductor_of_chat(&self, chat_id: &str) -> Option<ConductorMembership> {
        let mains: HashMap<String, String> = self
            .conductors()
            .into_iter()
            .map(|c| (c.main_chat_id, c.id))
            .collect();
        if mains.is_empty() {
            return None;
        }
        let mut current = chat_id.to_string();
        for depth in 0..=MAX_WALK_HOPS {
            if let Some(conductor_id) = mains.get(&current) {
                return Some(ConductorMembership {
                    conductor_id: conductor_id.clone(),
                    depth: u8::try_from(depth).unwrap_or(u8::MAX),
                });
            }
            let parent = self
                .overlay_row(KIND_CHATS, &current)
                .and_then(|row| row_to::<crate::workspace::RawChat>(&row))
                .map(Chat::from)
                .and_then(|chat| chat.parent_chat_id)
                .filter(|p| !p.trim().is_empty())?;
            current = parent;
        }
        None
    }
}
