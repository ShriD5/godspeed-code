//! The shell scripts run on the server (POSIX `sh`, through
//! [`crate::SshRunner::run_script`]). Values are baked in with
//! [`shell_quote`]; every script is idempotent, and none of them restarts an
//! engine that is already running: re-running the wizard or checking status
//! never interrupts a live session.
//!
//! Scripts report through `godspeed-<step>: …` lines on stdout and
//! `godspeed-<step>: error <code>` on stderr with a distinct exit code, which
//! [`install_failure_message`] turns into guidance.

use crate::errors::SshError;
use crate::probe::Arch;

/// Quote `value` for POSIX sh: bare when it is only safe characters, single
/// quotes otherwise (with `'` written as `'\''`).
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c));
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Server facts for the wizard and the status row; parsed by
/// [`crate::ServerProbe::parse`]. Read-only.
pub fn probe_script() -> String {
    PROBE.to_owned()
}

const PROBE: &str = r#"has() { command -v "$1" >/dev/null 2>&1; }
echo "os=$(uname -s 2>/dev/null)"
echo "arch=$(uname -m 2>/dev/null)"
if [ -r /etc/os-release ]; then echo "distro=$(. /etc/os-release 2>/dev/null; printf '%s' "${PRETTY_NAME:-}")"; fi
echo "user=$(id -un 2>/dev/null)"
echo "hostname=$(hostname 2>/dev/null || uname -n)"
echo "glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | sed -n 's/^glibc //p')"
has curl && echo curl=yes || echo curl=no
has wget && echo wget=yes || echo wget=no
if has sha256sum || has shasum; then echo sha256=yes; else echo sha256=no; fi
su=0
if has systemctl && systemctl --user show-environment >/dev/null 2>&1; then su=1; fi
[ "$su" = 1 ] && echo systemd_user=yes || echo systemd_user=no
if has loginctl; then echo "linger=$(loginctl show-user "$(id -un)" -p Linger 2>/dev/null | sed -n 's/^Linger=//p')"; fi
G="$HOME/.godspeed"
cur="$(readlink "$G/app/current" 2>/dev/null || true)"
echo "installed=${cur##*/}"
unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/godspeed.service"
pid=""
if [ -f "$unit" ]; then
  echo service=systemd
  if [ "$su" = 1 ] && systemctl --user is-active --quiet godspeed.service; then
    pid="$(systemctl --user show -p MainPID --value godspeed.service 2>/dev/null)"
  fi
elif [ -f "$G/run-headless.sh" ]; then
  echo service=nohup
  p="$(cat "$G/headless.pid" 2>/dev/null || true)"
  if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then pid="$p"; fi
else
  echo service=none
fi
if [ -n "$pid" ] && [ "$pid" != 0 ]; then
  echo running=yes
  echo "uptime=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ')"
else
  echo running=no
fi
[ -f "$G/session.json" ] && echo session=yes || echo session=no
"#;

/// What to install: the release tarball for one arch, and its checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub version: String,
    pub arch: Arch,
    /// `https://github.com/…/releases/download/v<version>/<file>`.
    pub url: String,
    /// From the Linux release manifest; when `None` the server reads the
    /// release's `<file>.sha256` instead. Either way a mismatch aborts.
    pub sha256: Option<String>,
}

impl InstallPlan {
    /// The GitHub release asset for `version` and `arch`.
    pub fn github(version: &str, arch: Arch, sha256: Option<String>) -> Self {
        let file = crate::linux_artifact(version, arch);
        Self {
            version: version.to_owned(),
            arch,
            url: format!("{}/v{version}/{file}", crate::RELEASE_DOWNLOAD_BASE),
            sha256,
        }
    }

    pub fn file(&self) -> String {
        crate::linux_artifact(&self.version, self.arch)
    }
}

