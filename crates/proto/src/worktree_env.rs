//! Worktree environments: what a project needs so several worktrees can run
//! side by side. Files copied in from the main checkout, a block of ports per
//! worktree, setup/teardown/wake commands and a branch-name template.
//!
//! The same shape is stored on the owning engine and read from the `env`
//! section of `godspeed.json`, which also accepts shorthands (a bare string
//! for a copy glob or a command, a bare number for the port block).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ProjectActionRun, valid_worktree_branch_prefix};

pub const MAX_COPY_RULES: usize = 50;
pub const MAX_GLOB_BYTES: usize = 512;
pub const MAX_ENV_COMMANDS: usize = 20;
pub const MAX_ENV_COMMAND_BYTES: usize = 16 * 1024;
pub const MAX_PORT_BLOCK: u16 = 100;
pub const MAX_PORT_NAMES: usize = 32;
pub const MAX_BRANCH_TEMPLATE_BYTES: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawEnvironment", rename_all = "camelCase")]
pub struct WorktreeEnvironment {
    pub copy: Vec<WorktreeCopyRule>,
    pub ports: Option<WorktreePortBlock>,
    pub setup: Vec<String>,
    pub teardown: Vec<String>,
    pub wake: Vec<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorktreeCopyRule {
    pub glob: String,
    #[serde(default)]
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorktreePortBlock {
    pub count: u16,
    #[serde(default)]
    pub names: BTreeMap<String, u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawEnvironment {
    #[serde(default)]
    copy: Vec<RawCopyRule>,
    #[serde(default)]
    ports: Option<RawPorts>,
    #[serde(default)]
    setup: Commands,
    #[serde(default)]
    teardown: Commands,
    #[serde(default)]
    wake: Commands,
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawCopyRule {
    Glob(String),
    Rule(WorktreeCopyRule),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawPorts {
    Count(u16),
    Block(WorktreePortBlock),
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Commands {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl From<Commands> for Vec<String> {
    fn from(commands: Commands) -> Self {
        match commands {
            Commands::None => Vec::new(),
            Commands::One(command) => vec![command],
            Commands::Many(commands) => commands,
        }
    }
}

impl From<RawEnvironment> for WorktreeEnvironment {
    fn from(raw: RawEnvironment) -> Self {
        Self {
            copy: raw
                .copy
                .into_iter()
                .map(|rule| match rule {
                    RawCopyRule::Glob(glob) => WorktreeCopyRule {
                        glob,
                        symlink: false,
                    },
                    RawCopyRule::Rule(rule) => rule,
                })
                .collect(),
            ports: raw.ports.map(|ports| match ports {
                RawPorts::Count(count) => WorktreePortBlock {
                    count,
                    names: BTreeMap::new(),
                },
                RawPorts::Block(block) => block,
            }),
            setup: raw.setup.into(),
            teardown: raw.teardown.into(),
            wake: raw.wake.into(),
            branch: raw.branch,
        }
    }
}

impl WorktreeEnvironment {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Trim entries and drop blank ones, then check every limit and template.
    pub fn normalized(mut self) -> Result<Self, String> {
        for rule in &mut self.copy {
            rule.glob = rule.glob.trim().to_string();
        }
        self.copy.retain(|rule| !rule.glob.is_empty());
        for commands in [&mut self.setup, &mut self.teardown, &mut self.wake] {
            for command in commands.iter_mut() {
                *command = command.trim().to_string();
            }
            commands.retain(|command| !command.is_empty());
        }
        self.branch = self
            .branch
            .map(|branch| branch.trim().to_string())
            .filter(|branch| !branch.is_empty());
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.copy.len() > MAX_COPY_RULES {
            return Err(format!("Copy at most {MAX_COPY_RULES} patterns"));
        }
        for rule in &self.copy {
            validate_glob(&rule.glob)?;
        }
        if let Some(ports) = &self.ports {
            validate_ports(ports)?;
        }
        let port_names = self.ports.as_ref().map(|ports| &ports.names);
        let port_count = self.ports.as_ref().map_or(0, |ports| ports.count);
        for (label, commands) in [
            ("Setup", &self.setup),
            ("Teardown", &self.teardown),
            ("Wake", &self.wake),
        ] {
            if commands.len() > MAX_ENV_COMMANDS {
                return Err(format!("{label} has more than {MAX_ENV_COMMANDS} commands"));
            }
            for command in commands {
                if command.len() > MAX_ENV_COMMAND_BYTES {
                    return Err(format!(
                        "{label} commands must not exceed {MAX_ENV_COMMAND_BYTES} bytes"
                    ));
                }
                for variable in template_variables(command)? {
                    check_command_variable(variable, port_count, port_names)
                        .map_err(|error| format!("{label}: {error}"))?;
                }
            }
        }
        if let Some(branch) = &self.branch {
            validate_branch_template(branch)?;
        }
        Ok(())
    }
}

fn validate_glob(glob: &str) -> Result<(), String> {
    if glob.len() > MAX_GLOB_BYTES {
        return Err(format!(
            "Copy patterns must not exceed {MAX_GLOB_BYTES} bytes"
        ));
    }
    if glob.starts_with('/') || glob.starts_with('\\') || glob.contains(':') {
        return Err(format!("\"{glob}\" must be relative to the project folder"));
    }
    for part in glob.split(['/', '\\']) {
        if part == ".." {
            return Err(format!("\"{glob}\" must stay inside the project folder"));
        }
        if part.eq_ignore_ascii_case(".git") {
            return Err(format!("\"{glob}\" would copy Git's own data"));
        }
    }
    Ok(())
}

fn validate_ports(ports: &WorktreePortBlock) -> Result<(), String> {
    if ports.count == 0 || ports.count > MAX_PORT_BLOCK {
        return Err(format!(
            "Reserve between 1 and {MAX_PORT_BLOCK} ports per worktree"
        ));
    }
    if ports.names.len() > MAX_PORT_NAMES {
        return Err(format!("Name at most {MAX_PORT_NAMES} ports"));
    }
    for (name, offset) in &ports.names {
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || name.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(format!(
                "Port name \"{name}\" must use letters, digits, '-' or '_'"
            ));
        }
        if *offset >= ports.count {
            return Err(format!(
                "Port \"{name}\" uses offset {offset}, outside the block of {}",
                ports.count
            ));
        }
    }
    Ok(())
}

fn check_command_variable(
    variable: &str,
    port_count: u16,
    port_names: Option<&BTreeMap<String, u16>>,
) -> Result<(), String> {
    match variable {
        "worktree" | "branch" | "project" | "root" | "slug" => Ok(()),
        "port" if port_count > 0 => Ok(()),
        "port" => Err("{{port}} needs a port block".into()),
        _ => match variable.strip_prefix("port.") {
            Some(key) => match key.parse::<u16>() {
                Ok(offset) if offset < port_count => Ok(()),
                Ok(_) => Err(format!("{{{{{variable}}}}} is outside the port block")),
                Err(_) if port_names.is_some_and(|names| names.contains_key(key)) => Ok(()),
                Err(_) => Err(format!("{{{{{variable}}}}} names no port")),
            },
            None => Err(format!("unknown variable {{{{{variable}}}}}")),
        },
    }
}

/// A branch template renders to a valid, per-worktree-unique branch name.
pub fn validate_branch_template(template: &str) -> Result<(), String> {
    if template.len() > MAX_BRANCH_TEMPLATE_BYTES {
        return Err(format!(
            "Branch template must not exceed {MAX_BRANCH_TEMPLATE_BYTES} bytes"
        ));
    }
    let variables = template_variables(template)?;
    if let Some(unknown) = variables
        .iter()
        .find(|variable| !matches!(**variable, "prefix" | "slug" | "project"))
    {
        return Err(format!(
            "Branch template can't use {{{{{unknown}}}}} (use {{{{prefix}}}}, {{{{slug}}}} or {{{{project}}}})"
        ));
    }
    if !variables.contains(&"slug") {
        return Err(
            "Branch template must include {{slug}} so every worktree gets its own branch".into(),
        );
    }
    let sample = render_template(template, |name| match name {
        "prefix" => Some("godspeed/".into()),
        "slug" => Some("swift-otter".into()),
        "project" => Some("app".into()),
        _ => None,
    })?;
    if valid_branch_name(&sample) {
        Ok(())
    } else {
        Err(format!(
            "\"{template}\" doesn't make a valid Git branch name"
        ))
    }
}

/// `git check-ref-format --branch` rules for the characters templates produce.
pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 250
        && valid_worktree_branch_prefix(name)
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
}

/// The `{{name}}` placeholders in `template`, in order.
pub fn template_variables(template: &str) -> Result<Vec<&str>, String> {
    let mut variables = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unclosed {{{{ in \"{template}\""))?;
        let name = after[..end].trim();
        if name.is_empty() {
            return Err(format!("empty {{{{}}}} in \"{template}\""));
        }
        variables.push(name);
        rest = &after[end + 2..];
    }
    Ok(variables)
}

/// Replace every `{{name}}` with `lookup(name)`; an unknown name is an error.
pub fn render_template(
    template: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unclosed {{{{ in \"{template}\""))?;
        let name = after[..end].trim();
        let value = lookup(name).ok_or_else(|| format!("unknown variable {{{{{name}}}}}"))?;
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The ports reserved for one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreePorts {
    pub base: u16,
    pub count: u16,
    #[serde(default)]
    pub names: BTreeMap<String, u16>,
}

impl WorktreePorts {
    pub fn last(&self) -> u16 {
        self.base + self.count.saturating_sub(1)
    }

    pub fn port(&self, key: &str) -> Option<u16> {
        let offset = match key.parse::<u16>() {
            Ok(offset) => offset,
            Err(_) => *self.names.get(key)?,
        };
        (offset < self.count).then(|| self.base + offset)
    }

    /// `:4100` or `:4100–4109`.
    pub fn label(&self) -> String {
        if self.count <= 1 {
            format!(":{}", self.base)
        } else {
            format!(":{}–{}", self.base, self.last())
        }
    }

    /// `web :4100 · api :4101`, or the range when no port is named.
    pub fn detail(&self) -> String {
        if self.names.is_empty() {
            return self.label();
        }
        let mut named: Vec<(&String, &u16)> = self.names.iter().collect();
        named.sort_by_key(|(name, offset)| (**offset, (*name).clone()));
        named
            .into_iter()
            .map(|(name, offset)| format!("{name} :{}", self.base + offset))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum WorktreeSetupState {
    Running,
    Succeeded,
    Failed {
        step: usize,
        exit_code: i32,
        command: String,
    },
}

/// One worktree's live environment state on its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeEnvironmentStatus {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<WorktreePorts>,
    #[serde(default)]
    pub sleeping: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<WorktreeSetupState>,
}

/// A project's saved environment plus an importable `godspeed.json` `env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeEnvironmentSnapshot {
    pub space_id: String,
    pub environment: WorktreeEnvironment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importable: Option<WorktreeEnvironment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_file_issue: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeWorktreeOutcome {
    pub status: WorktreeEnvironmentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_action: Option<ProjectActionRun>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthands_normalize_to_the_stored_shape() {
        let env: WorktreeEnvironment = serde_json::from_str(
            r#"{
                "copy": [".env*", {"glob": "node_modules", "symlink": true}],
                "ports": 3,
                "setup": "createdb app_{{slug}} -T app_dev",
                "teardown": ["dropdb app_{{slug}}"],
                "branch": "{{prefix}}{{slug}}"
            }"#,
        )
        .unwrap();
        assert_eq!(env.copy[0].glob, ".env*");
        assert!(!env.copy[0].symlink);
        assert!(env.copy[1].symlink);
        assert_eq!(env.ports.as_ref().unwrap().count, 3);
        assert_eq!(env.setup, ["createdb app_{{slug}} -T app_dev"]);
        assert!(env.wake.is_empty());
        env.validate().unwrap();

        let stored = serde_json::to_value(&env).unwrap();
        let reread: WorktreeEnvironment = serde_json::from_value(stored).unwrap();
        assert_eq!(reread, env);
        assert!(serde_json::from_str::<WorktreeEnvironment>(r#"{"unknown":1}"#).is_err());
    }

    #[test]
    fn validation_rejects_escapes_bad_ports_and_unknown_variables() {
        let with = |json: &str| {
            serde_json::from_str::<WorktreeEnvironment>(json)
                .unwrap()
                .validate()
        };
        assert!(with(r#"{"copy":["../secrets"]}"#).is_err());
        assert!(with(r#"{"copy":[".git/config"]}"#).is_err());
        assert!(with(r#"{"copy":["/etc/passwd"]}"#).is_err());
        assert!(with(r#"{"ports":0}"#).is_err());
        assert!(with(r#"{"ports":{"count":2,"names":{"web":2}}}"#).is_err());
        assert!(with(r#"{"setup":"run {{port}}"}"#).is_err());
        assert!(with(r#"{"ports":2,"setup":"run {{port.api}}"}"#).is_err());
        assert!(
            with(
                r#"{"ports":{"count":2,"names":{"api":1}},"setup":"run {{port.api}} {{port.1}}"}"#
            )
            .is_ok()
        );
        assert!(with(r#"{"setup":"echo {{nope}}"}"#).is_err());
        assert!(with(r#"{"setup":"echo {{slug"}"#).is_err());
        assert!(with(r#"{"branch":"feature/fixed"}"#).is_err());
        assert!(with(r#"{"branch":"{{prefix}}{{slug}}.lock"}"#).is_err());
        assert!(with(r#"{"branch":"{{port}}-{{slug}}"}"#).is_err());
        assert!(with(r#"{"branch":"wt/{{project}}-{{slug}}"}"#).is_ok());
    }

    #[test]
    fn normalized_trims_and_drops_blank_entries() {
        let env = WorktreeEnvironment {
            copy: vec![
                WorktreeCopyRule {
                    glob: "  .env  ".into(),
                    symlink: false,
                },
                WorktreeCopyRule {
                    glob: " ".into(),
                    symlink: true,
                },
            ],
            setup: vec![" make db ".into(), "".into()],
            branch: Some("  ".into()),
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(env.copy.len(), 1);
        assert_eq!(env.copy[0].glob, ".env");
        assert_eq!(env.setup, ["make db"]);
        assert_eq!(env.branch, None);
    }

    #[test]
    fn rendering_substitutes_and_reports_unknown_names() {
        let rendered = render_template(
            "createdb app_{{ slug }} -p {{port.web}}",
            |name| match name {
                "slug" => Some("swift-otter".into()),
                "port.web" => Some("4100".into()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(rendered, "createdb app_swift-otter -p 4100");
        assert!(render_template("{{missing}}", |_| None).is_err());
    }

    #[test]
    fn ports_resolve_offsets_and_names() {
        let ports = WorktreePorts {
            base: 4100,
            count: 3,
            names: BTreeMap::from([("web".into(), 0), ("api".into(), 1)]),
        };
        assert_eq!(ports.port("web"), Some(4100));
        assert_eq!(ports.port("2"), Some(4102));
        assert_eq!(ports.port("3"), None);
        assert_eq!(ports.label(), ":4100–4102");
        assert_eq!(ports.detail(), "web :4100 · api :4101");
    }
}
