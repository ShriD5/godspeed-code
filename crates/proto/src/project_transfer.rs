//! Adding a project to another device: the source engine lists the files git
//! ignores in the project and reads the ones the user picks, the target engine
//! clones the repository and writes them into the new checkout. The viewport
//! relays the files between the two engines (each through its own
//! authenticated link), so the engines never talk to each other.
//!
//! Also the pure parts every client shares: clone URL protocol rewriting, the
//! files selected by default, the relay caps and the default destination.

use serde::{Deserialize, Serialize};

/// The engine serves the project transfer RPCs below.
pub const CAPABILITY: &str = "project-transfer-v1";

/// One relayed file at most; larger ones are build output, not config.
pub const MAX_RELAY_FILE_BYTES: u64 = 1024 * 1024;
/// Everything relayed for one transfer at most.
pub const MAX_RELAY_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
/// Files relayed for one transfer at most.
pub const MAX_RELAY_FILES: usize = 100;
/// Ignored files a listing offers at most; past this it is a build folder.
pub const MAX_LISTED_IGNORED_FILES: usize = 200;
/// Bytes per read or write chunk. A chunk rides one relay frame, which
/// Cloudflare caps at 1 MiB; base64 of this is 512 KiB. A multiple of 3, so
/// chunks encode independently.
pub const RELAY_CHUNK_BYTES: usize = 384 * 1024;

/// `ListIgnoredFiles` request, answered by the device that owns the space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListIgnoredFilesRequest {
    pub space_id: String,
}

/// One untracked file git ignores, relative to the project folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IgnoredFile {
    pub path: String,
    pub size: u64,
}

/// `ListIgnoredFiles` reply: what a clone elsewhere needs to know about the
/// project. Wholly ignored folders such as `node_modules` are left out, and
/// so are symlinks and anything under `.git`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IgnoredFileListing {
    #[serde(default)]
    pub files: Vec<IgnoredFile>,
    /// More ignored files exist than [`MAX_LISTED_IGNORED_FILES`].
    #[serde(default)]
    pub truncated: bool,
    /// The fetch URL of `origin` (else the first remote), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Where the project sits below its repository root, `/`-separated, or "".
    #[serde(default)]
    pub subdirectory: String,
    /// The repository folder's name (the default destination's last part).
    #[serde(default)]
    pub repository_name: String,
}

/// `ReadProjectFile` request: one chunk of a file below the project folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadProjectFileRequest {
    pub space_id: String,
    pub path: String,
    #[serde(default)]
    pub offset: u64,
}

/// `ReadProjectFile` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFileChunk {
    /// Base64 of the bytes from the request's offset.
    pub data: String,
    /// The whole file's size.
    pub size: u64,
    pub next_offset: u64,
    pub done: bool,
}

/// `WriteProjectFile` request: one chunk of a relayed file. Offset 0 starts
/// the file; `done` puts it in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteProjectFileRequest {
    pub space_id: String,
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    /// Base64 of this chunk.
    pub data: String,
    #[serde(default)]
    pub done: bool,
}

/// `WriteProjectFile` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteProjectFileOutcome {
    /// Bytes written so far.
    pub written: u64,
    /// The file is in place.
    #[serde(default)]
    pub done: bool,
}

/// `CloneProject` request, sent to the device that receives the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneProjectRequest {
    /// Client-minted; names the clone for `CancelProjectClone`.
    pub clone_id: String,
    pub url: String,
    /// Absolute, or `~/…` on the receiving device. Must be missing or empty.
    pub destination: String,
    /// Where the project sits below the repository root, or "".
    #[serde(default)]
    pub subdirectory: String,
    /// The space to register for the new checkout (client-minted).
    pub space_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Retry once over HTTPS when the SSH login is refused. Set when the
    /// user kept the source's SSH remote rather than picking SSH.
    #[serde(default)]
    pub https_fallback: bool,
}

/// `CancelProjectClone` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelProjectCloneRequest {
    pub clone_id: String,
}

/// One item of the `CloneProject` stream. The stream ends after a terminal
/// item (`done`, `failed` or `cancelled`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum CloneProgress {
    Starting,
    /// Git reports progress: `stage` is its own words ("Receiving objects").
    Cloning {
        stage: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        percent: Option<u8>,
    },
    Registering,
    #[serde(rename_all = "camelCase")]
    Done {
        /// The new checkout's root.
        checkout_root: String,
        /// The registered project folder (the root or a folder below it).
        project_path: String,
        space_id: String,
    },
    Failed {
        message: String,
    },
    Cancelled,
}

impl CloneProgress {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }
}

// ---------------------------------------------------------------------------
// Pure helpers shared by clients
// ---------------------------------------------------------------------------

/// The two ways to reach a hosted repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloneProtocol {
    Ssh,
    Https,
}