/// Download, verify and unpack into `~/.godspeed/app/<version>`, then point
/// `current` at it and link `~/.local/bin/godspeed`. An already-installed
/// version is reused. A running engine keeps running on its old binary; the
/// engine's own updater restarts it into the new one once it is idle.
pub fn install_script(plan: &InstallPlan) -> String {
    format!(
        r#"set -eu
VERSION={version}
FILE={file}
URL={url}
EXPECTED={sha}
APP_ROOT="$HOME/.godspeed/app"
DEST="$APP_ROOT/$VERSION"
fail() {{ echo "godspeed-install: error $2" >&2; exit "$1"; }}
mkdir -p "$APP_ROOT"
if [ -x "$DEST/godspeed" ] && "$DEST/godspeed" --version >/dev/null 2>&1; then
  echo "godspeed-install: already-installed $VERSION"
else
  fetch() {{
    if command -v curl >/dev/null 2>&1; then curl -fsSL --connect-timeout 30 --max-time "$3" "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then wget -q --timeout=30 --tries=2 "$1" -O "$2"
    else fail 3 no-downloader
    fi
  }}
  STAGE="$(mktemp -d "$APP_ROOT/.stage-$VERSION-XXXXXX")"
  trap 'rm -rf "$STAGE"' EXIT
  if [ -z "$EXPECTED" ]; then
    fetch "$URL.sha256" "$STAGE/sum" 60 || fail 7 "download-failed $URL.sha256"
    EXPECTED="$(cut -d' ' -f1 < "$STAGE/sum")"
  fi
  fetch "$URL" "$STAGE/$FILE" 1800 || fail 7 "download-failed $URL"
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "$STAGE/$FILE" | cut -d' ' -f1)"
  else
    ACTUAL="$(shasum -a 256 "$STAGE/$FILE" | cut -d' ' -f1)"
  fi
  [ -n "$EXPECTED" ] && [ "$ACTUAL" = "$EXPECTED" ] || fail 4 checksum-mismatch
  mkdir "$STAGE/unpacked"
  tar -xzf "$STAGE/$FILE" -C "$STAGE/unpacked" --strip-components=1
  if ! "$STAGE/unpacked/godspeed" --version >/dev/null 2>"$STAGE/err"; then
    cat "$STAGE/err" >&2
    fail 5 does-not-run
  fi
  rm -rf "$DEST"
  mv "$STAGE/unpacked" "$DEST"
  echo "godspeed-install: installed $VERSION"
fi
ln -sfn "$DEST" "$APP_ROOT/.current.$$"
mv -Tf "$APP_ROOT/.current.$$" "$APP_ROOT/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$APP_ROOT/current/godspeed" "$HOME/.local/bin/godspeed"
echo "godspeed-install: current $VERSION"
"#,
        version = shell_quote(&plan.version),
        file = shell_quote(&plan.file()),
        url = shell_quote(&plan.url),
        sha = shell_quote(plan.sha256.as_deref().unwrap_or("")),
    )
}

/// The `systemd --user` unit: the same name and shape as `godspeed daemon
/// install` and the curl installer write, so `godspeed daemon …` and the
/// engine's updater manage it. Runs through the `current` symlink so updates
/// never edit the unit.
pub fn systemd_unit() -> String {
    SYSTEMD_UNIT.to_owned()
}

const SYSTEMD_UNIT: &str = "[Unit]
Description=Godspeed headless engine
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Environment=\"PATH=%h/.local/bin:%h/.npm-global/bin:%h/.bun/bin:%h/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"
ExecStart=%h/.godspeed/app/current/godspeed headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.godspeed/env

[Install]
WantedBy=default.target
";

/// The fallback supervisor when the server has no user systemd: restarts the
/// engine if it exits and records its own pid.
const NOHUP_RUNNER: &str = r#"#!/bin/sh
# Keeps `godspeed headless` running (no systemd user services here).
# Written by Godspeed's "Run on a server"; Stop / Uninstall remove it.
echo $$ > "$HOME/.godspeed/headless.pid"
while :; do
  "$HOME/.godspeed/app/current/godspeed" headless >>"$HOME/.godspeed/headless.out" 2>&1
  sleep 5
done
"#;

