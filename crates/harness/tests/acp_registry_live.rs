//! Opt-in live check of the ACP registry route for the agents Godspeed
//! promises (Factory Droid, GitHub Copilot CLI, Gemini CLI, Kimi CLI):
//! fetch the real index, install each agent with checksum verification into
//! a scratch dir, launch it with a scratch HOME and complete the ACP
//! `initialize` handshake. Never touches a real config dir or login.
//!
//! `GODSPEED_ACP_LIVE=1 TMPDIR=<scratch> cargo test -p zeron-harness --test acp_registry_live -- --ignored --nocapture`

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use zeron_harness::acp_registry::{self as registry, HttpsDownloader};

const AGENTS: [&str; 4] = ["factory-droid", "github-copilot-cli", "gemini", "kimi"];

#[tokio::test]
#[ignore = "network + large downloads; run with GODSPEED_ACP_LIVE=1"]
async fn promised_registry_agents_install_and_speak_acp() {
    if std::env::var_os("GODSPEED_ACP_LIVE").is_none() {
        eprintln!("set GODSPEED_ACP_LIVE=1 to run");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("acp-registry");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bytes = registry::fetch_index_bytes(registry::REGISTRY_URL).await.unwrap();
    let index = registry::parse_index(&bytes).unwrap();
    let target = registry::platform_target();
    let wanted: Vec<String> = std::env::var("GODSPEED_ACP_AGENTS")
        .map(|list| list.split(',').map(str::to_owned).collect())
        .unwrap_or_else(|_| AGENTS.iter().map(|id| id.to_string()).collect());
    let mut failures = Vec::new();
    for id in &wanted {
        let Some(agent) = index.agents.iter().find(|a| &a.id == id) else {
            failures.push(format!("{id}: not in the registry"));
            continue;
        };
        let plan = match registry::plan_install(agent, target) {
            Ok(plan) => plan,
            Err(reason) => {
                eprintln!("{id}: not installable here: {reason}");
                continue;
            }
        };
        let started = std::time::Instant::now();
        let installed = match registry::install(&root, agent, &plan, &HttpsDownloader).await {
            Ok(installed) => installed,
            Err(error) => {
                failures.push(format!("{id}: install failed: {error}"));
                continue;
            }
        };
        eprintln!(
            "{id} {} installed ({:?}, {}) in {:.1}s",
            installed.receipt.version,
            installed.receipt.kind,
            installed.receipt.integrity.split_at(20.min(installed.receipt.integrity.len())).0,
            started.elapsed().as_secs_f32()
        );
        let (program, args, env) = registry::launch_command(&installed).unwrap();
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .envs(&env)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env_remove("GEMINI_API_KEY")
            .env_remove("GOOGLE_API_KEY")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("FACTORY_API_KEY")
            .current_dir(&home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = program.parent() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(&path));
            command.env("PATH", std::env::join_paths(paths).unwrap());
        }
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false
                },
                "clientInfo": {"name": "godspeed-live-check", "version": "0"}
            }
        });
        stdin
            .write_all(format!("{init}\n").as_bytes())
            .await
            .unwrap();
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let reply = tokio::time::timeout(Duration::from_secs(90), async {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if value.get("id") == Some(&serde_json::json!(1)) {
                    return Some(value);
                }
            }
            None
        })
        .await;
        let _ = child.kill().await;
        match reply {
            Ok(Some(value)) if value.get("result").is_some() => eprintln!(
                "{id}: initialize ok, protocolVersion {}, auth methods {}",
                value["result"]["protocolVersion"],
                value["result"]["authMethods"]
                    .as_array()
                    .map(|methods| methods
                        .iter()
                        .filter_map(|m| m["id"].as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default()
            ),
            Ok(Some(value)) => failures.push(format!("{id}: initialize error {value}")),
            Ok(None) => failures.push(format!("{id}: exited before answering initialize")),
            Err(_) => failures.push(format!("{id}: no initialize reply within 90s")),
        }
        assert!(registry::uninstall(&root, id).unwrap());
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
