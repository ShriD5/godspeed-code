//! The servers this computer set up (`{data_dir}/servers.json`), so Settings →
//! Devices can show their status and offer Update / Stop / Uninstall later.
//! Only addresses and names: no keys, passwords or tokens.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedServer {
    /// Canonical `user@host[:port]` ([`crate::SshTarget`]'s `Display`).
    pub target: String,
    /// The server's host name from the probe (its device name once it syncs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Version installed by the last setup or update from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub added_at_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    servers: Vec<SavedServer>,
}

pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("servers.json")
}

/// Saved servers, oldest first. A missing or unreadable file is an empty list.
pub fn load(data_dir: &Path) -> Vec<SavedServer> {
    std::fs::read(store_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
        .map(|file| file.servers)
        .unwrap_or_default()
}

/// Write atomically (temp file + rename).
pub fn save(data_dir: &Path, servers: &[SavedServer]) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = store_path(data_dir);
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    let body = serde_json::to_vec_pretty(&File {
        servers: servers.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// Insert or update by target (keeps the original `added_at_ms`).
pub fn upsert(servers: &mut Vec<SavedServer>, server: SavedServer) {
    match servers.iter_mut().find(|s| s.target == server.target) {
        Some(existing) => {
            existing.hostname = server.hostname.or(existing.hostname.take());
            existing.version = server.version.or(existing.version.take());
        }
        None => servers.push(server),
    }
}

pub fn remove(servers: &mut Vec<SavedServer>, target: &str) {
    servers.retain(|s| s.target != target);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(target: &str, version: Option<&str>) -> SavedServer {
        SavedServer {
            target: target.into(),
            hostname: None,
            version: version.map(str::to_owned),
            added_at_ms: 1,
        }
    }

    #[test]
    fn round_trips_and_tolerates_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
        let mut servers = vec![];
        upsert(&mut servers, server("root@a", Some("0.5.9")));
        upsert(
            &mut servers,
            SavedServer {
                hostname: Some("box".into()),
                added_at_ms: 99,
                ..server("root@a", None)
            },
        );
        upsert(&mut servers, server("me@b:2222", None));
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].version.as_deref(), Some("0.5.9"));
        assert_eq!(servers[0].hostname.as_deref(), Some("box"));
        assert_eq!(servers[0].added_at_ms, 1);
        save(dir.path(), &servers).unwrap();
        assert_eq!(load(dir.path()), servers);
        remove(&mut servers, "root@a");
        assert_eq!(servers.len(), 1);
        std::fs::write(store_path(dir.path()), b"garbage").unwrap();
        assert!(load(dir.path()).is_empty());
    }
}
