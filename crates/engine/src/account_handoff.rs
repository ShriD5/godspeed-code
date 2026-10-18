//! Continue on another account: fork a chat (usually a Limited one) into a
//! NEW chat that runs on another signed-in account of the same provider, or
//! on another provider. Always the user's explicit choice and never in place:
//! switching mid-thread loses the prompt cache, so the original chat is left
//! exactly as it was and keeps its own resume-at-reset.
//!
//! - **Which account.** The target must be a signed-in account of the target
//!   harness, and not the login the source already runs on: the one its
//!   limit hit ([`ChatLimit::login`]), else the live one.
//! - **Switching.** Agent CLIs keep one live login per device (per model
//!   provider for OpenCode / Pi), so continuing on an account that is not
//!   live switches the harness to it, as Settings → Accounts does.
//! - **Context.** The new chat shows the source's transcript and a "Forked
//!   from" seam naming the source (the link back). What the provider gets:
//!   - same provider with a native fork (Claude Code): the chat points at the
//!     source's session and its first run passes `--fork-session`
//!     ([`zeron_proto::FORK_SESSION_OPTION`]), so the agent has the whole
//!     conversation and the source's session is never appended to;
//!   - otherwise: a compact summary built here from the transcript (no model
//!     call, [`continuation_brief`]) leads the first prompt of every fresh
//!     provider session.
//!
//! [`ChatLimit::login`]: zeron_proto::ChatLimit::login

use async_trait::async_trait;
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, Chat, ChatConfig, ContinuationContext,
    ContinueOnAccountReply, ContinueOnAccountRequest, HarnessId, ToolCall,
};

use crate::EngineError;
use crate::agent_accounts::AgentAccounts;
use crate::doc_host::DocHost;
use crate::workspace_host::WorkspaceHost;

/// Longest summary carried into the new chat's first prompt.
const BRIEF_MAX_CHARS: usize = 6_000;
const FIRST_REQUEST_CHARS: usize = 1_200;
const LATER_REQUEST_CHARS: usize = 400;
const LATER_REQUESTS: usize = 4;
const LAST_REPLY_CHARS: usize = 1_500;
const FILES_LISTED: usize = 25;
const COMMANDS_LISTED: usize = 6;
const COMMAND_CHARS: usize = 160;
const TITLE_SUFFIX: &str = " (continued)";

/// The device's agent logins, as the handoff needs them. Implemented by
/// [`AgentAccounts`]; tests substitute their own (the real one reads the
/// user's CLI credential stores).
#[async_trait]
pub trait AccountSwitcher: Send + Sync {
    async fn accounts(&self) -> Result<AgentAccountsSnapshot, EngineError>;
    async fn activate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError>;
}

#[async_trait]
impl AccountSwitcher for AgentAccounts {
    async fn accounts(&self) -> Result<AgentAccountsSnapshot, EngineError> {
        self.list(false).await
    }

    async fn activate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        AgentAccounts::activate(self, harness, account_id).await
    }
}

/// What [`continue_on_account`] did.
#[derive(Debug, Clone)]
pub struct Handoff {
    pub reply: ContinueOnAccountReply,
    /// The harness's live login changed to the target account.
    pub switched: bool,
}

