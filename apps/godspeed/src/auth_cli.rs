//! `godspeed login` / `godspeed logout` / `godspeed status` — the standalone auth surface.
//!
//! Sign-in used to live only inside `godspeed headless`, coupling authentication to
//! the long-running daemon. These commands work on the persisted session
//! (`{data_dir}/session.json`) and exit, so a service-managed `godspeed headless`
//! only ever *loads* credentials. While an engine is running it owns the session
//! (WorkOS refresh tokens are single-use and rotate on every refresh), so login
//! and logout take the same data-dir lock the engine holds and refuse politely
//! when it is busy.

use std::io::IsTerminal;

use zeron_engine::{AuthState, Engine, EngineConfig, InstanceLock, WorkspaceScope};

#[derive(Debug, PartialEq, Eq)]
struct AccountStatus {
    mode: &'static str,
    auth: String,
    healthy: bool,
}

fn account_status(scope: WorkspaceScope, auth: &AuthState) -> AccountStatus {
    match scope {
        WorkspaceScope::Local => match auth {
            AuthState::SignedOut => AccountStatus {
                mode: "local only",
                auth: "signed out (optional in local-only mode)".into(),
                healthy: true,
            },
            AuthState::NeedsOrganization { user } => AccountStatus {
                mode: "local only",
                auth: format!(
                    "signed in as {}; finish workspace setup to enable sync",
                    user.email
                ),
                healthy: true,
            },
            AuthState::SignedIn { user, .. } => AccountStatus {
                mode: "local only",
                auth: format!("signed in as {}; sync is ready after restart", user.email),
                healthy: true,
            },
        },
        WorkspaceScope::Development => AccountStatus {
            mode: "development",
            auth: "dev mode (bearer = user id)".into(),
            healthy: true,
        },
        WorkspaceScope::Synced => match auth {
            AuthState::SignedIn { user, org_id } => AccountStatus {
                mode: "synced",
                auth: format!(
                    "signed in as {}{}",
                    user.email,
                    org_id
                        .as_ref()
                        .map(|org| format!(" (workspace {org})"))
                        .unwrap_or_default()
                ),
                healthy: true,
            },
            AuthState::NeedsOrganization { user } => AccountStatus {
                mode: "synced",
                auth: format!(
                    "signed in as {} but no workspace selected — run `godspeed login`",
                    user.email
                ),
                healthy: false,
            },
            AuthState::SignedOut => AccountStatus {
                mode: "synced",
                auth: "saved session is no longer valid — run `godspeed login`".into(),
                healthy: false,
            },
        },
    }
}

/// `godspeed login`: authenticate via the paste-code flow (and workspace
/// onboarding), persist `session.json`, and exit. `org` selects a workspace
/// by organization id afterwards.
pub async fn login(config: EngineConfig, org: Option<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = engine_lock(&config, "sign in")?;
    let auth = Engine::build_auth(&config).await;
    if !auth.workos_enabled() {
        println!("Auth is in dev mode (no WorkOS client id) — there is nothing to sign in to.");
        return Ok(());
    }
    if let AuthState::SignedIn { user, org_id } = auth.state() {
        println!(
            "Already signed in as {}{}.",
            user.email,
            org_id
                .map(|org| format!(" (workspace {org})"))
                .unwrap_or_default()
        );
        println!("Run `godspeed logout` first to switch accounts.");
        println!("The next engine start will use the synced workspace.");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("godspeed login needs an interactive terminal");
    }
    zeron_engine::terminal_sign_in(&auth).await?;
    if let Some(org) = org.as_deref()
        && auth.state().org_id() != Some(org)
    {
        auth.select_org(org).await?;
    }
    match auth.state() {
        AuthState::SignedIn { user, org_id } => {
            println!(
                "\nSigned in as {}{}.",
                user.email,
                org_id
                    .map(|org| format!(" (workspace {org})"))
                    .unwrap_or_default()
            );
            println!(
                "Sync is ready. Start or restart Godspeed to open the synced workspace; existing local sessions will stay local."
            );
        }
        // terminal_sign_in only returns Ok once signed in; keep an honest fallback.
        _ => println!("Sign-in did not complete."),
    }
    Ok(())
}

/// `godspeed login --stdin-code [--org ID]`: the sign-in "Run on a server"
/// drives over SSH. No terminal: the URL goes to stdout as
/// `godspeed-login: url <url>`, the code the browser shows arrives on stdin
/// (a rejected code is reported as `godspeed-login: error …` and another can
/// follow), and the result is `godspeed-login: signed-in <email> <org>`,
/// `already-signed-in …` or `dev-mode`. Takes the engine lock like `login`.
pub async fn login_stdin_code(config: EngineConfig, org: Option<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = engine_lock(&config, "sign in")?;
    let auth = Engine::build_auth(&config).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut out = std::io::stdout();
    piped_sign_in(&auth, org.as_deref(), &mut rx, &mut out).await
}

