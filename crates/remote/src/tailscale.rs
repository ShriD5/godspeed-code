//! Tailscale peers as server candidates: when the `tailscale` CLI is installed,
//! `tailscale status --json` lists the tailnet's machines; the online ones
//! that can run a server are offered in the wizard.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscalePeer {
    /// Tailnet machine name (the MagicDNS label; follows renames).
    pub name: String,
    /// Full MagicDNS name without the trailing dot, when MagicDNS is on.
    pub dns_name: Option<String>,
    /// First Tailscale IPv4 address (else the first address).
    pub ip: Option<String>,
    /// `linux`, `macOS`, `windows`, …
    pub os: String,
    pub online: bool,
}

impl TailscalePeer {
    /// What to put after `user@` in the wizard: the MagicDNS name, else the
    /// Tailscale IP, else the machine name.
    pub fn ssh_host(&self) -> String {
        self.dns_name
            .clone()
            .or_else(|| self.ip.clone())
            .unwrap_or_else(|| self.name.clone())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    #[serde(default)]
    backend_state: Option<String>,
    #[serde(default)]
    peer: Option<std::collections::BTreeMap<String, Peer>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Peer {
    #[serde(default)]
    host_name: Option<String>,
    #[serde(rename = "DNSName", default)]
    dns_name: Option<String>,
    #[serde(default)]
    tailscale_i_ps: Option<Vec<String>>,
    #[serde(rename = "OS", default)]
    os: Option<String>,
    #[serde(default)]
    online: Option<bool>,
}

/// Parse `tailscale status --json`: every peer, online first, then by name.
/// Errors when Tailscale is not connected.
pub fn parse_status(json: &str) -> Result<Vec<TailscalePeer>, String> {
    let status: Status =
        serde_json::from_str(json).map_err(|err| format!("unreadable tailscale status: {err}"))?;
    if let Some(state) = status.backend_state.as_deref()
        && state != "Running"
    {
        return Err(format!("Tailscale isn't connected ({state})"));
    }
    let mut peers: Vec<TailscalePeer> = status
        .peer
        .unwrap_or_default()
        .into_values()
        .filter_map(|peer| {
            let dns_name = peer
                .dns_name
                .map(|name| name.trim_end_matches('.').to_owned())
                .filter(|name| !name.is_empty());
            let label = dns_name
                .as_deref()
                .and_then(|name| name.split('.').next())
                .map(str::to_owned);
            let ips = peer.tailscale_i_ps.unwrap_or_default();
            let ip = ips
                .iter()
                .find(|ip| ip.contains('.'))
                .or(ips.first())
                .cloned();
            let name = label
                .or(peer.host_name.filter(|name| !name.trim().is_empty()))
                .or_else(|| ip.clone())?;
            Some(TailscalePeer {
                name,
                dns_name,
                ip,
                os: peer.os.unwrap_or_default(),
                online: peer.online.unwrap_or(false),
            })
        })
        .collect();
    peers.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
    Ok(peers)
}

/// Peers worth offering: online machines that can run the engine over SSH
/// (phones and tablets can't).
pub fn server_candidates(peers: Vec<TailscalePeer>) -> Vec<TailscalePeer> {
    peers
        .into_iter()
        .filter(|peer| peer.online)
        .filter(|peer| {
            !matches!(
                peer.os.to_ascii_lowercase().as_str(),
                "ios" | "android" | "tvos"
            )
        })
        .collect()
}

/// The `tailscale` CLI: on `PATH` (in `path_env`), else the macOS app's
/// bundled CLI. `None` when Tailscale isn't installed.
pub fn find_cli(path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let on_path = path_env
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .map(|dir| dir.join("tailscale"))
        .find(|candidate| is_executable(candidate));
    on_path.or_else(|| {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        (cfg!(target_os = "macos") && is_executable(app)).then(|| app.to_path_buf())
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Run `<cli> status --json` (bounded) and return the server candidates.
pub fn list_online_peers(cli: &Path) -> Result<Vec<TailscalePeer>, String> {
    let mut child = Command::new(cli)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("couldn't run tailscale: {err}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        text
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("tailscale status didn't answer".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(err) => return Err(err.to_string()),
        }
    };
    let text = reader.join().unwrap_or_default();
    if !status.success() && text.trim().is_empty() {
        return Err("Tailscale isn't running".into());
    }
    parse_status(&text).map(server_candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "Version": "1.80.0",
      "BackendState": "Running",
      "Self": {"HostName": "mbp", "DNSName": "mbp.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.1"], "OS": "macOS", "Online": true},
      "Peer": {
        "nodekey:a": {"HostName": "ubuntu", "DNSName": "hetzner-box.tail1234.ts.net.", "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.64.0.2"], "OS": "linux", "Online": true},
        "nodekey:b": {"HostName": "iPhone", "DNSName": "iphone.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.3"], "OS": "iOS", "Online": true},
        "nodekey:c": {"HostName": "old-vps", "DNSName": "old-vps.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.4"], "OS": "linux", "Online": false},
        "nodekey:d": {"HostName": "nas", "DNSName": "", "TailscaleIPs": ["100.64.0.5"], "OS": "linux", "Online": true}
      }
    }"#;

    #[test]
    fn parses_peers_online_first() {
        let peers = parse_status(STATUS).unwrap();
        let names: Vec<_> = peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["hetzner-box", "iphone", "nas", "old-vps"]);
        let box_ = &peers[0];
        // The DNS label wins over the OS host name (it follows renames).
        assert_eq!(
            box_.dns_name.as_deref(),
            Some("hetzner-box.tail1234.ts.net")
        );
        assert_eq!(box_.ip.as_deref(), Some("100.64.0.2"), "IPv4 preferred");
        assert_eq!(box_.ssh_host(), "hetzner-box.tail1234.ts.net");
        // Without MagicDNS the IP is the host.
        assert_eq!(peers[2].ssh_host(), "100.64.0.5");
        assert_eq!(peers[2].name, "nas");
    }

    #[test]
    fn candidates_are_online_machines_that_can_run_a_server() {
        let names: Vec<_> = server_candidates(parse_status(STATUS).unwrap())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["hetzner-box", "nas"]);
    }

    #[test]
    fn a_stopped_tailscale_is_an_error() {
        let err = parse_status(r#"{"BackendState":"Stopped","Peer":null}"#).unwrap_err();
        assert!(err.contains("Stopped"), "{err}");
        assert!(parse_status("not json").is_err());
        assert_eq!(
            parse_status(r#"{"BackendState":"Running"}"#).unwrap(),
            vec![]
        );
    }

    #[cfg(unix)]
    #[test]
    fn finds_the_cli_on_path_and_runs_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("tailscale");
        std::fs::write(&cli, format!("#!/bin/sh\ncat <<'EOF'\n{STATUS}\nEOF\n")).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = find_cli(Some(dir.path().as_os_str())).unwrap();
        assert_eq!(found, cli);
        let peers = list_online_peers(&found).unwrap();
        assert_eq!(peers.len(), 2);
        let empty = tempfile::tempdir().unwrap();
        if !cfg!(target_os = "macos") {
            assert_eq!(find_cli(Some(empty.path().as_os_str())), None);
        }
    }
}