/// Start the engine as a user service. Already running: nothing restarts.
/// With systemd it also tries to enable lingering (keeps the engine running
/// after logout and starts it at boot); without, a `nohup` supervisor plus an
/// `@reboot` cron entry.
pub fn start_script() -> String {
    format!(
        r#"set -eu
G="$HOME/.godspeed"
[ -x "$G/app/current/godspeed" ] || {{ echo "godspeed-start: error not-installed" >&2; exit 6; }}
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  UNIT_DIR="${{XDG_CONFIG_HOME:-$HOME/.config}}/systemd/user"
  mkdir -p "$UNIT_DIR"
  NEXT="$UNIT_DIR/.godspeed.service.$$"
  cat > "$NEXT" <<'GODSPEED_UNIT'
{unit}GODSPEED_UNIT
  if [ -f "$UNIT_DIR/godspeed.service" ] && cmp -s "$NEXT" "$UNIT_DIR/godspeed.service"; then
    rm -f "$NEXT"
  else
    mv -f "$NEXT" "$UNIT_DIR/godspeed.service"
    systemctl --user daemon-reload
  fi
  systemctl --user enable godspeed.service >/dev/null 2>&1 || true
  if systemctl --user is-active --quiet godspeed.service; then
    echo "godspeed-start: already-running systemd"
  else
    systemctl --user start godspeed.service
    echo "godspeed-start: started systemd"
  fi
  if loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q '=yes'; then
    echo "godspeed-start: linger yes"
  elif loginctl enable-linger "$(id -un)" >/dev/null 2>&1; then
    echo "godspeed-start: linger enabled"
  else
    echo "godspeed-start: linger no"
  fi
else
  RUNNER="$G/run-headless.sh"
  cat > "$RUNNER.next" <<'GODSPEED_RUNNER'
{runner}GODSPEED_RUNNER
  chmod 700 "$RUNNER.next"
  mv -f "$RUNNER.next" "$RUNNER"
  sup="$(cat "$G/headless.pid" 2>/dev/null || true)"
  if [ -n "$sup" ] && kill -0 "$sup" 2>/dev/null; then
    echo "godspeed-start: already-running nohup"
  else
    if command -v setsid >/dev/null 2>&1; then
      nohup setsid "$RUNNER" >/dev/null 2>&1 < /dev/null &
    else
      nohup "$RUNNER" >/dev/null 2>&1 < /dev/null &
    fi
    echo "godspeed-start: started nohup"
  fi
  if command -v crontab >/dev/null 2>&1; then
    {{ crontab -l 2>/dev/null | grep -v '# godspeed-headless' || true; echo "@reboot $RUNNER >/dev/null 2>&1 # godspeed-headless"; }} | crontab -
    echo "godspeed-start: reboot cron"
  else
    echo "godspeed-start: reboot none"
  fi
fi
"#,
        unit = SYSTEMD_UNIT,
        runner = NOHUP_RUNNER,
    )
}

/// Stop the engine and keep it stopped across reboots (Start undoes this).
pub fn stop_script() -> String {
    format!("set -u\n{STOP_BODY}")
}

const STOP_BODY: &str = r#"G="$HOME/.godspeed"
UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/godspeed.service"
if [ -f "$UNIT" ] && command -v systemctl >/dev/null 2>&1; then
  systemctl --user disable --now godspeed.service >/dev/null 2>&1 || systemctl --user stop godspeed.service >/dev/null 2>&1 || true
  echo "godspeed-stop: stopped systemd"
fi
if [ -f "$G/headless.pid" ]; then
  sup="$(cat "$G/headless.pid" 2>/dev/null || true)"
  if [ -n "$sup" ] && kill -0 "$sup" 2>/dev/null; then
    # Freeze the supervisor so it can't restart the engine, stop the engine,
    # then end the supervisor.
    kill -STOP "$sup" 2>/dev/null || true
    pkill -TERM -P "$sup" 2>/dev/null || true
    kill -TERM "$sup" 2>/dev/null || true
    kill -CONT "$sup" 2>/dev/null || true
  fi
  rm -f "$G/headless.pid"
  echo "godspeed-stop: stopped nohup"
fi
if command -v crontab >/dev/null 2>&1 && crontab -l 2>/dev/null | grep -q '# godspeed-headless'; then
  crontab -l 2>/dev/null | grep -v '# godspeed-headless' | crontab - || true
fi
"#;

/// Update to the newest release with the server's own `godspeed update`
/// (downloads and verifies against the Linux release manifest, swaps
/// `current`, restarts the service). This restarts the engine: the wizard
/// warns first. The automatic updater, by contrast, waits until it is idle.
pub fn update_script() -> String {
    r#"set -u
G="$HOME/.godspeed"
BIN="$G/app/current/godspeed"
[ -x "$BIN" ] || { echo "godspeed-update: error not-installed" >&2; exit 6; }
out="$("$BIN" update 2>&1)"
code=$?
printf '%s\n' "$out"
[ "$code" -eq 0 ] || exit "$code"
UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/godspeed.service"
case "$out" in
  *"installed "*)
    if [ ! -f "$UNIT" ] && [ -f "$G/headless.pid" ]; then
      # No systemd: the supervisor starts the new version when the engine exits.
      pkill -TERM -P "$(cat "$G/headless.pid")" 2>/dev/null || true
      echo "godspeed-update: restarted nohup"
    fi
    ;;