/// The body of [`login_stdin_code`], over any line channel and writer.
async fn piped_sign_in(
    auth: &zeron_engine::Auth,
    org: Option<&str>,
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let report = |state: &AuthState| match state {
        AuthState::SignedIn { user, org_id } => Some(format!(
            "{} {}",
            user.email,
            org_id.as_deref().unwrap_or("-")
        )),
        _ => None,
    };
    if !auth.workos_enabled() {
        writeln!(out, "godspeed-login: dev-mode")?;
        return Ok(());
    }
    let current = auth.state();
    if let AuthState::SignedIn { org_id, .. } = &current
        && (org.is_none() || org_id.as_deref() == org)
    {
        writeln!(
            out,
            "godspeed-login: already-signed-in {}",
            report(&current).unwrap_or_default()
        )?;
        return Ok(());
    }
    if matches!(auth.state(), AuthState::SignedOut) {
        let url = auth.start_headless_sign_in();
        writeln!(out, "godspeed-login: url {url}")?;
        out.flush()?;
        loop {
            let Some(line) = lines.recv().await else {
                anyhow::bail!("stdin closed before a sign-in code arrived");
            };
            let pasted = line.trim();
            if pasted.is_empty() {
                continue;
            }
            match auth.complete_sign_in(pasted).await {
                Ok(()) => break,
                Err(err) => {
                    writeln!(out, "godspeed-login: error {err}")?;
                    out.flush()?;
                }
            }
        }
    }
    match (auth.state(), org) {
        (AuthState::SignedIn { org_id, .. }, Some(want)) if org_id.as_deref() == Some(want) => {}
        (AuthState::SignedIn { .. } | AuthState::NeedsOrganization { .. }, Some(want)) => {
            auth.select_org(want).await?;
        }
        (AuthState::NeedsOrganization { .. }, None) => {
            let orgs = auth.list_orgs().await?;
            match orgs.as_slice() {
                [only] => auth.select_org(&only.organization_id).await?,
                _ => anyhow::bail!(
                    "signed in, but no workspace was chosen — run `godspeed login` on this machine to pick one"
                ),
            }
        }
        _ => {}
    }
    match report(&auth.state()) {
        Some(who) => {
            writeln!(out, "godspeed-login: signed-in {who}")?;
            Ok(())
        }
        None => anyhow::bail!("sign-in did not complete"),
    }
}

/// `godspeed logout`: remove the persisted session.
pub async fn logout(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = engine_lock(&config, "sign out")?;
    let auth = Engine::build_auth(&config).await;
    if !auth.workos_enabled() {
        // Dev mode has no live session, but clear any stale session.json from a
        // previous WorkOS-mode run so the next real run starts signed out.
        auth.sign_out();
        println!("Auth is in dev mode — cleared any stale saved session.");
        println!("The next engine start will remain in development mode.");
        return Ok(());
    }
    match auth.state() {
        AuthState::SignedOut => {
            // Also remove malformed or stale files that could not be loaded.
            auth.sign_out();
            println!("No valid saved session; cleared any stale session file.");
        }
        state => {
            let email = state
                .user()
                .map(|u| u.email.clone())
                .unwrap_or_else(|| "<unknown>".into());
            auth.sign_out();
            println!(
                "Signed out {email} — removed {}.",
                config.data_dir.join("session.json").display()
            );
        }
    }
    println!("The next engine start will use the local-only workspace.");
    Ok(())
}

/// `godspeed status`: report the fixed scope a new engine would select, optional
/// auth, and engine liveness. Local-only is a healthy signed-out state.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let auth = Engine::build_auth(&config).await;
    let next_scope = Engine::initial_workspace_scope(&auth);
    let scope = live_engine_scope(config.ipc_port)
        .await
        .unwrap_or(next_scope);
    let mut account = account_status(scope, &auth.state());
    if scope == WorkspaceScope::Local && !auth.workos_enabled() {
        // Without WorkOS the auth layer reports a synthetic dev user; that is
        // not a sign-in, so describe what is actually configured.
        account.auth = "sync off (no edge configured; see README)".into();
    }
    println!("Data dir: {}", config.data_dir.display());
    println!("Edge:     {}", config.edge_url);
    println!("Mode:     {}", account.mode);
    println!("Auth:     {}", account.auth);
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() {
            "listening on"
        } else {
            "not listening on"
        },
        config.ipc_port
    );
    if !account.healthy {
        std::process::exit(1);
    }
    Ok(())
}

