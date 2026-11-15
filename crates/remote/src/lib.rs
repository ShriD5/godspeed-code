//! "Run on a server" (Settings → Devices): install the headless engine on a
//! Linux machine over SSH and keep it running as a user service, so agents
//! keep working when the laptop is closed.
//!
//! The flow, driven by the desktop wizard:
//!
//! 1. [`SshTarget::parse`] the `user@host[:port]` the user typed, or pick an
//!    online Tailscale peer ([`tailscale::list_online_peers`]).
//! 2. [`SshRunner::run_script`] the [`scripts::probe_script`] with
//!    `BatchMode=yes`: the user's own ssh config, keys and agent, never a
//!    password. Failures map to [`SshError`] with a clear next step.
//! 3. [`scripts::install_script`]: download the release tarball for the
//!    detected arch from GitHub, check its sha256, unpack it into
//!    `~/.godspeed/app/<version>` and point `current` at it.
//! 4. Sign in ([`login::LoginSession`]): `godspeed login --stdin-code` on the
//!    server prints the sign-in URL; the user signs in in their browser and
//!    pastes the code, which goes to the server over the SSH channel's stdin.
//! 5. [`scripts::start_script`]: a `systemd --user` unit (with lingering) or,
//!    without systemd, a `nohup` supervisor plus a `@reboot` cron entry.
//!
//! Every script is idempotent and never restarts a running engine: running
//! the wizard again, or checking status, leaves live sessions alone.

pub mod errors;
pub mod login;
pub mod probe;
pub mod scripts;
pub mod servers;
pub mod ssh;
pub mod tailscale;
pub mod target;

pub use errors::SshError;
pub use probe::{Arch, ServerProbe, ServiceMode};
pub use ssh::SshRunner;
pub use target::{SshTarget, TargetError};

/// GitHub releases base the server downloads from
/// (`<base>/v<version>/<file>`).
pub const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/ShriD5/godspeed-code/releases/download";

/// `godspeed-<version>-linux-<arch>.tar.gz`, the artifact
/// `scripts/package-linux.sh` builds and the Linux release workflow uploads.
pub fn linux_artifact(version: &str, arch: Arch) -> String {
    format!("godspeed-{version}-linux-{}.tar.gz", arch.as_str())
}

/// Compact uptime for the server row: `40s`, `12m`, `5h 12m`, `3d 4h`.
pub fn format_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names_match_the_packaging_script() {
        assert_eq!(
            linux_artifact("0.5.9", Arch::X86_64),
            "godspeed-0.5.9-linux-x86_64.tar.gz"
        );
        assert_eq!(
            linux_artifact("0.5.9", Arch::Aarch64),
            "godspeed-0.5.9-linux-aarch64.tar.gz"
        );
    }

    #[test]
    fn uptime_is_compact() {
        assert_eq!(format_uptime(40), "40s");
        assert_eq!(format_uptime(12 * 60 + 5), "12m");
        assert_eq!(format_uptime(5 * 3600 + 12 * 60), "5h 12m");
        assert_eq!(format_uptime(3 * 86_400 + 4 * 3600 + 59), "3d 4h");
    }
}
