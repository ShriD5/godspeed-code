//! Conductor service: repo discovery, creation, notes and the nesting rules
//! (`zeron_proto::conductor`). Rows live in the registry; this is the
//! `WorkspaceHost`-facing logic.

mod discover;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::Utc;
use zeron_proto::{
    Chat, Conductor, ConductorMembership, ConductorNode, ConductorRepo, CreateConductorParams,
    MAX_CONDUCTOR_DEPTH, MAX_NOTES_BYTES, UpdateConductorParams, can_spawn_at,
};

pub use discover::discover_repos;

use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id};

/// Instructions and notes to put in front of a conductor chat's first prompt:
/// the main agent's role at depth 0, the sub-agent's below. `None` for a chat
/// outside every conductor.
pub fn conductor_prompt_prefix(ws: &WorkspaceHost, chat_id: &str) -> Option<String> {
    let membership = ws.conductor_membership(chat_id).ok()??;
    let conductor = ws.conductor(&membership.conductor_id).ok()??;
    Some(if membership.depth == 0 {
        zeron_proto::main_agent_instructions(&conductor, &ws.conductor_repos(&conductor))
    } else {
        zeron_proto::sub_agent_instructions(&conductor, membership.depth)
    })
}

/// Deepest chain the tree walk follows; far above the cap, it only stops
/// loops in a corrupt registry.
const MAX_TREE_HOPS: usize = 16;

fn no_such_conductor(id: &str) -> EngineError {
    EngineError::Other(format!("no such conductor: {id}"))
}