esac
"#
    .to_owned()
}

/// Stop, remove the service and the installed binaries. Chats, settings and
/// the sign-in in `~/.godspeed` stay (the wizard says how to remove them).
pub fn uninstall_script() -> String {
    format!(
        r#"set -u
{STOP_BODY}if [ -f "$UNIT" ]; then
  rm -f "$UNIT"
  systemctl --user daemon-reload >/dev/null 2>&1 || true
fi
rm -f "$G/run-headless.sh" "$G/headless.pid"
rm -rf "$G/app"
if [ -L "$HOME/.local/bin/godspeed" ]; then
  case "$(readlink "$HOME/.local/bin/godspeed")" in
    "$G"/*) rm -f "$HOME/.local/bin/godspeed" ;;
  esac
fi
echo "godspeed-uninstall: done"
"#
    )
}

/// What [`start_script`] reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOutcome {
    pub systemd: bool,
    pub already_running: bool,
    /// `Some(true)` lingering on (or just enabled), `Some(false)` couldn't
    /// enable it (needs `sudo loginctl enable-linger`), `None` not systemd.
    pub linger: Option<bool>,
    /// nohup mode: whether an `@reboot` cron entry was written.
    pub reboot_cron: Option<bool>,
}

impl StartOutcome {
    pub fn parse(stdout: &str) -> Option<Self> {
        let mut outcome = None::<Self>;
        let mut linger = None;
        let mut cron = None;
        for line in stdout.lines() {
            let Some(rest) = line.trim().strip_prefix("godspeed-start: ") else {
                continue;
            };
            match rest {
                "started systemd"
                | "already-running systemd"
                | "started nohup"
                | "already-running nohup" => {
                    outcome = Some(Self {
                        systemd: rest.ends_with("systemd"),
                        already_running: rest.starts_with("already"),
                        linger: None,
                        reboot_cron: None,
                    });
                }
                "linger yes" | "linger enabled" => linger = Some(true),
                "linger no" => linger = Some(false),
                "reboot cron" => cron = Some(true),
                "reboot none" => cron = Some(false),
                _ => {}
            }
        }
        outcome.map(|o| Self {
            linger,
            reboot_cron: cron,
            ..o
        })
    }

    /// What the user still has to do on the server, if anything.
    pub fn follow_up(&self, user: &str) -> Option<String> {
        match (self.systemd, self.linger, self.reboot_cron) {
            (true, Some(false), _) => Some(format!(
                "To keep the engine running after you log out and start it at boot, run `sudo loginctl enable-linger {user}` on the server once."
            )),
            (false, _, Some(false)) => Some(
                "This server has no cron, so the engine won't start again after a reboot. Open Settings → Devices and press Start after rebooting it."
                    .into(),
            ),
            _ => None,
        }
    }
}

/// Guidance for a failed install, start or update.
pub fn install_failure_message(err: &SshError, target: &str) -> String {
    let SshError::Remote { code, stderr } = err else {
        return err.user_message(target);
    };
    match code {
        Some(3) => "The server needs curl or wget: run `sudo apt-get install -y curl` there.".into(),
        Some(4) => "The download didn't match its published checksum, so nothing was installed. Try again; if it keeps failing, the release may be broken.".into(),
        Some(5) if stderr.contains("libasound") => "Godspeed needs the ALSA library on the server: run `sudo apt-get install -y libasound2t64` (Ubuntu 24.04; `libasound2` on older systems), then try again.".into(),
        Some(5) if stderr.contains("GLIBC_") => "This server's glibc is too old for Godspeed's Linux build. Use Ubuntu 24.04 or newer.".into(),
        Some(5) => format!(
            "The downloaded Godspeed didn't start on the server: {}",
            crate::errors::last_lines(stderr, 2)
        ),
        Some(6) => "Godspeed isn't installed on that server yet.".into(),
        Some(7) => "The server couldn't download the release from GitHub. Check it can reach github.com, then try again.".into(),
        _ => err.user_message(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("0.5.9"), "0.5.9");
        assert_eq!(
            shell_quote("https://github.com/a/b/releases/download/v1/x.tar.gz"),
            "https://github.com/a/b/releases/download/v1/x.tar.gz"
        );
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(shell_quote("x;y"), "'x;y'");
    }

    #[cfg(unix)]
    #[test]
    fn quoted_values_survive_a_real_shell() {
        for value in [
            "",
            "a b",
            "it's",
            "$(touch /tmp/never)",
            "`id`",
            "a\nb",
            "\\",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", shell_quote(value)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), value);
        }
    }

    #[test]
    fn install_script_bakes_in_quoted_values() {
        let plan = InstallPlan::github("0.5.9", Arch::Aarch64, Some("ab12".into()));
        assert_eq!(
            plan.url,
            "https://github.com/ShriD5/godspeed-code/releases/download/v0.5.9/godspeed-0.5.9-linux-aarch64.tar.gz"
        );
        let script = install_script(&plan);
        assert!(script.starts_with("set -eu\nVERSION=0.5.9\n"));
        assert!(script.contains("\nFILE=godspeed-0.5.9-linux-aarch64.tar.gz\n"));
        assert!(script.contains(&format!("\nURL={}\n", plan.url)));
        assert!(script.contains("\nEXPECTED=ab12\n"));
        assert!(script.contains("fail 4 checksum-mismatch"));
        // Without a manifest checksum the server reads the .sha256 asset.
        let script = install_script(&InstallPlan::github("0.5.9", Arch::X86_64, None));
        assert!(script.contains("\nEXPECTED=''\n"));
        assert!(script.contains(r#"fetch "$URL.sha256""#));
        // Hostile values stay data.
        let hostile = InstallPlan {
            version: "1'; rm -rf ~; '".into(),
            arch: Arch::X86_64,
            url: "https://x/$(id)".into(),
            sha256: Some("`id`".into()),
        };
        let script = install_script(&hostile);
        assert!(script.contains(r"VERSION='1'\''; rm -rf ~; '\'''"));
        assert!(script.contains("URL='https://x/$(id)'"));
        assert!(script.contains("EXPECTED='`id`'"));
    }

    #[cfg(unix)]
    #[test]
    fn scripts_are_valid_sh() {
        let plan = InstallPlan::github("0.5.9", Arch::X86_64, None);
        for script in [
            probe_script(),
            install_script(&plan),
            start_script(),
            stop_script(),
            update_script(),
            uninstall_script(),
        ] {
            let out = std::process::Command::new("sh")
                .arg("-n")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child.stdin.take().unwrap().write_all(script.as_bytes())?;
                    child.wait_with_output()
                })
                .unwrap();
            assert!(
                out.status.success(),
                "sh -n failed: {}\n{script}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The install script against a local "release": a fake tarball served
    /// from a file:// URL through a curl shim, under a throwaway HOME.
    #[cfg(target_os = "linux")]
    #[test]
    fn install_script_installs_verifies_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let pkg = tmp.path().join("pkg/godspeed-9.9.9-linux-x86_64");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("godspeed"), "#!/bin/sh\necho 'godspeed 9.9.9'\n").unwrap();
        std::fs::set_permissions(pkg.join("godspeed"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let tarball = tmp.path().join("release.tar.gz");
        assert!(
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&tarball)
                .arg("-C")
                .arg(tmp.path().join("pkg"))
                .arg("godspeed-9.9.9-linux-x86_64")
                .status()
                .unwrap()
                .success()
        );
        let sum = String::from_utf8(
            std::process::Command::new("sha256sum")
                .arg(&tarball)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let sum = sum.split_whitespace().next().unwrap().to_owned();
        let run = |plan: &InstallPlan| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(install_script(plan))
                .env("HOME", &home)
                .output()
                .unwrap()
        };
        let plan = InstallPlan {
            version: "9.9.9".into(),
            arch: Arch::X86_64,
            url: format!("file://{}", tarball.display()),
            sha256: Some(sum.clone()),
        };
        // A wrong checksum installs nothing.
        let bad = run(&InstallPlan {
            sha256: Some("00".into()),
            ..plan.clone()
        });
        assert_eq!(bad.status.code(), Some(4));
        assert!(!home.join(".godspeed/app/9.9.9").exists());
        let good = run(&plan);
        assert!(
            good.status.success(),
            "{}",
            String::from_utf8_lossy(&good.stderr)
        );
        assert!(String::from_utf8_lossy(&good.stdout).contains("installed 9.9.9"));
        assert_eq!(
            std::fs::read_link(home.join(".godspeed/app/current")).unwrap(),
            home.join(".godspeed/app/9.9.9")
        );
        let again = run(&plan);
        assert!(String::from_utf8_lossy(&again.stdout).contains("already-installed 9.9.9"));
    }

    #[test]
    fn systemd_unit_runs_the_current_symlink_headless() {
        let unit = systemd_unit();
        assert!(unit.contains("\nExecStart=%h/.godspeed/app/current/godspeed headless\n"));
        assert!(unit.contains("\nRestart=on-failure\n"));
        assert!(unit.contains("\nEnvironmentFile=-%h/.godspeed/env\n"));
        assert!(unit.contains("\nWantedBy=default.target\n"));
        assert!(unit.contains("Environment=\"PATH=%h/.local/bin:"));
        assert!(unit.ends_with('\n'));
        // The start script writes exactly this unit.
        assert!(start_script().contains(&unit));
    }

    #[test]
    fn starting_never_restarts_a_running_engine() {
        let script = start_script();
        assert!(
            !script.contains("restart"),
            "start must not restart: {script}"
        );
        assert!(script.contains("is-active --quiet godspeed.service"));
        assert!(script.contains("already-running nohup"));
        // Unit rewrites are content-checked, so an unchanged unit doesn't even reload.
        assert!(script.contains("cmp -s"));
        // Status checks are read-only.
        for verb in ["start", "stop", "restart", "kill", "rm "] {
            assert!(
                !probe_script().contains(&format!("systemctl --user {verb}")),
                "probe must not {verb}"
            );
        }
        assert!(!probe_script().contains("kill -TERM"));
    }

    #[test]
    fn start_outcomes() {
        let outcome =
            StartOutcome::parse("godspeed-start: started systemd\ngodspeed-start: linger no\n")
                .unwrap();
        assert!(outcome.systemd && !outcome.already_running);
        assert_eq!(outcome.linger, Some(false));
        assert!(
            outcome
                .follow_up("deploy")
                .unwrap()
                .contains("sudo loginctl enable-linger deploy")
        );
        let running = StartOutcome::parse(
            "godspeed-start: already-running systemd\ngodspeed-start: linger enabled\n",
        )
        .unwrap();
        assert!(running.already_running);
        assert_eq!(running.follow_up("deploy"), None);
        let nohup =
            StartOutcome::parse("godspeed-start: started nohup\ngodspeed-start: reboot none\n")
                .unwrap();
        assert!(!nohup.systemd);
        assert!(nohup.follow_up("x").unwrap().contains("no cron"));
        assert_eq!(StartOutcome::parse("nothing"), None);
    }

    #[test]
    fn uninstall_keeps_data_and_only_removes_our_symlink() {
        let script = uninstall_script();
        assert!(script.contains(r#"rm -rf "$G/app""#));
        assert!(!script.contains("rm -rf \"$G\"\n"));
        assert!(!script.contains("rm -rf \"$HOME/.godspeed\""));
        assert!(!script.contains("session.json"));
        assert!(script.contains(r#""$G"/*) rm -f "$HOME/.local/bin/godspeed""#));
    }

    #[test]
    fn install_failures_map_to_guidance() {
        let remote = |code, stderr: &str| SshError::Remote {
            code: Some(code),
            stderr: stderr.into(),
        };
        let msg = |err| install_failure_message(&err, "root@box");
        assert!(msg(remote(3, "")).contains("curl or wget"));
        assert!(msg(remote(4, "")).contains("checksum"));
        assert!(
            msg(remote(
                5,
                "godspeed: error while loading shared libraries: libasound.so.2: cannot open shared object file"
            ))
            .contains("libasound2t64")
        );
        assert!(msg(remote(5, "version `GLIBC_2.39' not found")).contains("glibc"));
        assert!(msg(remote(7, "")).contains("github.com"));
        assert!(msg(SshError::AuthFailed).contains("SSH keys"));
    }
}
