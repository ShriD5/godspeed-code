//! Running a worktree's own commands.
//!
//! Setup runs visibly in the worktree's "(setup)" terminal as a POSIX script
//! that stops at the first failing step, says which one in the terminal, and
//! records the outcome in a status file the engine reports from. Teardown
//! runs headless before the worktree is removed, with a time limit.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use zeron_proto::WorktreeSetupState;

pub const TEARDOWN_STEP_TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_TAIL_BYTES: usize = 800;

pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// A POSIX script running `steps` in order, each in its own subshell.
pub fn setup_script(steps: &[String], status: &Path) -> String {
    let status = sh_quote(&status.to_string_lossy());
    let total = steps.len();
    let mut script = format!("#!/bin/sh\nprintf 'running\\n' > {status}\n");
    for (index, step) in steps.iter().enumerate() {
        let number = index + 1;
        script.push_str(&format!(
            "printf '\\033[2m[%s/%s] %s\\033[0m\\n' {number} {total} {}\n(\n{step}\n)\ncode=$?\nif [ \"$code\" -ne 0 ]; then\n  printf 'failed %s %s\\n' {number} \"$code\" > {status}\n  printf '\\n\\033[31mWorktree setup stopped: step %s of %s failed (exit %s). The worktree is kept; fix the problem and rerun the step here.\\033[0m\\n' {number} {total} \"$code\"\n  exit \"$code\"\nfi\n",
            sh_quote(step)
        ));
    }
    script.push_str(&format!(
        "printf 'ok\\n' > {status}\nprintf '\\033[32mWorktree setup finished.\\033[0m\\n'\n"
    ));
    script
}

pub fn read_setup_state(status: &Path, steps: &[String]) -> Option<WorktreeSetupState> {
    let text = std::fs::read_to_string(status).ok()?;
    let mut words = text.split_whitespace();
    match words.next()? {
        "running" => Some(WorktreeSetupState::Running),
        "ok" => Some(WorktreeSetupState::Succeeded),
        "failed" => {
            let step: usize = words.next()?.parse().ok()?;
            let exit_code: i32 = words.next()?.parse().ok()?;
            Some(WorktreeSetupState::Failed {
                step,
                exit_code,
                command: steps.get(step.checked_sub(1)?).cloned().unwrap_or_default(),
            })
        }
        _ => None,
    }
}

/// Run `commands` one after another in `cwd`; the first failure stops and
/// is described with the tail of its output.
pub async fn run_headless(
    label: &str,
    commands: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<(), String> {
    for (index, command) in commands.iter().enumerate() {
        let mut process = shell_command(command);
        process
            .current_dir(cwd)
            .envs(environment)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = login_path() {
            process.env("PATH", path);
        }
        let step = format!("{label} step {} ({command})", index + 1);
        let output = match tokio::time::timeout(timeout, process.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("{step} could not start: {error}")),
            Err(_) => {
                return Err(format!(
                    "{step} did not finish within {}s",
                    timeout.as_secs()
                ));
            }
        };
        if !output.status.success() {
            let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&output.stdout).into_owned();
            }
            let text = text.trim();
            let tail =
                &text[floor_char_boundary(text, text.len().saturating_sub(OUTPUT_TAIL_BYTES))..];
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string());
            return Err(if tail.is_empty() {
                format!("{step} failed (exit {code})")
            } else {
                format!("{step} failed (exit {code}): {tail}")
            });
        }
    }
    Ok(())
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("/bin/sh");
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    use std::os::windows::process::CommandExt;
    let mut process = tokio::process::Command::new("cmd.exe");
    process.arg("/C").raw_arg(command);
    process.as_std_mut().creation_flags(0x08000000);
    process
}

/// The engine's PATH plus the user's login-shell PATH, so commands find
/// the same tools they do in a terminal.
fn login_path() -> Option<std::ffi::OsString> {
    let mut paths: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(shell) = zeron_harness::shell_env::login_shell_path() {
        paths.extend(std::env::split_paths(shell));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| !path.as_os_str().is_empty() && seen.insert(path.clone()));
    std::env::join_paths(paths).ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn setup_script_stops_at_the_first_failure_and_records_it() {
        let temp = tempfile::tempdir().unwrap();
        let status = temp.path().join("status");
        let marker = temp.path().join("marker");
        let steps = vec![
            format!("echo one > {}", sh_quote(&marker.to_string_lossy())),
            "exit 3".to_string(),
            format!("echo three >> {}", sh_quote(&marker.to_string_lossy())),
        ];
        let script = temp.path().join("setup.sh");
        std::fs::write(&script, setup_script(&steps, &status)).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg(&script)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&output.stdout).contains("step 2 of 3 failed (exit 3)"));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "one\n");
        assert_eq!(
            read_setup_state(&status, &steps),
            Some(WorktreeSetupState::Failed {
                step: 2,
                exit_code: 3,
                command: "exit 3".into()
            })
        );

        let fine = vec!["true".to_string(), "echo 'it''s quoted'".to_string()];
        std::fs::write(&script, setup_script(&fine, &status)).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg(&script)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            read_setup_state(&status, &fine),
            Some(WorktreeSetupState::Succeeded)
        );
    }

    #[tokio::test]
    async fn headless_commands_get_the_environment_and_report_failures() {
        let temp = tempfile::tempdir().unwrap();
        let environment = BTreeMap::from([("GODSPEED_PORT".to_string(), "41000".to_string())]);
        run_headless(
            "Teardown",
            &["echo $GODSPEED_PORT > port.txt".into()],
            temp.path(),
            &environment,
            TEARDOWN_STEP_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("port.txt")).unwrap(),
            "41000\n"
        );
        let error = run_headless(
            "Teardown",
            &[
                "true".into(),
                "echo no database >&2; exit 4".into(),
                "touch never".into(),
            ],
            temp.path(),
            &environment,
            TEARDOWN_STEP_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(error.contains("Teardown step 2"), "{error}");
        assert!(error.contains("exit 4"), "{error}");
        assert!(error.contains("no database"), "{error}");
        assert!(!temp.path().join("never").exists());
        let slow = run_headless(
            "Teardown",
            &["sleep 5".into()],
            temp.path(),
            &environment,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(slow.contains("did not finish"), "{slow}");
    }
}