impl WorkspaceHost {
    /// Make a conductor over `params.folder`: a space per repo found there
    /// (reusing any that exist), the main chat, and the registry row.
    /// Idempotent by conductor id so optimistic retries never duplicate.
    pub fn create_conductor(
        &self,
        params: CreateConductorParams,
    ) -> Result<Conductor, EngineError> {
        let folder = params.folder.trim().trim_end_matches('/').to_string();
        if folder.is_empty() {
            return Err(EngineError::Other("A conductor needs a folder".into()));
        }
        let own_device = self.device_id().to_string();
        let device_id = params
            .device_id
            .clone()
            .unwrap_or_else(|| own_device.clone());
        if device_id != own_device {
            return Err(EngineError::Other(
                "A conductor is created on the device that holds its folder".into(),
            ));
        }
        let id = if params.id.trim().is_empty() {
            new_id()
        } else {
            params.id.trim().to_string()
        };
        if let Some(existing) = self.conductor(&id)? {
            return Ok(existing);
        }
        if params.chat_id.trim().is_empty() {
            return Err(EngineError::Other(
                "A conductor needs a main chat id".into(),
            ));
        }
        if !Path::new(&folder).is_dir() {
            return Err(EngineError::Other(format!("{folder} is not a folder")));
        }
        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                Path::new(&folder)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.clone())
            });
        let purpose = params
            .purpose
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);

        let space_ids = self.ensure_repo_spaces(&device_id, &folder, &[])?;
        self.create_chat(
            &params.chat_id,
            None,
            Some(&device_id),
            params.config.clone(),
            Some(folder.clone()),
        )?;
        let conductor = Conductor {
            id,
            name,
            purpose,
            folder,
            device_id,
            main_chat_id: params.chat_id.clone(),
            space_ids,
            config: params.config,
            notes: String::new(),
            archived: false,
            created_at: Utc::now(),
        };
        self.mutate(|doc| doc.upsert_conductor(&conductor))?;
        Ok(conductor)
    }

    /// Spaces for every repo in `folder`, existing ones first (in `known`
    /// order), then any new ones. Never removes a space.
    fn ensure_repo_spaces(
        &self,
        device_id: &str,
        folder: &str,
        known: &[String],
    ) -> Result<Vec<String>, EngineError> {
        let mut ids: Vec<String> = known.to_vec();
        for repo in discover_repos(Path::new(folder)) {
            let path = repo.to_string_lossy().into_owned();
            let existing = self
                .read_spaces()?
                .into_iter()
                .find(|s| s.device_id == device_id && s.path == path);
            let space_id = match existing {
                Some(space) => space.id,
                None => {
                    let space_id = new_id();
                    self.create_space(&space_id, device_id, &path, None, true)?;
                    space_id
                }
            };
            if !ids.contains(&space_id) {
                ids.push(space_id);
            }
        }
        Ok(ids)
    }

    pub fn conductor(&self, id: &str) -> Result<Option<Conductor>, EngineError> {
        Ok(self.read(|doc| doc.conductor(id)))
    }

    pub fn conductors(&self) -> Result<Vec<Conductor>, EngineError> {
        Ok(self.read(|doc| doc.conductors()))
    }

    pub fn update_conductor(&self, patch: UpdateConductorParams) -> Result<Conductor, EngineError> {
        if !self.mutate(|doc| doc.update_conductor(&patch))? {
            return Err(no_such_conductor(&patch.id));
        }
        let conductor = self
            .conductor(&patch.id)?
            .ok_or_else(|| no_such_conductor(&patch.id))?;
        // The new config reaches the agent with the main chat's next fresh
        // session, so it goes on the chat row too.
        if let Some(config) = &patch.config {
            self.set_chat_config(&conductor.main_chat_id, config)?;
        }
        Ok(conductor)
    }

    /// Add spaces for repos that appeared in the folder since creation.
    pub fn rescan_conductor(&self, id: &str) -> Result<Conductor, EngineError> {
        let conductor = self.conductor(id)?.ok_or_else(|| no_such_conductor(id))?;
        let space_ids = self.ensure_repo_spaces(
            &conductor.device_id,
            &conductor.folder,
            &conductor.space_ids,
        )?;
        if space_ids != conductor.space_ids {
            self.mutate(|doc| doc.set_conductor_spaces(id, &space_ids))?;
        }
        self.conductor(id)?.ok_or_else(|| no_such_conductor(id))
    }

    pub fn set_conductor_notes(&self, id: &str, notes: &str) -> Result<(), EngineError> {
        if notes.len() > MAX_NOTES_BYTES {
            return Err(EngineError::Other(format!(
                "Notes are over the {} KB limit",
                MAX_NOTES_BYTES / 1024
            )));
        }
        if self.mutate(|doc| doc.set_conductor_notes(id, notes))? {
            Ok(())
        } else {
            Err(no_such_conductor(id))
        }
    }

    /// The main chat and every chat below it, parents before children.
    pub fn conductor_tree(&self, id: &str) -> Result<Vec<ConductorNode>, EngineError> {
        let conductor = self.conductor(id)?.ok_or_else(|| no_such_conductor(id))?;
        let chats = self.read_chats()?;
        let mut children: HashMap<&str, Vec<&Chat>> = HashMap::new();
        for chat in &chats {
            if let Some(parent) = chat.parent_chat_id.as_deref() {
                children.entry(parent).or_default().push(chat);
            }
        }
        for siblings in children.values_mut() {
            siblings.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        }
        let mut nodes = Vec::new();
        let Some(main) = chats.iter().find(|c| c.id == conductor.main_chat_id) else {
            return Ok(nodes);
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut frontier = vec![(main, 0u8)];
        while let Some((chat, depth)) = frontier.pop() {
            if !seen.insert(chat.id.as_str()) || usize::from(depth) > MAX_TREE_HOPS {
                continue;
            }
            nodes.push(ConductorNode {
                chat_id: chat.id.clone(),
                parent_chat_id: chat.parent_chat_id.clone(),
                depth,
                title: chat.title.clone(),
                space_id: chat.space_id.clone(),
                archived: chat.archived,
            });
            for child in children.get(chat.id.as_str()).into_iter().flatten().rev() {
                frontier.push((child, depth.saturating_add(1)));
            }
        }
        Ok(nodes)
    }

    /// The repos a conductor covers, for prompts and tool output. Spaces that
    /// were deleted since are skipped.
    pub fn conductor_repos(&self, conductor: &Conductor) -> Vec<ConductorRepo> {
        conductor
            .space_ids
            .iter()
            .filter_map(|id| self.space(id).ok().flatten())
            .map(|space| ConductorRepo {
                name: space.name.clone().unwrap_or_else(|| {
                    Path::new(&space.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| space.path.clone())
                }),
                space_id: space.id,
                path: space.path,
            })
            .collect()
    }

    pub fn conductor_membership(
        &self,
        chat_id: &str,
    ) -> Result<Option<ConductorMembership>, EngineError> {
        Ok(self.read(|doc| doc.conductor_of_chat(chat_id)))
    }

    /// Whether `chat` may start (or be woken by) child chats: any top-level
    /// chat, or a conductor member still above the depth cap. Everything else
    /// keeps the one-level rule.
    pub fn chat_may_parent(&self, chat: &Chat) -> Result<bool, EngineError> {
        if chat.is_top_level() {
            return Ok(true);
        }
        Ok(self
            .conductor_membership(&chat.id)?
            .is_some_and(|m| can_spawn_at(m.depth)))
    }

    /// Refuse a child under a conductor member that is already at the cap.
    pub(crate) fn check_conductor_depth(&self, parent_id: &str) -> Result<(), EngineError> {
        match self.conductor_membership(parent_id)? {
            Some(m) if !can_spawn_at(m.depth) => Err(EngineError::Other(format!(
                "Sub-agents nest at most {MAX_CONDUCTOR_DEPTH} levels below a conductor's main chat"
            ))),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeron_sync::DocsStore;

    use super::*;
    use crate::workspace_host::{WorkspaceHost, WorkspaceHostConfig};

    fn host(dir: &Path) -> WorkspaceHost {
        WorkspaceHost::open(
            Arc::new(DocsStore::open(dir).unwrap()),
            WorkspaceHostConfig {
                device_id: "dev-a".into(),
                device_name: "Dev A".into(),
                platform: "macos".into(),
                org_id: "org".into(),
                user_id: "user".into(),
                edge: None,
                local_only: false,
            },
        )
        .unwrap()
    }

    fn folder_with_repos(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            std::fs::create_dir_all(dir.path().join(name).join(".git")).unwrap();
        }
        dir
    }

    fn params(folder: &Path, chat_id: &str) -> CreateConductorParams {
        CreateConductorParams {
            chat_id: chat_id.into(),
            id: String::new(),
            folder: folder.to_string_lossy().into_owned(),
            name: None,
            purpose: None,
            device_id: None,
            config: None,
        }
    }

    /// Start a chat under `parent`, as the MCP does for a sub-agent.
    fn spawn(host: &WorkspaceHost, id: &str, parent: &str) -> Result<(), EngineError> {
        host.create_chat_with_parent(
            id,
            None,
            Some("dev-a"),
            None,
            Some("/tmp".into()),
            Some(parent.into()),
        )
    }

    #[tokio::test]
    async fn create_makes_a_space_per_repo_and_a_main_chat() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&["api", "web"]);
        let conductor = host
            .create_conductor(params(folder.path(), "main"))
            .unwrap();
        assert_eq!(conductor.space_ids.len(), 2);
        assert_eq!(host.read_spaces().unwrap().len(), 2);
        let main = host.chat("main").unwrap().unwrap();
        assert_eq!(main.cwd.as_deref(), Some(conductor.folder.as_str()));
        assert_eq!(host.conductors().unwrap(), vec![conductor.clone()]);
        assert_eq!(
            host.conductor_membership("main").unwrap().map(|m| m.depth),
            Some(0)
        );
        // A retry with the same id changes nothing.
        let mut again = params(folder.path(), "main");
        again.id = conductor.id.clone();
        assert_eq!(host.create_conductor(again).unwrap(), conductor);
        assert_eq!(host.conductors().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_folder_without_repos_still_creates() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&[]);
        let conductor = host
            .create_conductor(params(folder.path(), "main"))
            .unwrap();
        assert!(conductor.space_ids.is_empty());
    }

    #[tokio::test]
    async fn a_second_conductor_over_the_same_folder_reuses_spaces() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&["api", "web"]);
        let first = host
            .create_conductor(params(folder.path(), "main-1"))
            .unwrap();
        let second = host
            .create_conductor(params(folder.path(), "main-2"))
            .unwrap();
        assert_eq!(first.space_ids, second.space_ids);
        assert_eq!(host.read_spaces().unwrap().len(), 2);
        assert_ne!(first.id, second.id);
    }

    #[tokio::test]
    async fn a_missing_folder_or_other_device_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let missing = data.path().join("nope");
        assert!(host.create_conductor(params(&missing, "main")).is_err());
        let folder = folder_with_repos(&[]);
        let mut remote = params(folder.path(), "main");
        remote.device_id = Some("dev-b".into());
        assert!(host.create_conductor(remote).is_err());
    }

    #[tokio::test]
    async fn nesting_is_allowed_to_the_cap_and_refused_beyond() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&[]);
        host.create_conductor(params(folder.path(), "main"))
            .unwrap();
        spawn(&host, "d1", "main").unwrap();
        spawn(&host, "d2", "d1").unwrap();
        spawn(&host, "d3", "d2").unwrap();
        let err = spawn(&host, "d4", "d3").unwrap_err().to_string();
        assert!(err.contains(&MAX_CONDUCTOR_DEPTH.to_string()), "{err}");
        assert!(host.chat("d4").unwrap().is_none());
    }

    #[tokio::test]
    async fn only_conductor_members_may_parent_below_the_top_level() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&[]);
        host.create_conductor(params(folder.path(), "main"))
            .unwrap();
        spawn(&host, "member", "main").unwrap();
        host.create_chat("plain", None, Some("dev-a"), None, Some("/tmp".into()))
            .unwrap();
        spawn(&host, "plain-child", "plain").unwrap();
        let may = |id: &str| {
            host.chat_may_parent(&host.chat(id).unwrap().unwrap())
                .unwrap()
        };
        assert!(may("main"));
        assert!(may("member"));
        assert!(may("plain"));
        assert!(!may("plain-child"), "the one-level rule is unchanged");
        spawn(&host, "d2", "member").unwrap();
        spawn(&host, "d3", "d2").unwrap();
        assert!(!may("d3"), "at the cap");
    }

    #[tokio::test]
    async fn notes_over_the_cap_are_rejected_and_small_ones_stored() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&[]);
        let c = host
            .create_conductor(params(folder.path(), "main"))
            .unwrap();
        host.set_conductor_notes(&c.id, "ship fridays").unwrap();
        assert_eq!(
            host.conductor(&c.id).unwrap().unwrap().notes,
            "ship fridays"
        );
        assert!(
            host.set_conductor_notes(&c.id, &"x".repeat(MAX_NOTES_BYTES + 1))
                .is_err()
        );
        assert!(host.set_conductor_notes("missing", "x").is_err());
    }

    #[tokio::test]
    async fn rescan_adds_new_repos_and_never_removes() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&["api"]);
        let c = host
            .create_conductor(params(folder.path(), "main"))
            .unwrap();
        assert_eq!(c.space_ids.len(), 1);
        std::fs::create_dir_all(folder.path().join("web/.git")).unwrap();
        let rescanned = host.rescan_conductor(&c.id).unwrap();
        assert_eq!(rescanned.space_ids.len(), 2);
        assert_eq!(rescanned.space_ids[0], c.space_ids[0]);
        std::fs::remove_dir_all(folder.path().join("api")).unwrap();
        assert_eq!(host.rescan_conductor(&c.id).unwrap().space_ids.len(), 2);
    }

    #[tokio::test]
    async fn update_changes_fields_and_the_tree_lists_sub_agents() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&[]);
        let c = host
            .create_conductor(params(folder.path(), "main"))
            .unwrap();
        spawn(&host, "d1", "main").unwrap();
        spawn(&host, "d2", "d1").unwrap();
        let updated = host
            .update_conductor(UpdateConductorParams {
                id: c.id.clone(),
                name: Some("Renamed".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(updated.name, "Renamed");
        let tree = host.conductor_tree(&c.id).unwrap();
        let shape: Vec<_> = tree.iter().map(|n| (n.chat_id.as_str(), n.depth)).collect();
        assert_eq!(shape, [("main", 0), ("d1", 1), ("d2", 2)]);
        assert!(host.conductor_tree("missing").is_err());
    }

    #[tokio::test]
    async fn conductor_prompt_prefix_depends_on_depth_and_membership() {
        let data = tempfile::tempdir().unwrap();
        let host = host(data.path());
        let folder = folder_with_repos(&["api"]);
        let c = host
            .create_conductor(params(folder.path(), "main"))
            .unwrap();
        host.set_conductor_notes(&c.id, "blunt reviews").unwrap();
        spawn(&host, "sub", "main").unwrap();
        host.create_chat("plain", None, Some("dev-a"), None, Some("/tmp".into()))
            .unwrap();
        let main = conductor_prompt_prefix(&host, "main").unwrap();
        assert!(main.contains("main agent"));
        assert!(main.contains("blunt reviews"));
        assert!(main.contains("/api"));
        let sub = conductor_prompt_prefix(&host, "sub").unwrap();
        assert!(sub.contains("sub-agent"));
        assert!(sub.contains("blunt reviews"));
        assert_eq!(conductor_prompt_prefix(&host, "plain"), None);
    }
}
