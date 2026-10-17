//! What a worktree's commands and processes see: template variables for
//! setup/teardown/wake commands and the environment variables injected into
//! its terminals, project actions and agent runs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use zeron_proto::WorktreePorts;

#[derive(Debug, Clone)]
pub struct WorktreeContext {
    pub worktree: PathBuf,
    pub root: PathBuf,
    pub project: String,
    pub branch: String,
    pub slug: String,
    pub ports: Option<WorktreePorts>,
}

impl WorktreeContext {
    pub fn lookup(&self, name: &str) -> Option<String> {
        match name {
            "worktree" => Some(self.worktree.to_string_lossy().into_owned()),
            "root" => Some(self.root.to_string_lossy().into_owned()),
            "project" => Some(self.project.clone()),
            "branch" => Some(self.branch.clone()),
            "slug" => Some(self.slug.clone()),
            "port" => self.ports.as_ref().map(|ports| ports.base.to_string()),
            _ => {
                let key = name.strip_prefix("port.")?;
                self.ports.as_ref()?.port(key).map(|port| port.to_string())
            }
        }
    }

    pub fn render(&self, template: &str) -> Result<String, String> {
        zeron_proto::worktree_env::render_template(template, |name| self.lookup(name))
    }

    pub fn render_all(&self, templates: &[String]) -> Result<Vec<String>, String> {
        templates
            .iter()
            .map(|template| self.render(template))
            .collect()
    }

    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::from([
            (
                "GODSPEED_PROJECT_ROOT".to_string(),
                self.root.to_string_lossy().into_owned(),
            ),
            (
                "GODSPEED_WORKTREE_PATH".to_string(),
                self.worktree.to_string_lossy().into_owned(),
            ),
            ("GODSPEED_WORKTREE_SLUG".to_string(), self.slug.clone()),
        ]);
        if let Some(ports) = &self.ports {
            environment.extend(port_environment(ports));
        }
        environment
    }
}

/// `GODSPEED_PORT`, `GODSPEED_PORT_1..N-1` and `PORT_<NAME>` per alias.
pub fn port_environment(ports: &WorktreePorts) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    environment.insert("GODSPEED_PORT".to_string(), ports.base.to_string());
    for offset in 1..ports.count {
        environment.insert(
            format!("GODSPEED_PORT_{offset}"),
            (ports.base + offset).to_string(),
        );
    }
    for (name, offset) in &ports.names {
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        environment.insert(format!("PORT_{name}"), (ports.base + offset).to_string());
    }
    environment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> WorktreeContext {
        WorktreeContext {
            worktree: "/wt/app/swift-otter".into(),
            root: "/code/app".into(),
            project: "app".into(),
            branch: "godspeed/swift-otter".into(),
            slug: "swift-otter".into(),
            ports: Some(WorktreePorts {
                base: 41010,
                count: 3,
                names: BTreeMap::from([("web".into(), 0), ("api-v2".into(), 2)]),
            }),
        }
    }

    #[test]
    fn templates_and_environment_expose_paths_and_ports() {
        let context = context();
        assert_eq!(
            context
                .render("createdb {{project}}_{{slug}} -p {{port.api-v2}} # {{branch}} {{port.1}}")
                .unwrap(),
            "createdb app_swift-otter -p 41012 # godspeed/swift-otter 41011"
        );
        assert!(context.render("{{port.nope}}").is_err());
        let environment = context.environment();
        assert_eq!(environment["GODSPEED_PORT"], "41010");
        assert_eq!(environment["GODSPEED_PORT_1"], "41011");
        assert_eq!(environment["GODSPEED_PORT_2"], "41012");
        assert!(!environment.contains_key("GODSPEED_PORT_3"));
        assert_eq!(environment["PORT_WEB"], "41010");
        assert_eq!(environment["PORT_API_V2"], "41012");
        assert_eq!(environment["GODSPEED_WORKTREE_PATH"], "/wt/app/swift-otter");
        assert_eq!(environment["GODSPEED_PROJECT_ROOT"], "/code/app");
    }
}
