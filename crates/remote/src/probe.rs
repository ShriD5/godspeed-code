//! What the probe script reports about a server: OS, arch, tools, service
//! manager, the installed Godspeed and whether its engine is running.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// From `uname -m`.
    pub fn from_uname(machine: &str) -> Option<Self> {
        match machine.trim() {
            "x86_64" | "amd64" => Some(Self::X86_64),
            "aarch64" | "arm64" | "armv8l" => Some(Self::Aarch64),
            _ => None,
        }
    }

    /// The artifact-name spelling (`uname -m` on Linux).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// How the engine is kept running on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceMode {
    /// `~/.config/systemd/user/godspeed.service`.
    Systemd,
    /// `~/.godspeed/run-headless.sh` under nohup (+ `@reboot` cron).
    Nohup,
    /// Not set up.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProbe {
    /// `uname -s` (`Linux`, `Darwin`, …).
    pub os: String,
    /// `uname -m`.
    pub machine: String,
    /// `PRETTY_NAME` from `/etc/os-release`.
    pub distro: Option<String>,
    pub user: Option<String>,
    pub hostname: Option<String>,
    /// glibc version (`getconf GNU_LIBC_VERSION`), `None` for musl or unknown.
    pub glibc: Option<String>,
    pub curl: bool,
    pub wget: bool,
    pub sha256: bool,
    /// `systemctl --user` reaches a user manager from this SSH session.
    pub systemd_user: bool,
    /// `loginctl` Linger for the user; `None` when unknown.
    pub linger: Option<bool>,
    pub service: ServiceMode,
    /// The version `~/.godspeed/app/current` points at.
    pub installed: Option<String>,
    pub running: bool,
    pub uptime_secs: Option<u64>,
    /// `~/.godspeed/session.json` exists (signed in).
    pub signed_in: bool,
}

impl ServerProbe {
    /// Parse the probe's `key=value` lines (unknown keys ignored).
    pub fn parse(stdout: &str) -> Result<Self, String> {
        let map: HashMap<&str, &str> = stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect();
        let get = |key: &str| map.get(key).copied().filter(|v| !v.is_empty());
        let yes = |key: &str| get(key) == Some("yes");
        let os = get("os")
            .ok_or("the server didn't answer the setup check (no `uname` output)")?
            .to_owned();
        Ok(Self {
            os,
            machine: get("arch").unwrap_or_default().to_owned(),
            distro: get("distro").map(str::to_owned),
            user: get("user").map(str::to_owned),
            hostname: get("hostname").map(str::to_owned),
            glibc: get("glibc").map(str::to_owned),
            curl: yes("curl"),
            wget: yes("wget"),
            sha256: yes("sha256"),
            systemd_user: yes("systemd_user"),
            linger: match get("linger") {
                Some("yes") => Some(true),
                Some("no") => Some(false),
                _ => None,
            },
            service: match get("service") {
                Some("systemd") => ServiceMode::Systemd,
                Some("nohup") => ServiceMode::Nohup,
                _ => ServiceMode::None,
            },
            installed: get("installed").map(str::to_owned),
            running: yes("running"),
            uptime_secs: get("uptime").and_then(|v| v.parse().ok()),
            signed_in: yes("session"),
        })
    }

    /// The release arch to install, or why this server can't run Godspeed.
    pub fn supported_arch(&self) -> Result<Arch, String> {
        if self.os != "Linux" {
            return Err(match self.os.as_str() {
                "Darwin" => "That's a Mac. Run on a server needs Linux; on a Mac, install the Godspeed app and sign in instead.".into(),
                other => format!("Run on a server needs Linux; this server runs {other}."),
            });
        }
        let arch = Arch::from_uname(&self.machine).ok_or_else(|| {
            format!(
                "There's no Godspeed build for {} yet (x86_64 and aarch64 only).",
                self.machine
            )
        })?;
        if self.glibc.is_none() {
            return Err(
                "This Linux doesn't use glibc (Alpine and other musl systems aren't supported yet). Ubuntu 24.04 or Debian 13 work."
                    .into(),
            );
        }
        if !self.curl && !self.wget {
            return Err("The server needs curl or wget to download Godspeed: `sudo apt-get install -y curl`.".into());
        }
        if !self.sha256 {
            return Err("The server needs sha256sum (coreutils) to check the download.".into());
        }
        Ok(arch)
    }

    /// Older glibc than the release builders (Ubuntu 24.04): the install may
    /// fail to start the binary, and the wizard says so up front.
    pub fn glibc_warning(&self) -> Option<String> {
        let glibc = self.glibc.as_deref()?;
        (!glibc_at_least(glibc, (2, 39))).then(|| {
            format!(
                "This server has glibc {glibc}. Godspeed's Linux build is made on Ubuntu 24.04 (glibc 2.39) and may not start here."
            )
        })
    }