fn failed(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

/// Whether `account` is the login `source` already runs on: the one its
/// limit hit, or, when the limit recorded none, the harness's live one. Pure.
pub fn is_same_login(
    source: &Chat,
    snapshot: &AgentAccountsSnapshot,
    account: &AgentAccount,
) -> bool {
    if source.config.as_ref().map(|c| c.harness) != Some(account.harness) {
        return false;
    }
    let recorded = source
        .limit
        .as_ref()
        .and_then(|limit| limit.login.as_ref())
        .filter(|login| login.harness == account.harness)
        .and_then(|login| login.fingerprint.as_deref());
    match recorded {
        Some(fingerprint) => snapshot.fingerprint_with(account).as_deref() == Some(fingerprint),
        None => account.active,
    }
}

/// The source session the new chat forks natively on `harness`, when that
/// harness can fork a session it did not create (Claude Code's
/// `--resume … --fork-session`) and the source has one. Pure.
pub fn native_fork_session(source: &Chat, harness: HarnessId) -> Option<String> {
    let same_provider = source.config.as_ref().map(|c| c.harness) == Some(harness);
    (same_provider && harness == HarnessId::ClaudeCode)
        .then(|| source.harness_session_id.clone())
        .flatten()
        .filter(|id| !id.is_empty())
}

/// "Fix the login bug (continued)", without stacking suffixes. Pure.
pub fn continued_title(source: &Chat) -> String {
    let base = source
        .title
        .clone()
        .or_else(|| source.last_message_preview.clone())
        .unwrap_or_else(|| "New session".into());
    let base = base.trim_end_matches(TITLE_SUFFIX);
    format!("{base}{TITLE_SUFFIX}")
}

/// `text` cut to `max` characters on a char boundary, marked when cut.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn entry_text(entry: &SessionMessageEntry) -> String {
    entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A compact, model-free summary of a conversation for a new provider
/// session: the original request, the latest requests, the files changed,
/// recent commands and the last reply, capped at [`BRIEF_MAX_CHARS`].
/// `None` when the transcript says nothing. Pure.
pub fn continuation_brief(source_title: &str, entries: &[SessionMessageEntry]) -> Option<String> {
    let requests: Vec<String> = entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .map(entry_text)
        .filter(|t| !t.is_empty())
        .collect();
    let last_reply = entries
        .iter()
        .rev()
        .filter(|e| e.role == MessageRole::Assistant)
        .map(entry_text)
        .find(|t| !t.is_empty());
    if requests.is_empty() && last_reply.is_none() {
        return None;
    }
    let mut files: Vec<String> = Vec::new();
    let mut commands: Vec<String> = Vec::new();
    for part in entries.iter().flat_map(|e| e.parts.iter()) {
        let MessagePart::Tool {
            call, diff_stats, ..
        } = part
        else {
            continue;
        };
        let mut touched: Vec<&str> = match call {
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => vec![path],
            ToolCall::ApplyPatch { path: Some(path) } => vec![path],
            _ => Vec::new(),
        };
        touched.extend(diff_stats.iter().flatten().map(|stat| stat.path.as_str()));
        for path in touched {
            if !path.is_empty() && !files.iter().any(|f| f == path) {
                files.push(path.to_owned());
            }
        }
        if let ToolCall::Exec { command } = call {
            let command = clip(&command.replace('\n', " "), COMMAND_CHARS);
            commands.retain(|c| *c != command);
            commands.push(command);
        }
    }

    let mut brief = format!(
        "This conversation continues \"{}\" from another session that stopped on its usage \
         limit. Below is a summary of it taken from its transcript; pick up the work from there.\n\n\
         <previous-conversation>\n",
        clip(source_title, 120)
    );
    if let Some(first) = requests.first() {
        brief.push_str(&format!(
            "Original request:\n{}\n\n",
            clip(first, FIRST_REQUEST_CHARS)
        ));
    }
    let later = &requests[requests.len().min(1)..];
    if !later.is_empty() {
        brief.push_str("Latest requests:\n");
        for request in &later[later.len().saturating_sub(LATER_REQUESTS)..] {
            brief.push_str(&format!("- {}\n", clip(request, LATER_REQUEST_CHARS)));
        }
        brief.push('\n');
    }
    if !files.is_empty() {
        let shown = files.len().min(FILES_LISTED);
        brief.push_str(&format!("Files changed: {}", files[..shown].join(", ")));
        if files.len() > shown {
            brief.push_str(&format!(" (+{} more)", files.len() - shown));
        }
        brief.push_str("\n\n");
    }
    if !commands.is_empty() {
        brief.push_str("Recent commands:\n");
        for command in &commands[commands.len().saturating_sub(COMMANDS_LISTED)..] {
            brief.push_str(&format!("- `{command}`\n"));
        }
        brief.push('\n');
    }
    if let Some(reply) = last_reply {
        brief.push_str(&format!(
            "Last reply:\n{}\n",
            clip(&reply, LAST_REPLY_CHARS)
        ));
    }
    let closing = "</previous-conversation>";
    let body = clip(&brief, BRIEF_MAX_CHARS - closing.len() - 1);
    Some(format!("{body}\n{closing}"))
}

/// The marker entry that ends the copied history and names the source.
fn seam(chat_id: &str, source: &Chat, device_id: &str) -> SessionMessageEntry {
    let id = format!("fork:{chat_id}");
    SessionMessageEntry {
        duration_ms: None,
        turn_stats: None,
        id: id.clone(),
        role: MessageRole::System,
        parts: vec![MessagePart::Fork {
            id,
            source_chat_id: source.id.clone(),
            source_title: source
                .title
                .clone()
                .or_else(|| source.last_message_preview.clone())
                .unwrap_or_else(|| "New session".into()),
        }],
        created_at: chrono::Utc::now().timestamp_millis(),
        device_id: device_id.to_owned(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    }
}

/// The source chat this one was continued from, read off its seam.
fn continued_from(entries: &[SessionMessageEntry], chat_id: &str) -> Option<String> {
    let seam_id = format!("fork:{chat_id}");
    entries
        .iter()
        .filter(|e| e.id == seam_id)
        .flat_map(|e| e.parts.iter())
        .find_map(|part| match part {
            MessagePart::Fork { source_chat_id, .. } => Some(source_chat_id.clone()),
            _ => None,
        })
}

/// Fork `request.source_chat_id` into `request.chat_id` on the requested
/// account (see the module docs). Idempotent per new chat id; refuses the
/// source's own login, unknown accounts and chats hosted elsewhere, and
/// changes nothing when it refuses.
pub async fn continue_on_account(
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
    accounts: &dyn AccountSwitcher,
    request: ContinueOnAccountRequest,
) -> Result<Handoff, EngineError> {
    let source = workspace
        .chat(&request.source_chat_id)?
        .ok_or_else(|| failed("That chat no longer exists"))?;
    if source.device_id != doc_host.device_id() {
        return Err(failed("Continue a chat on the device that hosts it"));
    }
    if let Some(existing) = workspace.chat(&request.chat_id)? {
        let handle = doc_host.open(&existing.id)?;
        let entries = handle.doc().read_entries()?;
        if continued_from(&entries, &existing.id).as_deref() != Some(source.id.as_str()) {
            return Err(failed("Chat id already exists"));
        }
        let context = if handle.doc().native_fork_session().is_some() {
            ContinuationContext::NativeFork
        } else {
            ContinuationContext::Brief
        };
        return Ok(Handoff {
            reply: ContinueOnAccountReply {
                chat: existing,
                context,
            },
            switched: false,
        });
    }

    let snapshot = accounts.accounts().await?;
    let target = snapshot
        .accounts
        .iter()
        .find(|a| a.id == request.account_id && a.harness == request.harness)
        .cloned()
        .ok_or_else(|| failed("That account is no longer signed in"))?;
    if is_same_login(&source, &snapshot, &target) {
        return Err(failed(
            "This chat already runs on that account; pick another one",
        ));
    }
    if !target.active && !target.switchable {
        return Err(failed("Godspeed can't switch to that account"));
    }

    let source_doc = doc_host.open(&source.id)?;
    let entries = source_doc.doc().read_entries()?;
    let native = native_fork_session(&source, target.harness);
    let brief = match native {
        Some(_) => None,
        None => Some(
            continuation_brief(&continued_title(&source), &entries)
                .ok_or_else(|| failed("Nothing to continue yet: this chat has no messages"))?,
        ),
    };

    // Everything is checked: switch, then write. The switch is the user's
    // pick; a failed write afterwards leaves the account switched, as a
    // Settings switch would.
    let switched = !target.active;
    if switched {
        accounts.activate(target.harness, &target.id).await?;
    }

    let mut chat = source.clone();
    chat.id = request.chat_id;
    chat.parent_chat_id = None;
    chat.title = Some(continued_title(&source));
    chat.archived = false;
    chat.created_at = chrono::Utc::now();
    chat.last_message_at = None;
    chat.last_message_preview = None;
    chat.last_seen_at = None;
    chat.limit = None;
    chat.lifecycle = Default::default();
    chat.room_gen = Some(2);
    let same_provider = source.config.as_ref().map(|c| c.harness) == Some(target.harness);
    chat.config = match (source.config.clone(), same_provider) {
        (Some(mut config), true) => {
            if request.model.is_some() {
                config.model = request.model.clone();
            }
            Some(config)
        }
        (config, _) => Some(ChatConfig {
            harness: target.harness,
            model: request.model.clone(),
            reasoning: None,
            model_options: Default::default(),
            sandbox: config
                .as_ref()
                .map_or(zeron_proto::SandboxLevel::WorkspaceWrite, |c| c.sandbox),
            access_mode: config.and_then(|c| c.access_mode),
        }),
    };
    if native.is_none() {
        chat.harness_session_id = None;
        chat.harness_session_cwd = None;
    }

    // History first, then the row, so a crash never leaves a discoverable
    // empty chat (the side-chat fork's order).
    let target_doc = doc_host.open(&chat.id)?;
    for entry in entries
        .iter()
        .filter(|e| e.status != Some(MessageStatus::Streaming))
    {
        let mut entry = entry.clone();
        // Old approvals belong to the source's runtime.
        for part in &mut entry.parts {
            if let MessagePart::Input { resolved, .. } = part {
                *resolved = true;
            }
        }
        target_doc.doc().push_message(&entry)?;
    }
    target_doc
        .doc()
        .push_message(&seam(&chat.id, &source, doc_host.device_id()))?;
    let context = match (&native, &brief) {
        (Some(session), _) => {
            target_doc.doc().set_native_fork_session(session)?;
            ContinuationContext::NativeFork
        }
        (None, Some(brief)) => {
            target_doc.doc().set_carried_context(brief)?;
            ContinuationContext::Brief
        }
        (None, None) => unreachable!("a brief is built whenever there is no native fork"),
    };
    doc_host.persist_fork(&target_doc)?;
    workspace.import_chat_row(&chat)?;
    tracing::info!(
        source = %source.id,
        chat = %chat.id,
        harness = ?target.harness,
        ?context,
        switched,
        "continued chat on another account"
    );
    Ok(Handoff {
        reply: ContinueOnAccountReply { chat, context },
        switched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use zeron_proto::{ChatLimit, LimitLogin, LimitRecovery, login_fingerprint};

    fn account(id: &str, harness: HarnessId, active: bool) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": harness,
            "email": format!("{id}@example.com"),
            "planLabel": null,
            "active": active,
            "switchable": true,
        }))
        .unwrap()
    }

    fn snapshot(accounts: Vec<AgentAccount>) -> AgentAccountsSnapshot {
        AgentAccountsSnapshot {
            accounts,
            warnings: Vec::new(),
        }
    }

    /// Accounts in memory; records activations.
    struct Fake {
        snapshot: Mutex<AgentAccountsSnapshot>,
        activated: Mutex<Vec<(HarnessId, String)>>,
    }

    impl Fake {
        fn new(accounts: Vec<AgentAccount>) -> Arc<Self> {
            Arc::new(Self {
                snapshot: Mutex::new(snapshot(accounts)),
                activated: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AccountSwitcher for Fake {
        async fn accounts(&self) -> Result<AgentAccountsSnapshot, EngineError> {
            Ok(self.snapshot.lock().unwrap().clone())
        }
        async fn activate(
            &self,
            harness: HarnessId,
            account_id: &str,
        ) -> Result<AgentAccountsSnapshot, EngineError> {
            self.activated
                .lock()
                .unwrap()
                .push((harness, account_id.to_owned()));
            let mut snapshot = self.snapshot.lock().unwrap();
            for row in snapshot
                .accounts
                .iter_mut()
                .filter(|a| a.harness == harness)
            {
                row.active = row.id == account_id;
            }
            Ok(snapshot.clone())
        }
    }

    fn text(id: &str, role: MessageRole, body: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            duration_ms: None,
            turn_stats: None,
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: format!("{id}-t"),
                text: body.into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn tool(id: &str, call: ToolCall) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call,
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    fn limited(harness: HarnessId, fingerprint: Option<String>) -> ChatLimit {
        ChatLimit {
            limited_at: chrono::Utc::now(),
            reset_at: None,
            reset_source: None,
            window: Some("5-hour".into()),
            recovery: LimitRecovery::Ask,
            login: Some(LimitLogin {
                harness,
                fingerprint,
            }),
        }
    }

    fn chat_on(harness: HarnessId) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": "src",
            "deviceId": "dev",
            "title": "Fix the login bug",
            "archived": false,
            "cwd": "/tmp",
            "branch": null,
            "checkoutId": null,
            "config": {
                "harness": harness,
                "model": "opus",
                "reasoning": null,
                "sandbox": "workspace-write",
            },
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": chrono::Utc::now(),
            "harnessSessionId": "claude-session-1",
            "harnessSessionCwd": "/tmp",
        }))
        .unwrap()
    }

    #[test]
    fn the_limited_login_is_refused_and_others_are_not() {
        let accounts = snapshot(vec![
            account("a", HarnessId::ClaudeCode, true),
            account("b", HarnessId::ClaudeCode, false),
            account("x", HarnessId::Codex, true),
        ]);
        let mut source = chat_on(HarnessId::ClaudeCode);
        // No limit recorded: the live login is the one it runs on.
        assert!(is_same_login(&source, &accounts, &accounts.accounts[0]));
        assert!(!is_same_login(&source, &accounts, &accounts.accounts[1]));
        assert!(!is_same_login(&source, &accounts, &accounts.accounts[2]));
        // The limit hit `b` (the user switched to `a` since): `b` is refused.
        source.limit = Some(limited(
            HarnessId::ClaudeCode,
            login_fingerprint(["b"].into_iter()),
        ));
        assert!(!is_same_login(&source, &accounts, &accounts.accounts[0]));
        assert!(is_same_login(&source, &accounts, &accounts.accounts[1]));
    }

    #[test]
    fn only_claude_on_the_same_provider_forks_natively() {
        let source = chat_on(HarnessId::ClaudeCode);
        assert_eq!(
            native_fork_session(&source, HarnessId::ClaudeCode).as_deref(),
            Some("claude-session-1")
        );
        assert_eq!(native_fork_session(&source, HarnessId::Codex), None);
        let codex = chat_on(HarnessId::Codex);
        assert_eq!(native_fork_session(&codex, HarnessId::Codex), None);
        let mut fresh = chat_on(HarnessId::ClaudeCode);
        fresh.harness_session_id = Some(String::new());
        assert_eq!(native_fork_session(&fresh, HarnessId::ClaudeCode), None);
    }

    #[test]
    fn titles_do_not_stack() {
        let mut source = chat_on(HarnessId::ClaudeCode);
        assert_eq!(continued_title(&source), "Fix the login bug (continued)");
        source.title = Some("Fix the login bug (continued)".into());
        assert_eq!(continued_title(&source), "Fix the login bug (continued)");
    }

    #[test]
    fn the_brief_is_compact_and_keeps_what_matters() {
        let mut entries = vec![text(
            "u1",
            MessageRole::User,
            "Fix the login bug in auth.rs",
        )];
        let mut work = text("a1", MessageRole::Assistant, "Looking at it.");
        work.parts.push(tool(
            "t1",
            ToolCall::EditFile {
                path: "src/auth.rs".into(),
                old_string: None,
                new_string: None,
            },
        ));
        work.parts.push(tool(
            "t2",
            ToolCall::Exec {
                command: "cargo test -p auth".into(),
            },
        ));
        work.parts.push(tool(
            "t3",
            ToolCall::EditFile {
                path: "src/auth.rs".into(),
                old_string: None,
                new_string: None,
            },
        ));
        entries.push(work);
        for i in 0..8 {
            entries.push(text(
                &format!("u{}", i + 2),
                MessageRole::User,
                &format!("follow-up {i}"),
            ));
        }
        entries.push(text("a9", MessageRole::Assistant, &"z".repeat(5_000)));
        let brief = continuation_brief("Fix the login bug", &entries).unwrap();
        assert!(brief.contains("\"Fix the login bug\""));
        assert!(brief.contains("Original request:\nFix the login bug in auth.rs"));
        // Only the latest few follow-ups.
        assert!(brief.contains("- follow-up 7"));
        assert!(brief.contains("- follow-up 4"));
        assert!(!brief.contains("follow-up 3"));
        assert_eq!(brief.matches("src/auth.rs").count(), 1);
        assert!(brief.contains("- `cargo test -p auth`"));
        // The long reply is clipped and the whole brief capped.
        assert!(brief.contains('…'));
        assert!(brief.chars().count() <= BRIEF_MAX_CHARS);
        assert!(brief.ends_with("</previous-conversation>"));
        assert_eq!(continuation_brief("Empty", &[]), None);
    }

    /// A harness that records each request and answers with a session id.
    struct Capture {
        id: HarnessId,
        requests: Arc<Mutex<Vec<zeron_proto::RunRequest>>>,
    }

    #[async_trait]
    impl zeron_harness::Harness for Capture {
        fn id(&self) -> HarnessId {
            self.id
        }
        fn display_name(&self) -> &str {
            "Capture"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> zeron_proto::SteeringMode {
            zeron_proto::SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[zeron_proto::ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<zeron_proto::Model>, zeron_harness::HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            request: zeron_proto::RunRequest,
            _: zeron_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<zeron_proto::AgentEvent, zeron_harness::HarnessError>,
            >,
            zeron_harness::HarnessError,
        > {
            use futures::StreamExt as _;
            let n = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request);
                requests.len()
            };
            Ok(
                futures::stream::iter(vec![Ok(zeron_proto::AgentEvent::Done {
                    status: zeron_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(format!("own-session-{n}")),
                })])
                .boxed(),
            )
        }
    }

    async fn engine_with(
        source: &Chat,
        entries: &[SessionMessageEntry],
    ) -> (tempfile::TempDir, crate::EngineCore) {
        engine_capturing(source, entries, Vec::new()).await
    }

    async fn engine_capturing(
        source: &Chat,
        entries: &[SessionMessageEntry],
        harnesses: Vec<Capture>,
    ) -> (tempfile::TempDir, crate::EngineCore) {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::HarnessRegistry::new();
        for harness in harnesses {
            registry.register(Arc::new(harness));
        }
        let core =
            crate::EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
                .unwrap();
        let mut row = source.clone();
        row.device_id = core.device_id.clone();
        core.workspace.import_chat_row(&row).unwrap();
        let doc = core.doc_host.open(&row.id).unwrap();
        for entry in entries {
            doc.doc().push_message(entry).unwrap();
        }
        (dir, core)
    }

    fn request(harness: HarnessId, account: &str) -> ContinueOnAccountRequest {
        ContinueOnAccountRequest {
            chat_id: "next".into(),
            source_chat_id: "src".into(),
            harness,
            account_id: account.into(),
            model: None,
        }
    }

    #[tokio::test]
    async fn continuing_on_another_claude_account_forks_natively_and_leaves_the_source() {
        let mut source = chat_on(HarnessId::ClaudeCode);
        source.limit = Some(limited(
            HarnessId::ClaudeCode,
            login_fingerprint(["a"].into_iter()),
        ));
        let entries = vec![
            text("u1", MessageRole::User, "Remember PINEAPPLE"),
            text("a1", MessageRole::Assistant, "Got it"),
        ];
        let (_dir, core) = engine_with(&source, &entries).await;
        let before = core.workspace.chat("src").unwrap().unwrap();
        let fake = Fake::new(vec![
            account("a", HarnessId::ClaudeCode, true),
            account("b", HarnessId::ClaudeCode, false),
        ]);

        // The limited account itself is refused, and nothing changes.
        let refused = continue_on_account(
            &core.workspace,
            &core.doc_host,
            fake.as_ref(),
            request(HarnessId::ClaudeCode, "a"),
        )
        .await;
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("already runs on that account")
        );
        assert!(core.workspace.chat("next").unwrap().is_none());
        assert!(fake.activated.lock().unwrap().is_empty());

        let handoff = continue_on_account(
            &core.workspace,
            &core.doc_host,
            fake.as_ref(),
            request(HarnessId::ClaudeCode, "b"),
        )
        .await
        .unwrap();
        assert!(handoff.switched);
        assert_eq!(
            *fake.activated.lock().unwrap(),
            [(HarnessId::ClaudeCode, "b".to_owned())]
        );
        assert_eq!(handoff.reply.context, ContinuationContext::NativeFork);
        let chat = core.workspace.chat("next").unwrap().unwrap();
        assert_eq!(
            chat.parent_chat_id, None,
            "a chat of its own, not a side chat"
        );
        assert_eq!(chat.title.as_deref(), Some("Fix the login bug (continued)"));
        assert_eq!(chat.limit, None);
        assert_eq!(chat.harness_session_id.as_deref(), Some("claude-session-1"));
        assert_eq!(chat.config.as_ref().unwrap().model.as_deref(), Some("opus"));
        let doc = core.doc_host.open("next").unwrap();
        assert_eq!(
            doc.doc().native_fork_session().as_deref(),
            Some("claude-session-1")
        );
        assert_eq!(doc.doc().carried_context(), None);
        let copied = doc.doc().read_entries().unwrap();
        assert_eq!(copied.len(), 3);
        assert_eq!(continued_from(&copied, "next").as_deref(), Some("src"));

        // The source is untouched: same row, same transcript.
        assert_eq!(core.workspace.chat("src").unwrap().unwrap(), before);
        assert_eq!(
            core.doc_host
                .open("src")
                .unwrap()
                .doc()
                .read_entries()
                .unwrap()
                .len(),
            2
        );

        // A retry answers with the same chat and switches nothing more.
        let again = continue_on_account(
            &core.workspace,
            &core.doc_host,
            fake.as_ref(),
            request(HarnessId::ClaudeCode, "b"),
        )
        .await
        .unwrap();
        assert_eq!(again.reply.chat.id, "next");
        assert!(!again.switched);
        assert_eq!(fake.activated.lock().unwrap().len(), 1);
        core.shutdown().await;
    }

    #[tokio::test]
    async fn continuing_on_another_provider_carries_a_brief() {
        let source = chat_on(HarnessId::ClaudeCode);
        let entries = vec![
            text("u1", MessageRole::User, "Remember PINEAPPLE"),
            text("a1", MessageRole::Assistant, "Got it"),
        ];
        let (_dir, core) = engine_with(&source, &entries).await;
        let fake = Fake::new(vec![
            account("a", HarnessId::ClaudeCode, true),
            account("x", HarnessId::Codex, true),
        ]);
        let mut ask = request(HarnessId::Codex, "x");
        ask.model = Some("gpt-5.5".into());
        let handoff = continue_on_account(&core.workspace, &core.doc_host, fake.as_ref(), ask)
            .await
            .unwrap();
        // Already live: nothing to switch.
        assert!(!handoff.switched);
        assert_eq!(handoff.reply.context, ContinuationContext::Brief);
        let chat = core.workspace.chat("next").unwrap().unwrap();
        let config = chat.config.unwrap();
        assert_eq!(config.harness, HarnessId::Codex);
        assert_eq!(config.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            chat.harness_session_id, None,
            "never resumes another provider's session"
        );
        let doc = core.doc_host.open("next").unwrap();
        assert_eq!(doc.doc().native_fork_session(), None);
        assert!(doc.doc().carried_context().unwrap().contains("PINEAPPLE"));

        // Unknown accounts are refused.
        let mut unknown = request(HarnessId::Codex, "gone");
        unknown.chat_id = "other".into();
        assert!(
            continue_on_account(&core.workspace, &core.doc_host, fake.as_ref(), unknown)
                .await
                .is_err()
        );
        core.shutdown().await;
    }

    fn run_request(harness: HarnessId, prompt: &str) -> zeron_proto::RunRequest {
        zeron_proto::RunRequest {
            mcp: None,
            prompt: prompt.into(),
            harness: Some(harness),
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            resume: None,
            attachments: vec![],
            worktree: None,
            access: None,
            env: Default::default(),
        }
    }

    /// Dispatch one turn into `chat` and wait for the harness to see it.
    async fn turn(
        core: &crate::EngineCore,
        requests: &Arc<Mutex<Vec<zeron_proto::RunRequest>>>,
        chat: &str,
        harness: HarnessId,
        prompt: &str,
    ) -> zeron_proto::RunRequest {
        let seen = requests.lock().unwrap().len();
        core.sessions
            .dispatch(chat, harness, run_request(harness, prompt), None)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let idle = !core.sessions.has_run(chat);
                if requests.lock().unwrap().len() > seen && idle {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the turn reached the harness");
        requests.lock().unwrap()[seen].clone()
    }

    #[tokio::test]
    async fn the_first_run_forks_the_source_session_then_continues_its_own() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = chat_on(HarnessId::ClaudeCode);
        let entries = vec![text("u1", MessageRole::User, "Remember PINEAPPLE")];
        let (_dir, core) = engine_capturing(
            &source,
            &entries,
            vec![Capture {
                id: HarnessId::ClaudeCode,
                requests: requests.clone(),
            }],
        )
        .await;
        let fake = Fake::new(vec![
            account("a", HarnessId::ClaudeCode, true),
            account("b", HarnessId::ClaudeCode, false),
        ]);
        continue_on_account(
            &core.workspace,
            &core.doc_host,
            fake.as_ref(),
            request(HarnessId::ClaudeCode, "b"),
        )
        .await
        .unwrap();

        let first = turn(&core, &requests, "next", HarnessId::ClaudeCode, "Continue").await;
        assert_eq!(first.resume.as_deref(), Some("claude-session-1"));
        assert_eq!(
            first.model_options.get(zeron_proto::FORK_SESSION_OPTION),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(first.prompt, "Continue", "a native fork carries no summary");
        let second = turn(&core, &requests, "next", HarnessId::ClaudeCode, "And then?").await;
        assert_eq!(second.resume.as_deref(), Some("own-session-1"));
        assert!(
            !second
                .model_options
                .contains_key(zeron_proto::FORK_SESSION_OPTION)
        );
        // The source still resumes its own session, unforked.
        let original = turn(&core, &requests, "src", HarnessId::ClaudeCode, "Later").await;
        assert_eq!(original.resume.as_deref(), Some("claude-session-1"));
        assert!(
            !original
                .model_options
                .contains_key(zeron_proto::FORK_SESSION_OPTION)
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn a_fresh_session_on_another_provider_starts_from_the_brief() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = chat_on(HarnessId::ClaudeCode);
        let entries = vec![
            text("u1", MessageRole::User, "Remember PINEAPPLE"),
            text("a1", MessageRole::Assistant, "Got it"),
        ];
        let (_dir, core) = engine_capturing(
            &source,
            &entries,
            vec![Capture {
                id: HarnessId::Codex,
                requests: requests.clone(),
            }],
        )
        .await;
        let fake = Fake::new(vec![
            account("a", HarnessId::ClaudeCode, true),
            account("x", HarnessId::Codex, true),
        ]);
        continue_on_account(
            &core.workspace,
            &core.doc_host,
            fake.as_ref(),
            request(HarnessId::Codex, "x"),
        )
        .await
        .unwrap();
        let first = turn(&core, &requests, "next", HarnessId::Codex, "Continue").await;
        assert_eq!(first.resume, None);
        assert!(first.prompt.starts_with("This conversation continues"));
        assert!(first.prompt.contains("PINEAPPLE"));
        assert!(
            first
                .prompt
                .ends_with("</previous-conversation>\n\nContinue")
        );
        // The provider session holds it now.
        let second = turn(&core, &requests, "next", HarnessId::Codex, "And then?").await;
        assert_eq!(second.resume.as_deref(), Some("own-session-1"));
        assert_eq!(second.prompt, "And then?");
        core.shutdown().await;
    }
}