struct ParsedCloneUrl<'a> {
    protocol: CloneProtocol,
    host: &'a str,
    path: &'a str,
    custom_port: bool,
}

/// `user@` prefix of an authority, dropped (HTTPS credentials never move to
/// the other protocol).
fn strip_userinfo(authority: &str) -> &str {
    match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    }
}

fn parse_authority_url<'a>(rest: &'a str, protocol: CloneProtocol) -> Option<ParsedCloneUrl<'a>> {
    let (authority, path) = rest.split_once('/')?;
    if path.is_empty() || authority.chars().any(char::is_whitespace) {
        return None;
    }
    let host_port = strip_userinfo(authority);
    let (host, custom_port) = match host_port.split_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, true)
        }
        Some(_) => return None,
        None => (host_port, false),
    };
    if host.is_empty() {
        return None;
    }
    Some(ParsedCloneUrl {
        protocol,
        host,
        path,
        custom_port,
    })
}

fn parse_clone_url(url: &str) -> Option<ParsedCloneUrl<'_>> {
    let url = url.trim();
    if url.chars().any(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        return parse_authority_url(rest, CloneProtocol::Ssh);
    }
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        return parse_authority_url(rest, CloneProtocol::Https);
    }
    if url.contains("://") {
        return None; // file://, git://: nothing to switch between
    }
    // scp-like `[user@]host:path` — but not `C:\…` or `/abs/path`.
    let (authority, path) = url.split_once(':')?;
    if authority.is_empty()
        || authority.contains('/')
        || path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
    {
        return None;
    }
    let host = strip_userinfo(authority);
    if host.is_empty() || host.len() == 1 {
        return None; // a drive letter
    }
    Some(ParsedCloneUrl {
        protocol: CloneProtocol::Ssh,
        host,
        path,
        custom_port: false,
    })
}

/// The protocol a clone URL uses, `None` for local paths and other schemes.
pub fn clone_url_protocol(url: &str) -> Option<CloneProtocol> {
    parse_clone_url(url).map(|parsed| parsed.protocol)
}

/// Rewrites a clone URL to `protocol`, because the receiving device may
/// only have SSH keys or only an HTTPS credential helper. `None` when the
/// URL can't be rewritten without guessing, such as a custom port.
pub fn clone_url_for_protocol(url: &str, protocol: CloneProtocol) -> Option<String> {
    let parsed = parse_clone_url(url)?;
    if parsed.protocol == protocol {
        return Some(url.trim().to_string());
    }
    if parsed.custom_port {
        return None;
    }
    Some(match protocol {
        CloneProtocol::Ssh => format!("git@{}:{}", parsed.host, parsed.path),
        CloneProtocol::Https => format!("https://{}/{}", parsed.host, parsed.path),
    })
}

/// Env files are the ignored files people almost always want on another
/// device: `.env`, `.env.*`, `.envrc` and `.dev.vars`, in any folder.
pub fn is_default_copied_file(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name == ".env"
        || name == ".envrc"
        || name == ".dev.vars"
        || name
            .strip_prefix(".env.")
            .is_some_and(|rest| !rest.is_empty())
}

/// The folder a repository's clone defaults to on another device: the same
/// place below the home folder as on the source (`/Users/me/code/app` →
/// `~/code/app`), else `~/<repository name>`. The receiving engine expands `~`.
pub fn default_destination(source_checkout_root: &str, repository_name: &str) -> String {
    let normalized = source_checkout_root.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    let below_home = ["/Users/", "/home/"]
        .iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
        .or_else(|| {
            // C:/Users/name/…
            let lower = trimmed.get(1..)?.to_ascii_lowercase();
            lower
                .starts_with(":/users/")
                .then(|| &trimmed[":/users/".len() + 1..])
        })
        .and_then(|rest| rest.split_once('/'))
        .map(|(_user, rest)| rest)
        .filter(|rest| !rest.is_empty());
    match below_home {
        Some(rest) => format!("~/{rest}"),
        None => {
            let name = if repository_name.trim().is_empty() {
                trimmed.rsplit('/').next().unwrap_or("project")
            } else {
                repository_name.trim()
            };
            format!("~/{name}")
        }
    }
}