    /// One line for the wizard: `Ubuntu 24.04.1 LTS · x86_64 · systemd`.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.distro.clone().unwrap_or_else(|| self.os.clone())];
        if !self.machine.is_empty() {
            parts.push(self.machine.clone());
        }
        parts.push(if self.systemd_user {
            "systemd user services".into()
        } else {
            "no systemd user services (nohup fallback)".into()
        });
        if let Some(version) = &self.installed {
            parts.push(format!("Godspeed {version} installed"));
        }
        parts.join(" · ")
    }
}

/// `2.39` ≥ `(2, 39)`; unparseable reads as too old.
fn glibc_at_least(version: &str, min: (u64, u64)) -> bool {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= min,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str = "os=Linux\narch=x86_64\ndistro=Ubuntu 24.04.1 LTS\nuser=deploy\nhostname=hetzner-box\nglibc=2.39\ncurl=yes\nwget=no\nsha256=yes\nsystemd_user=yes\nlinger=no\nservice=systemd\ninstalled=0.5.9\nrunning=yes\nuptime=3600\nsession=yes\n";

    #[test]
    fn parses_a_full_probe() {
        let probe = ServerProbe::parse(UBUNTU).unwrap();
        assert_eq!(probe.os, "Linux");
        assert_eq!(probe.supported_arch(), Ok(Arch::X86_64));
        assert_eq!(probe.distro.as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(probe.hostname.as_deref(), Some("hetzner-box"));
        assert!(probe.systemd_user && probe.curl && !probe.wget && probe.sha256);
        assert_eq!(probe.linger, Some(false));
        assert_eq!(probe.service, ServiceMode::Systemd);
        assert_eq!(probe.installed.as_deref(), Some("0.5.9"));
        assert!(probe.running && probe.signed_in);
        assert_eq!(probe.uptime_secs, Some(3600));
        assert_eq!(
            probe.summary(),
            "Ubuntu 24.04.1 LTS · x86_64 · systemd user services · Godspeed 0.5.9 installed"
        );
    }

    #[test]
    fn arch_detection() {
        assert_eq!(Arch::from_uname("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("amd64\n"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("aarch64"), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("armv7l"), None);
        assert_eq!(Arch::from_uname("riscv64"), None);
        let arm = ServerProbe::parse(&UBUNTU.replace("arch=x86_64", "arch=aarch64")).unwrap();
        assert_eq!(arm.supported_arch(), Ok(Arch::Aarch64));
    }

    #[test]
    fn unsupported_servers_explain_why() {
        let probe = |text: &str| {
            ServerProbe::parse(text)
                .unwrap()
                .supported_arch()
                .unwrap_err()
        };
        assert!(probe(&UBUNTU.replace("os=Linux", "os=Darwin")).contains("That's a Mac"));
        assert!(probe(&UBUNTU.replace("os=Linux", "os=FreeBSD")).contains("FreeBSD"));
        assert!(probe(&UBUNTU.replace("arch=x86_64", "arch=armv7l")).contains("armv7l"));
        assert!(probe(&UBUNTU.replace("glibc=2.39\n", "")).contains("musl"));
        let old = ServerProbe::parse(&UBUNTU.replace("glibc=2.39", "glibc=2.35")).unwrap();
        assert!(old.supported_arch().is_ok(), "old glibc only warns");
        assert!(old.glibc_warning().unwrap().contains("2.35"));
        assert_eq!(ServerProbe::parse(UBUNTU).unwrap().glibc_warning(), None);
        assert!(probe(&UBUNTU.replace("curl=yes", "curl=no")).contains("curl or wget"));
        assert!(probe(&UBUNTU.replace("sha256=yes", "sha256=no")).contains("sha256sum"));
        // wget alone is fine.
        let wget = UBUNTU
            .replace("curl=yes", "curl=no")
            .replace("wget=no", "wget=yes");
        assert!(ServerProbe::parse(&wget).unwrap().supported_arch().is_ok());
    }

    #[test]
    fn a_fresh_server_and_garbage() {
        let fresh = ServerProbe::parse("os=Linux\narch=x86_64\nglibc=2.40\ncurl=yes\nsha256=yes\nsystemd_user=no\nservice=none\ninstalled=\nrunning=no\nuptime=\nsession=no\n").unwrap();
        assert_eq!(fresh.installed, None);
        assert!(!fresh.running);
        assert_eq!(fresh.uptime_secs, None);
        assert_eq!(fresh.service, ServiceMode::None);
        assert_eq!(fresh.linger, None);
        assert!(fresh.summary().contains("nohup fallback"));
        assert!(ServerProbe::parse("Welcome to Ubuntu!\n").is_err());
        assert!(glibc_at_least("2.40", (2, 39)));
        assert!(glibc_at_least("3.0", (2, 39)));
        assert!(!glibc_at_least("2.9", (2, 39)));
        assert!(!glibc_at_least("x", (2, 39)));
    }
}