/// Prefer the immutable scope of a live runtime. Falling back to the next-boot
/// derivation is correct when no engine is listening and tolerant of old
/// daemons that predate EngineInfo.
async fn live_engine_scope(ipc_port: u16) -> Option<WorkspaceScope> {
    let client = zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .ok()?;
    let value = client
        .call(zeron_rpc::methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .ok()?;
    serde_json::from_value::<zeron_engine::EngineInfo>(value)
        .ok()
        .map(|info| info.workspace_scope)
}

/// The same exclusive data-dir lock the engine holds for its lifetime: taken for
/// the whole login/logout mutation so we never rotate or delete a session out
/// from under a running engine (whose in-memory copy would fight back — the next
/// token refresh re-persists it).
fn engine_lock(config: &EngineConfig, verb: &str) -> anyhow::Result<InstanceLock> {
    InstanceLock::acquire(&config.data_dir).map_err(|err| {
        anyhow::anyhow!(
            "{err}\nCannot {verb} while an engine is running — stop it first \
             (`godspeed daemon stop`, or quit the Godspeed app), or use the running UI instead."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_engine::{AuthUser, HarnessId};

    fn config(data_dir: &std::path::Path) -> EngineConfig {
        EngineConfig {
            data_dir: data_dir.to_path_buf(),
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            ipc_port: 0,
            default_harness: HarnessId::Mock,
            org_id: None,
            workos_client_id: Some("client_test".into()),
        }
    }

    #[test]
    fn signed_out_local_status_is_healthy() {
        let status = account_status(WorkspaceScope::Local, &AuthState::SignedOut);
        assert_eq!(status.mode, "local only");
        assert_eq!(status.auth, "signed out (optional in local-only mode)");
        assert!(status.healthy);
    }

    #[test]
    fn synced_status_still_requires_a_complete_account() {
        let user = AuthUser {
            id: "user-1".into(),
            email: "user@example.com".into(),
            name: None,
        };
        assert!(
            !account_status(
                WorkspaceScope::Synced,
                &AuthState::NeedsOrganization { user: user.clone() },
            )
            .healthy
        );
        assert!(
            account_status(
                WorkspaceScope::Synced,
                &AuthState::SignedIn {
                    user,
                    org_id: Some("org-1".into()),
                },
            )
            .healthy
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_and_logout_lock_out_a_running_engine() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let _engine = InstanceLock::acquire(dir.path()).unwrap();

        for verb in ["sign in", "sign out"] {
            let error = engine_lock(&config, verb).unwrap_err().to_string();
            assert!(error.contains("while an engine is running"));
            assert!(error.contains("stop it first"));
        }
    }

    /// A closed channel holding `items`, like stdin with that input.
    fn lines(items: &[&str]) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for item in items {
            tx.send((*item).to_owned()).unwrap();
        }
        rx
    }

    #[tokio::test]
    async fn piped_sign_in_reports_dev_mode_and_existing_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut dev = config(dir.path());
        dev.workos_client_id = None;
        let auth = Engine::build_auth(&dev).await;
        let mut out = Vec::new();
        piped_sign_in(&auth, None, &mut lines(&[]), &mut out)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "godspeed-login: dev-mode\n"
        );

        std::fs::write(
            dir.path().join("session.json"),
            r#"{"refreshToken":"refresh-1","user":{"id":"user-1","email":"user@example.com"},"orgId":"org-1"}"#,
        )
        .unwrap();
        let auth = Engine::build_auth(&config(dir.path())).await;
        for org in [None, Some("org-1")] {
            let mut out = Vec::new();
            piped_sign_in(&auth, org, &mut lines(&[]), &mut out)
                .await
                .unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "godspeed-login: already-signed-in user@example.com org-1\n"
            );
        }
    }

    #[tokio::test]
    async fn piped_sign_in_prints_the_url_and_reports_bad_codes() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Engine::build_auth(&config(dir.path())).await;
        let mut out = Vec::new();
        let err = piped_sign_in(
            &auth,
            Some("org-1"),
            &mut lines(&["", "not-a-code"]),
            &mut out,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("stdin closed"), "{err}");
        let out = String::from_utf8(out).unwrap();
        let mut rows = out.lines();
        let url = rows.next().unwrap();
        assert!(url.starts_with("godspeed-login: url "), "{url}");
        assert!(
            url.contains("authorize") && url.contains("callback"),
            "{url}"
        );
        let rejected = rows.next().unwrap();
        assert!(rejected.starts_with("godspeed-login: error "), "{rejected}");
        assert_eq!(rows.next(), None);
        assert!(!dir.path().join("session.json").exists());
    }

    #[tokio::test]
    async fn logout_makes_the_next_engine_start_local() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let session = dir.path().join("session.json");
        std::fs::write(
            &session,
            r#"{"refreshToken":"refresh-1","user":{"id":"user-1","email":"user@example.com"},"orgId":"org-1"}"#,
        )
        .unwrap();
        let before = Engine::build_auth(&config).await;
        assert_eq!(
            Engine::initial_workspace_scope(&before),
            WorkspaceScope::Synced
        );

        logout(config.clone()).await.unwrap();

        assert!(!session.exists());
        let after = Engine::build_auth(&config).await;
        assert_eq!(
            Engine::initial_workspace_scope(&after),
            WorkspaceScope::Local
        );
    }
}