/// The checkout root of a project at `project_path` that sits `subdirectory`
/// below it. Falls back to the project path when they don't line up.
pub fn checkout_root_of(project_path: &str, subdirectory: &str) -> String {
    let sub = subdirectory.trim_matches('/');
    if sub.is_empty() {
        return project_path.to_string();
    }
    let normalized = project_path.trim_end_matches(['/', '\\']);
    let unix = normalized.replace('\\', "/");
    match unix.strip_suffix(sub) {
        Some(root) if root.ends_with('/') => normalized[..root.len() - 1].to_string(),
        _ => project_path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_between_scp_style_ssh_and_https() {
        assert_eq!(
            clone_url_for_protocol("git@github.com:pingdotgg/t3code.git", CloneProtocol::Https)
                .as_deref(),
            Some("https://github.com/pingdotgg/t3code.git")
        );
        assert_eq!(
            clone_url_for_protocol(
                "https://github.com/pingdotgg/t3code.git",
                CloneProtocol::Ssh
            )
            .as_deref(),
            Some("git@github.com:pingdotgg/t3code.git")
        );
        assert_eq!(
            clone_url_for_protocol(
                "ssh://git@gitlab.com/group/sub/repo.git",
                CloneProtocol::Https
            )
            .as_deref(),
            Some("https://gitlab.com/group/sub/repo.git")
        );
    }

    #[test]
    fn keeps_its_own_protocol_and_drops_https_credentials_on_switch() {
        assert_eq!(
            clone_url_for_protocol("https://token@github.com/a/b.git", CloneProtocol::Https)
                .as_deref(),
            Some("https://token@github.com/a/b.git")
        );
        assert_eq!(
            clone_url_for_protocol("https://token@github.com/a/b.git", CloneProtocol::Ssh)
                .as_deref(),
            Some("git@github.com:a/b.git")
        );
    }

    #[test]
    fn refuses_rewrites_that_would_guess_wrong() {
        assert_eq!(
            clone_url_for_protocol("ssh://git@host.example:2222/a/b.git", CloneProtocol::Https),
            None
        );
        assert_eq!(
            clone_url_for_protocol("https://host.example:8443/a/b.git", CloneProtocol::Ssh),
            None
        );
        assert_eq!(
            clone_url_for_protocol("https://host.example:8443/a/b.git", CloneProtocol::Https)
                .as_deref(),
            Some("https://host.example:8443/a/b.git")
        );
        assert_eq!(
            clone_url_for_protocol("/srv/git/repo.git", CloneProtocol::Https),
            None
        );
        assert_eq!(clone_url_protocol("file:///srv/git/repo.git"), None);
        assert_eq!(clone_url_protocol("C:\\repos\\app"), None);
        assert_eq!(clone_url_protocol("C:/repos/app"), None);
        assert_eq!(
            clone_url_protocol("git@github.com:a/b.git"),
            Some(CloneProtocol::Ssh)
        );
        assert_eq!(
            clone_url_protocol("http://example.com/a/b.git"),
            Some(CloneProtocol::Https)
        );
    }

    #[test]
    fn selects_env_files_anywhere_in_the_tree() {
        let picked: Vec<&str> = [
            ".env",
            "apps/web/.env.local",
            ".envrc",
            ".dev.vars",
            "dist.log",
            ".environment",
            ".env.",
            "worker/.dev.vars",
        ]
        .into_iter()
        .filter(|path| is_default_copied_file(path))
        .collect();
        assert_eq!(
            picked,
            [
                ".env",
                "apps/web/.env.local",
                ".envrc",
                ".dev.vars",
                "worker/.dev.vars"
            ]
        );
    }

    #[test]
    fn destination_mirrors_the_place_below_home() {
        assert_eq!(
            default_destination("/Users/me/Developer/godspeed", "godspeed"),
            "~/Developer/godspeed"
        );
        assert_eq!(default_destination("/home/me/app/", "app"), "~/app");
        assert_eq!(
            default_destination("C:\\Users\\me\\src\\app", "app"),
            "~/src/app"
        );
        assert_eq!(default_destination("/srv/checkouts/app", "app"), "~/app");
        assert_eq!(default_destination("/Users/me", "repo"), "~/repo");
        assert_eq!(default_destination("/opt/thing", ""), "~/thing");
    }

    #[test]
    fn checkout_root_strips_the_subdirectory() {
        assert_eq!(checkout_root_of("/r/mono/apps/web", "apps/web"), "/r/mono");
        assert_eq!(checkout_root_of("/r/mono", ""), "/r/mono");
        assert_eq!(checkout_root_of("/r/xapps/web", "apps/web"), "/r/xapps/web");
    }

    #[test]
    fn clone_progress_is_tagged_by_phase() {
        let done = CloneProgress::Done {
            checkout_root: "/a".into(),
            project_path: "/a/b".into(),
            space_id: "s".into(),
        };
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            serde_json::json!({
                "phase": "done", "checkoutRoot": "/a", "projectPath": "/a/b", "spaceId": "s"
            })
        );
        assert!(done.is_terminal());
        let cloning: CloneProgress = serde_json::from_value(
            serde_json::json!({ "phase": "cloning", "stage": "Receiving objects", "percent": 40 }),
        )
        .unwrap();
        assert!(!cloning.is_terminal());
        assert_eq!(
            serde_json::from_value::<CloneProgress>(serde_json::json!({ "phase": "cancelled" }))
                .unwrap(),
            CloneProgress::Cancelled
        );
    }
}
