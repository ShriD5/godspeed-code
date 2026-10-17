//! zeron-rpc — the typed control plane (UiRpc / ControlRpc) over WebSocket + in-memory
//! transports, plus the device-room relay transport ({s,k,to,from} frames — [`device_room`]).
//!
//! Framing: ndjson envelopes, one JSON object per WebSocket text message (or per line on
//! byte transports), matching the shape of godspeed's Effect RPC without the Effect runtime:
//!
//! - client → server: `{id, method, params}` to invoke, `{id, cancel: true}` to stop a stream;
//! - server → client: `{id, ok}` / `{id, err}` for unary calls,
//!   `{id, item}`* then `{id, done: true}` (or `{id, err}`) for streams.
//!
//! The server dispatches into an [`RpcService`]; the [`RpcClient`] offers `call` and
//! `subscribe`. Both ends run over any pair of string channels, so the in-memory transport
//! ([`memory_client`]) exercises the exact same code path as the WebSocket one.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod client;
pub mod device_room;
mod server;

pub use client::{RpcClient, RpcSubscription, connect_ws};
pub use device_room::{
    DeviceFrameHeader, DeviceLink, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig,
    NudgeHandler, PeerLiveness, PeerLivenessProbe, StaticToken, TokenError, TokenSource,
    decode_device_frame, device_room_ws_url, encode_device_frame,
};
pub use server::{MAX_HTTP_BODY, serve_connection, serve_ws_listener};

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/research/feature-inventory.md §2.
pub mod methods {
    pub const VOICE_CAPABILITIES_V2: &str = "VoiceCapabilitiesV2";
    pub const PREPARE_VOICE_V2: &str = "PrepareVoiceV2";
    pub const OWN_VOICE_V2: &str = "OwnVoiceV2";
    pub const NEGOTIATE_VOICE_V2: &str = "NegotiateVoiceV2";
    pub const CONFIRM_VOICE_MEDIA_V2: &str = "ConfirmVoiceMediaV2";
    pub const REPORT_VOICE_MEDIA_V2: &str = "ReportVoiceMediaV2";
    pub const STOP_VOICE_V2: &str = "StopVoiceV2";
    pub const CANCEL_VOICE_ATTEMPT_V2: &str = "CancelVoiceAttemptV2";
    pub const WATCH_PREVIEWS: &str = "WatchPreviews";
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    pub const CANCEL_INSTALL: &str = "CancelInstall";
    pub const INSTALL_HARNESS: &str = "InstallHarness";
    /// Flip a harness's enablement on the target device (Settings → Providers);
    /// replies with the device's fresh `ListHarnesses` catalog.
    pub const GET_TITLE_SETTINGS: &str = "GetTitleSettings";
    pub const SET_TITLE_SETTINGS: &str = "SetTitleSettings";
    pub const SET_HARNESS_ENABLED: &str = "SetHarnessEnabled";
    pub const LIST_MODELS: &str = "ListModels";
    pub const LIST_SKILLS: &str = "ListSkills";
    pub const LIST_COMMANDS: &str = "ListCommands";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
    pub const TAKE_PROJECT_ACTION_SETUP: &str = "TakeProjectActionSetup";
    /// Peer-to-peer delivery fallback: the SENDER's engine forwards a queued
    /// command entry (client-minted id and all) straight over the device-room
    /// link when its chat2 rows can't reach the edge but the host's peer link
    /// is alive. The host claims the id in its processed ledger before
    /// executing, so the doc row arriving later dedupes to a no-op —
    /// exactly-once by construction. Params `{chatId, entry}`.
    pub const RELAY_COMMAND: &str = "RelayCommand";
    /// User-driven delivery retry for a chat with unadopted queued sends:
    /// fresh chat2 socket, host nudge, drain pass, and a new delivery escort
    /// per pending command. Params `{chatId}`; IPC-only.
    pub const RETRY_DELIVERY: &str = "RetryDelivery";
    pub const FORK_SIDE_CHAT: &str = "ForkSideChat";
    pub const WATCH_DOC_MESSAGES: &str = "WatchDocMessages";
    /// Explicit user navigation, `{chatId}`. Prioritizes this device's sync
    /// connection; automatic subscriptions and retries must not call it.
    pub const FOCUS_CHAT: &str = "FocusChat";
    /// Messages typed while the agent was busy, held on the chat doc so every
    /// device sees the same queue. `{ chatId }` → `{ items: QueuedMessage[] }`.
    pub const WATCH_QUEUE: &str = "WatchQueue";
    /// Append to the queue. `{ chatId, text, attachments?, holdForTurnEnd? }` → `{ id }`.
    pub const QUEUE_MESSAGE: &str = "QueueMessage";
    /// Retype a queued message; empty text deletes it.
    /// `{ chatId, id, text }` → `{ changed }`.
    pub const UPDATE_QUEUED_MESSAGE: &str = "UpdateQueuedMessage";
    /// Acquire a host-authoritative edit lease for one queued row.
    pub const BEGIN_QUEUED_MESSAGE_EDIT: &str = "BeginQueuedMessageEdit";
    /// Renew an acquired queue edit lease.
    pub const RENEW_QUEUED_MESSAGE_EDIT: &str = "RenewQueuedMessageEdit";
    /// Commit, cancel, discard, or explicitly release an acquired edit.
    pub const FINISH_QUEUED_MESSAGE_EDIT: &str = "FinishQueuedMessageEdit";
    /// Reorder. `{ chatId, id, toIndex }` → `{ changed }`.
    pub const MOVE_QUEUED_MESSAGE: &str = "MoveQueuedMessage";
    pub const REMOVE_QUEUED_MESSAGE: &str = "RemoveQueuedMessage";
    /// Interrupt whatever is running and send this one. `{ chatId, id }` → `{ sent }`.
    pub const SEND_QUEUED_MESSAGE_NOW: &str = "SendQueuedMessageNow";
    /// Steer this row into the live turn without interrupting it.
    /// `{ chatId, id }` → `{ sent }`.
    pub const STEER_QUEUED_MESSAGE_NOW: &str = "SteerQueuedMessageNow";
    /// Nudge every open room client to verify liveness NOW (window focus,
    /// app foregrounded). No params; IPC-only. Each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so this is cheap to spam.
    pub const PROBE_SYNC: &str = "ProbeSync";
    /// Live sync introspection (`godspeed sync` / debug surfaces): per-room
    /// connection state, last pushed-frame/ack ages, rejoin/probe/resync
    /// counters for the workspace room and every open chat doc. No params;
    /// IPC-only.
    pub const SYNC_STATUS: &str = "SyncStatus";
    /// Pushed edge-connectivity posture (`zeron_proto::Connectivity`):
    /// current value first, then every change — the connection pill /
    /// composer-honesty / queued-badge feed. No params; IPC-only.
    pub const WATCH_CONNECTIVITY: &str = "WatchConnectivity";
    /// In-flight queued-attachment transfers (`zeron_proto::TransferProgress`
    /// list): current set first, then a fresh snapshot per landed chunk —
    /// the sending thumbnail's percent-ring feed. No params; IPC-only.
    pub const WATCH_TRANSFERS: &str = "WatchTransfers";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_SIDEBAR_PREFERENCES: &str = "WatchSidebarPreferences";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    /// Spaces registry (device+folder pairs) from the workspace doc.
    pub const WATCH_SPACES: &str = "WatchSpaces";
    /// Entity mutations against the workspace doc (feature-inventory §2 DataRpc).
    /// Params are tagged `{op: createChat|createSpace|renameSpace|deleteSpace|
    /// renameChat|setChatArchived|deleteChat|renameDevice|markChatSeen, …}`.
    pub const MUTATE: &str = "Mutate";
    /// This engine's identity → `{deviceId}` (IPC-only; never relay-forwarded —
    /// the answer is about whichever engine you are directly connected to).
    pub const LOCAL_DEVICE: &str = "LocalDevice";
    /// This engine runtime's fixed device and workspace identity.
    pub const ENGINE_INFO: &str = "EngineInfo";
    /// Readiness barrier for the engine runtime. The call completes once stores
    /// and journals are assembled, or fails with the assembly error.
    pub const ENGINE_READY: &str = "EngineReady";
    /// Ask a headless IPC owner to drain its runtime and exit successfully.
    /// Headed IPC owners do not implement this method: closing another app's
    /// engine behind its windows would leave that process unusable.
    pub const STOP_ENGINE: &str = "StopEngine";
    pub const AUTH_STATUS: &str = "AuthStatus";
    // AuthRpc mutations (feature-inventory §2 AuthRpc; IPC-only).
    pub const SIGN_IN: &str = "SignIn";
    pub const SIGN_IN_HEADLESS: &str = "SignInHeadless";
    pub const COMPLETE_SIGN_IN: &str = "CompleteSignIn";
    pub const SIGN_OUT: &str = "SignOut";
    pub const LIST_ORGS: &str = "ListOrgs";
    pub const CREATE_ORG: &str = "CreateOrg";
    pub const SELECT_ORG: &str = "SelectOrg";
    /// One-time local→synced profile import: what's importable (unary).
    pub const LOCAL_IMPORT_STATUS: &str = "LocalImportStatus";
    /// One-time local→synced profile import: run it (stream of progress items).
    pub const IMPORT_LOCAL_WORKSPACE: &str = "ImportLocalWorkspace";
    // Chat import from other agent apps (`zeron_proto::chat_import`). IPC-only:
    // the sources are this device's files and the chats land on this device.
    /// `{}` → `ChatImportPreview[]`, one per source (T3 Code, Claude Code, Codex).
    pub const SCAN_CHAT_IMPORT_SOURCES: &str = "ScanChatImportSources";
    /// `{source}` → `ChatImportPreview`.
    pub const PREVIEW_CHAT_IMPORT: &str = "PreviewChatImport";
    /// `ChatImportRequest` → stream of `ChatImportProgress`, ending in `summary`.
    pub const RUN_CHAT_IMPORT: &str = "RunChatImport";
    // Message search (ControlRpc, relay-forwardable — each engine answers
    // from its own local message index).
    /// `SearchMessagesRequest` → `SearchMessagesResult`.
    pub const SEARCH_MESSAGES: &str = "SearchMessages";
    // Repos / worktrees / folders (ControlRpc, relay-forwardable).
    pub const LIST_REPOS: &str = "ListRepos";
    pub const ADD_REPO: &str = "AddRepo";
    pub const CLONE_REPO: &str = "CloneRepo";
    pub const CREATE_REPO: &str = "CreateRepo";
    pub const LIST_BRANCHES: &str = "ListBranches";
    pub const LIST_REFS: &str = "ListRefs";
    pub const LIST_GIT_HISTORY: &str = "ListGitHistory";
    /// Fuzzy commit-subject / SHA search over the complete public history.
    pub const SEARCH_GIT_HISTORY: &str = "SearchGitHistory";
    /// Resolve hosted profile images for a page of Git commit authors.
    pub const RESOLVE_GIT_AVATARS: &str = "ResolveGitAvatars";
    /// Update remote-tracking refs without changing HEAD, the index, or files.
    pub const FETCH_ALL: &str = "FetchAll";
    pub const SWITCH_REF: &str = "SwitchRef";
    pub const LIST_FOLDERS: &str = "ListFolders";
    pub const GET_WORKTREE_SETTINGS: &str = "GetWorktreeSettings";
    pub const SET_WORKTREE_SETTINGS: &str = "SetWorktreeSettings";
    /// The device's browse roots: home plus mounted drives/volumes.
    pub const LIST_DRIVES: &str = "ListDrives";
    /// Folders under the host's home by name (new-project picker).
    pub const SEARCH_HOME_FOLDERS: &str = "SearchHomeFolders";
    /// The new-project picker closed: drop the host's home index if unpinned.
    pub const RELEASE_HOME_FOLDER_SEARCH: &str = "ReleaseHomeFolderSearch";
    /// Fuzzy relative-path search rooted in a known chat or space checkout.
    pub const SEARCH_FILES: &str = "SearchFiles";
    // Device-local workspace filesystem operations. All are relay-forwardable;
    // WatchWorkspaceFiles is the only streaming method in this group.
    pub const LIST_WORKSPACE_DIRECTORY: &str = "ListWorkspaceDirectory";
    pub const SEARCH_WORKSPACE_FILES: &str = "SearchWorkspaceFiles";
    /// Start or keep alive a workspace's search index (fire-and-forget warm).
    pub const WARM_WORKSPACE_SEARCH: &str = "WarmWorkspaceSearch";
    /// Plain-text search over a workspace's file contents.
    pub const SEARCH_WORKSPACE_CONTENT: &str = "SearchWorkspaceContent";
    pub const READ_WORKSPACE_IMAGE: &str = "ReadWorkspaceImage";
    pub const READ_WORKSPACE_FILE: &str = "ReadWorkspaceFile";
    pub const MOVE_WORKSPACE_ENTRY: &str = "MoveWorkspaceEntry";
    pub const DELETE_WORKSPACE_ENTRY: &str = "DeleteWorkspaceEntry";
    pub const WRITE_WORKSPACE_FILE: &str = "WriteWorkspaceFile";
    pub const WATCH_WORKSPACE_FILES: &str = "WatchWorkspaceFiles";
    pub const CREATE_WORKTREE: &str = "CreateWorktree";
    pub const DELETE_WORKTREE: &str = "DeleteWorktree";
    /// `{repoPath?}` → `OrphanWorktree[]`: Godspeed worktrees no chat uses.
    pub const LIST_ORPHAN_WORKTREES: &str = "ListOrphanWorktrees";
    // Project Actions are private state on the device that owns the project.
    pub const LIST_PROJECT_ACTIONS: &str = "ListProjectActions";
    pub const UPSERT_PROJECT_ACTION: &str = "UpsertProjectAction";
    pub const DELETE_PROJECT_ACTION: &str = "DeleteProjectAction";
    pub const RUN_PROJECT_ACTION: &str = "RunProjectAction";
    // Terminals (ControlRpc, relay-forwardable; SubscribeTerminal streams).
    pub const OPEN_TERMINAL: &str = "OpenTerminal";
    pub const SUBSCRIBE_TERMINAL: &str = "SubscribeTerminal";
    pub const WRITE_TERMINAL: &str = "WriteTerminal";
    pub const RESIZE_TERMINAL: &str = "ResizeTerminal";
    pub const CLOSE_TERMINAL: &str = "CloseTerminal";
    /// Checkout-diff stream for the target device's chats (DataRpc,
    /// relay-forwardable — diffs are produced where the checkout lives).
    pub const WATCH_CHECKOUT_DIFFS: &str = "WatchCheckoutDiffs";
    pub const WATCH_WORKSPACE_GIT_STATUS: &str = "WatchWorkspaceGitStatus";
    /// Current pull request for one checkout, resolved on the checkout's host device.
    pub const WATCH_CHECKOUT_CHANGE_REQUEST: &str = "WatchCheckoutChangeRequest";
    /// One page of a repository's open pull requests (`filter`, `after`
    /// cursor), with the total count.
    pub const LIST_CHANGE_REQUEST_PAGE: &str = "ListChangeRequestPage";
    /// Resolve a selected checkout through local Git metadata only.
    pub const GET_CHANGE_REQUEST_REPOSITORY: &str = "GetChangeRequestRepository";
    pub const GET_CHANGE_REQUEST: &str = "GetChangeRequest";
    pub const POST_CHANGE_REQUEST_COMMENT: &str = "PostChangeRequestComment";
    pub const GET_CHANGE_REQUEST_DIFF: &str = "GetChangeRequestDiff";
    pub const GET_CHECKOUT_DIFF: &str = "GetCheckoutDiff";
    /// Permanently restore one chat-owned checkout to its current HEAD and
    /// remove only its untracked, non-ignored paths.
    pub const DISCARD_WORKING_TREE: &str = "DiscardWorkingTree";
    pub const GET_CHECKOUT_FILE_DIFF_TEXT: &str = "GetCheckoutFileDiffText";
    // Agent accounts (ControlRpc, relay-forwardable — CLI logins are per-device).
    pub const LIST_AGENT_ACCOUNTS: &str = "ListAgentAccounts";
    pub const ACTIVATE_AGENT_ACCOUNT: &str = "ActivateAgentAccount";
    pub const FORGET_AGENT_ACCOUNT: &str = "ForgetAgentAccount";
    pub const START_AGENT_LOGIN: &str = "StartAgentLogin";
    pub const COMPLETE_AGENT_LOGIN: &str = "CompleteAgentLogin";
    pub const POLL_AGENT_LOGIN: &str = "PollAgentLogin";
    pub const CANCEL_AGENT_LOGIN: &str = "CancelAgentLogin";
    // Uploads / attachments (ControlRpc, relay-forwardable — target the chat's host device).
    pub const UPLOAD_CHUNK: &str = "UploadChunk";
    pub const UPLOAD_COMMIT: &str = "UploadCommit";
    pub const READ_ATTACHMENT_CHUNK: &str = "ReadAttachmentChunk";
    /// Store an agent's HTML page and show it inline in that chat's live
    /// turn (`show_visual`). Local only: the calling agent runs on this device.
    pub const PUBLISH_VISUAL: &str = "PublishVisual";
    /// Lazy full-tool-output fetch from the R2 sidecar by doc-resident ref
    /// (chat2-sync A3). Edge-direct from any device — never relay-forwarded.
    pub const FETCH_TOOL_BLOB: &str = "FetchToolBlob";
    // Updates (ControlRpc, relay-forwardable — a device reports/applies its own
    // binary's update). Stream: current UpdateStatus, then every change.
    pub const UPDATE_STATUS: &str = "UpdateStatus";
    /// Download + apply the newest release on the target device (symlink-managed
    /// installs; the service restart is scheduled after the reply flushes).
    pub const APPLY_UPDATE: &str = "ApplyUpdate";
    /// Device-local agent CLI update lifecycle. The stream emits the complete
    /// ordered status list initially and after every transition.
    pub const WATCH_HARNESS_UPDATES: &str = "WatchHarnessUpdates";
    pub const CHECK_HARNESS_UPDATES: &str = "CheckHarnessUpdates";
    pub const APPLY_HARNESS_UPDATE: &str = "ApplyHarnessUpdate";
    pub const CANCEL_HARNESS_UPDATE: &str = "CancelHarnessUpdate";
    pub const DISMISS_HARNESS_UPDATE: &str = "DismissHarnessUpdate";
    pub const SET_HARNESS_UPDATE_POLICY: &str = "SetHarnessUpdatePolicy";
    // Always-on agents (`zeron_proto::agents`). Device-scoped like repos, so
    // relay-forwardable: a laptop manages the agents its VPS engine hosts.
    /// `{}` → `AgentSummary[]`.
    pub const LIST_AGENTS: &str = "ListAgents";
    /// Stream of `AgentsSnapshot`: current first, then one per change.
    pub const WATCH_AGENTS: &str = "WatchAgents";
    /// `{draft}` → `AgentSummary`.
    pub const CREATE_AGENT: &str = "CreateAgent";
    /// `{id, draft}` → `AgentSummary`.
    pub const UPDATE_AGENT: &str = "UpdateAgent";
    /// `{id}` → `{deleted}`. Keeps the agent's chats; drops its inbox rows.
    pub const DELETE_AGENT: &str = "DeleteAgent";
    /// `{id, enabled}` → `AgentSummary`. Enabling also lifts an auto-pause.
    pub const SET_AGENT_ENABLED: &str = "SetAgentEnabled";
    /// `{id}` → `AgentRun` (the new run, or why it was skipped).
    pub const RUN_AGENT_NOW: &str = "RunAgentNow";
    /// `{id}` → `AgentSummary` with a fresh webhook token; the old URL dies.
    pub const ROTATE_AGENT_WEBHOOK: &str = "RotateAgentWebhook";
    /// `AgentRunFilter` → `AgentRun[]`, newest first.
    pub const LIST_AGENT_RUNS: &str = "ListAgentRuns";
    /// `{ids, read?, archived?}` → `{changed}`.
    pub const MARK_AGENT_RUNS: &str = "MarkAgentRuns";

    // Telegram bot for always-on agents (`zeron_proto::telegram`). Device-
    // scoped like the agents, so relay-forwardable. No method ever returns
    // the bot token.
    /// `{}` → `TelegramStatus`.
    pub const GET_TELEGRAM: &str = "GetTelegram";
    /// Stream of `TelegramStatus`: current first, then one per change.
    pub const WATCH_TELEGRAM: &str = "WatchTelegram";
    /// `{token}` → `TelegramStatus`. Saves the token and (re)starts the poller.
    pub const SET_TELEGRAM_TOKEN: &str = "SetTelegramToken";
    /// `{}` → `TelegramStatus`. Forgets the token and stops the poller.
    pub const CLEAR_TELEGRAM_TOKEN: &str = "ClearTelegramToken";
    /// `{}` → `TelegramLinkCode`: a fresh one-time code (the old one dies).
    pub const CREATE_TELEGRAM_LINK_CODE: &str = "CreateTelegramLinkCode";
    /// `{chatId}` → `TelegramStatus`.
    pub const UNLINK_TELEGRAM_CHAT: &str = "UnlinkTelegramChat";
    /// `{defaults: TelegramDefaults}` → `TelegramStatus`. Applies to chats
    /// linked from now on.
    pub const SET_TELEGRAM_DEFAULTS: &str = "SetTelegramDefaults";

    // "Needs you" inbox (`zeron_proto::attention`). Device-scoped like the
    // chats and agent runs it folds together, so relay-forwardable.
    /// `{}` → `AttentionSnapshot`.
    pub const GET_ATTENTION: &str = "GetAttention";
    /// Stream of `AttentionSnapshot`: current first, then one per change.
    pub const WATCH_ATTENTION: &str = "WatchAttention";
    /// `AttentionResponse {itemId, decision}` → `{}`. Approvals only.
    pub const RESPOND_ATTENTION: &str = "RespondAttention";
    /// `{itemIds}` → `BatchApproval`. Approves only items that are low-risk
    /// approvals when the call lands; everything else is skipped.
    pub const APPROVE_LOW_RISK: &str = "ApproveLowRisk";
    /// `{itemId}` → `{dismissed}`: mark a finished, failed or timed-out item seen.
    pub const DISMISS_ATTENTION: &str = "DismissAttention";
    /// `{}` → `QuestionTimeoutSettings`.
    pub const GET_QUESTION_TIMEOUT_SETTINGS: &str = "GetQuestionTimeoutSettings";
    /// `QuestionTimeoutSettings` → `QuestionTimeoutSettings`.
    pub const SET_QUESTION_TIMEOUT_SETTINGS: &str = "SetQuestionTimeoutSettings";
    /// `{chatId, timeout: ChatQuestionTimeout | null}` → `QuestionTimeoutSettings`.
    pub const SET_CHAT_QUESTION_TIMEOUT: &str = "SetChatQuestionTimeout";

    // Usage-limit recovery (`zeron_proto::ChatLimit`). Relay-forwardable: the
    // chat's host device owns the recovery timer and its default.
    /// `{}` → `LimitRecoverySettings`.
    pub const GET_LIMIT_RECOVERY_SETTINGS: &str = "GetLimitRecoverySettings";
    /// `LimitRecoverySettings` → `LimitRecoverySettings`.
    pub const SET_LIMIT_RECOVERY_SETTINGS: &str = "SetLimitRecoverySettings";
    /// `{chatId, action: LimitRecoveryAction, resetAt?}` → `{limit: ChatLimit | null}`.
    pub const SET_CHAT_LIMIT_RECOVERY: &str = "SetChatLimitRecovery";

    // Thread lifecycle (`zeron_proto::ChatLifecycle`). Relay-forwardable: the
    // host device runs the snooze and auto-settle timer for its chats.
    /// `{}` → `ChatLifecycleSettings`.
    pub const GET_CHAT_LIFECYCLE_SETTINGS: &str = "GetChatLifecycleSettings";
    /// `ChatLifecycleSettings` → `ChatLifecycleSettings`.
    pub const SET_CHAT_LIFECYCLE_SETTINGS: &str = "SetChatLifecycleSettings";

    // Git write actions (ControlRpc, relay-forwardable — they run on the
    // device that owns the checkout). Params carry the checkout `cwd`.
    /// Branch, upstream, ahead/behind and remote facts for one checkout.
    pub const GIT_BRANCH_STATUS: &str = "GitBranchStatus";
    /// Stage all changes (or `files`) and commit; an empty message is generated.
    pub const GIT_COMMIT: &str = "GitCommit";
    /// Push the current branch, creating its upstream on first push. Never forces.
    pub const GIT_PUSH: &str = "GitPush";
    /// Fast-forward the current branch to its upstream; divergence is an error.
    pub const GIT_PULL: &str = "GitPull";
    /// Pull (fast-forward) then push.
    pub const GIT_SYNC: &str = "GitSync";
    /// `git init` an existing project folder.
    pub const GIT_INIT: &str = "GitInit";
    pub const GENERATE_COMMIT_MESSAGE: &str = "GenerateCommitMessage";
    pub const GENERATE_PULL_REQUEST: &str = "GeneratePullRequest";
    pub const GENERATE_BRANCH_NAME: &str = "GenerateBranchName";
    /// Push if needed, then `gh pr create` (generated title/body when empty).
    pub const CREATE_PULL_REQUEST: &str = "CreatePullRequest";
    /// `gh repo create --source . --push`; only on an explicit user request.
    pub const PUBLISH_REPOSITORY: &str = "PublishRepository";
}

/// Stable capability error codes transported without provider stderr.
pub mod capability_errors {
    pub const PULL_REQUESTS_CLI_UNAVAILABLE: &str = "pull_requests.cli_unavailable";
    pub const PULL_REQUESTS_AUTHENTICATION: &str = "pull_requests.authentication";
    pub const PULL_REQUESTS_RATE_LIMITED: &str = "pull_requests.rate_limited";
    pub const PULL_REQUESTS_TIMEOUT: &str = "pull_requests.timeout";
    pub const PULL_REQUESTS_DECODE: &str = "pull_requests.decode";
    pub const PULL_REQUESTS_COMMAND_FAILED: &str = "pull_requests.command_failed";
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("bad params: {0}")]
    BadParams(String),
    /// Stable machine-readable capability failure preserved across transports.
    #[error("capability error: {0}")]
    Capability(String),
    #[error("{0}")]
    Failed(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("connection closed")]
    Closed,
}

/// A client-originated frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
}

/// A server-originated frame. Exactly one of `ok` / `err` / `item` / `done` is meaningful.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerFrame {
    pub id: u64,
    /// `"ok": null` is a reply whose value is `null` (for example an absent
    /// `Option`), distinct from a frame without `ok`.
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

fn present_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// What a service returns for one invocation.
pub enum RpcReply {
    /// Unary response — sent as `{id, ok}`.
    Value(serde_json::Value),
    /// Stream — each item sent as `{id, item}`, then `{id, done: true}` when it ends.
    Stream(BoxStream<'static, serde_json::Value>),
}

impl RpcReply {
    /// Serialize a value into a unary reply.
    pub fn value<T: Serialize>(value: &T) -> Result<Self, RpcError> {
        serde_json::to_value(value)
            .map(RpcReply::Value)
            .map_err(|e| RpcError::Failed(format!("serialize response: {e}")))
    }
}

/// Server-side dispatch: one implementation serves every transport.
#[async_trait]
pub trait RpcService: Send + Sync + 'static {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError>;

    /// A plain HTTP request that reached the IPC listener without asking for
    /// a WebSocket upgrade (agent webhooks). Wrappers must forward this like
    /// `handle`; the default answers 404 so nothing is exposed by accident.
    async fn handle_http(&self, _request: HttpRequest) -> HttpResponse {
        HttpResponse::json(404, &serde_json::json!({ "error": "not found" }))
    }
}

/// One HTTP/1.1 request read off the IPC listener. Header names are lowercased.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    /// Request target as sent, query string included.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Always JSON: these endpoints answer machines.
    pub body: String,
}

impl HttpResponse {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            body: body.to_string(),
        }
    }
}

/// Deserialize typed params out of the envelope's `params` value.
pub fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::BadParams(e.to_string()))
}

/// Spawn an in-memory server for `service` and return a connected client.
/// Same envelopes, same dispatch loop as the WebSocket path — the in-process UI
/// transport (ARCHITECTURE §1 "zero serialization shortcuts").
pub fn memory_client(service: Arc<dyn RpcService>) -> RpcClient {
    let (client_out, server_in) = tokio::sync::mpsc::channel::<String>(256);
    let (server_out, client_in) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(serve_connection(service, server_out, server_in));
    RpcClient::new(client_out, client_in)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Mutex;

    struct TestService;

    struct CancelAwareService {
        dropped: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(dropped) = self.0.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[async_trait]
    impl RpcService for CancelAwareService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            if !matches!(
                method,
                methods::WATCH_CHECKOUT_CHANGE_REQUEST | methods::WATCH_HARNESS_UPDATES | "Silent"
            ) {
                return Err(RpcError::UnknownMethod(method.into()));
            }
            let guard = DropSignal(self.dropped.lock().unwrap().take());
            let stream = futures::stream::unfold(guard, |guard| async move {
                let item = std::future::pending::<Option<(serde_json::Value, DropSignal)>>().await;
                drop(guard);
                item
            });
            if method == methods::WATCH_HARNESS_UPDATES {
                // Update watches send an initial status snapshot, then may stay
                // quiet indefinitely. Legacy peers do not send a readiness frame.
                return Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!([]) })
                        .chain(stream)
                        .boxed(),
                ));
            }
            Ok(RpcReply::Stream(stream.boxed()))
        }
    }

    #[async_trait]
    impl RpcService for TestService {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                "Echo" => Ok(RpcReply::Value(params)),
                "Count" => {
                    let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                    Ok(RpcReply::Stream(
                        futures::stream::iter((0..n).map(|i| serde_json::json!(i))).boxed(),
                    ))
                }
                "Never" => Ok(RpcReply::Stream(futures::stream::pending().boxed())),
                "Boom" => Err(RpcError::Failed("boom".into())),
                "Capability" => Err(RpcError::Capability(
                    capability_errors::PULL_REQUESTS_AUTHENTICATION.into(),
                )),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn memory_call_stream_and_error() {
        let client = memory_client(Arc::new(TestService));

        let echoed = client
            .call("Echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 3}))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(v) = items.recv().await {
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                serde_json::json!(0),
                serde_json::json!(1),
                serde_json::json!(2)
            ]
        );

        let err = client
            .call("Boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Failed(m) if m == "boom"));

        let err = client
            .call("Capability", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RpcError::Capability(code)
                if code == capability_errors::PULL_REQUESTS_AUTHENTICATION
        ));
    }

    #[tokio::test]
    async fn checked_stream_acknowledges_support_and_preserves_unknown_method() {
        let client = memory_client(Arc::new(TestService));

        let mut items = client
            .subscribe_checked("Count", serde_json::json!({"n": 1}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, None);

        let error = match client
            .subscribe_checked("FutureStream", serde_json::Value::Null)
            .await
        {
            Ok(_) => panic!("old service must reject unknown stream"),
            Err(error) => error,
        };
        assert!(matches!(error, RpcError::UnknownMethod(method) if method == "FutureStream"));
    }

    #[tokio::test]
    async fn dropping_checked_subscription_cancels_pending_server_stream() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let client = memory_client(Arc::new(CancelAwareService {
            dropped: Mutex::new(Some(dropped_tx)),
        }));
        let stream = client
            .subscribe_checked(
                methods::WATCH_CHECKOUT_CHANGE_REQUEST,
                serde_json::Value::Null,
            )
            .await
            .unwrap();

        drop(stream);

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("server stream cancelled")
            .expect("drop signal");
    }

    #[tokio::test]
    async fn scoped_subscription_returns_before_first_item_and_cancels_silence() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let service = Arc::new(CancelAwareService {
            dropped: Mutex::new(Some(dropped_tx)),
        });
        let client = memory_client(service.clone());
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.subscribe_scoped("Silent", serde_json::Value::Null),
        )
        .await
        .unwrap()
        .unwrap();
        while service.dropped.lock().unwrap().is_some() {
            tokio::task::yield_now().await;
        }
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_one_device_update_watch_preserves_the_other() {
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (second_tx, mut second_rx) = tokio::sync::oneshot::channel();
        let first = memory_client(Arc::new(CancelAwareService {
            dropped: Mutex::new(Some(first_tx)),
        }));
        let second = memory_client(Arc::new(CancelAwareService {
            dropped: Mutex::new(Some(second_tx)),
        }));
        let first_watch = first
            .subscribe_checked(methods::WATCH_HARNESS_UPDATES, serde_json::Value::Null)
            .await
            .unwrap();
        let second_watch = second
            .subscribe_checked(methods::WATCH_HARNESS_UPDATES, serde_json::Value::Null)
            .await
            .unwrap();

        drop(first_watch);
        tokio::time::timeout(std::time::Duration::from_secs(1), first_rx)
            .await
            .expect("first device's quiet stream cancelled")
            .expect("first drop signal");
        assert!(matches!(
            second_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        drop(second_watch);
        tokio::time::timeout(std::time::Duration::from_secs(1), second_rx)
            .await
            .expect("second device's quiet stream cancelled")
            .expect("second drop signal");
    }

    #[tokio::test]
    async fn unary_null_reply_resolves_the_call() {
        let client = memory_client(Arc::new(TestService));
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.call("Echo", serde_json::Value::Null),
        )
        .await
        .expect("a null reply is a reply")
        .unwrap();
        assert_eq!(reply, serde_json::Value::Null);
        let frame: ServerFrame = serde_json::from_str(r#"{"id":1,"ok":null}"#).unwrap();
        assert_eq!(frame.ok, Some(serde_json::Value::Null));
        let frame: ServerFrame = serde_json::from_str(r#"{"id":1,"done":true}"#).unwrap();
        assert_eq!(frame.ok, None);
    }

    #[tokio::test]
    async fn websocket_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));

        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client
            .call("Echo", serde_json::json!("hello"))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!("hello"));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, Some(serde_json::json!(1)));
        assert_eq!(items.recv().await, None);
    }

    #[tokio::test]
    async fn handshake_with_origin_header_is_rejected() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));

        // A browser page opening ws://127.0.0.1:{port} always sends Origin;
        // the server must refuse the handshake before serving any RPC.
        let mut req = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("origin", "https://evil.example".parse().unwrap());
        let result = tokio_tungstenite::connect_async(req).await;
        assert!(
            result.is_err(),
            "handshake carrying an Origin header must be rejected"
        );

        // A native viewport (no Origin) still connects and can call RPC — the
        // reject must not be a blanket denial.
        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client.call("Echo", serde_json::json!("ok")).await.unwrap();
        assert_eq!(echoed, serde_json::json!("ok"));
    }

    struct HttpEcho;

    #[async_trait]
    impl RpcService for HttpEcho {
        async fn handle(&self, _: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
            Ok(RpcReply::Value(params))
        }
        async fn handle_http(&self, request: HttpRequest) -> HttpResponse {
            HttpResponse::json(
                202,
                &serde_json::json!({
                    "method": request.method,
                    "path": request.path,
                    "event": request.header("X-Event"),
                    "body": String::from_utf8_lossy(&request.body),
                }),
            )
        }
    }

    async fn raw_http(port: u16, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn plain_http_shares_the_port_with_websockets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(HttpEcho)));

        let body = r#"{"a":1}"#;
        let response = raw_http(
            port,
            &format!(
                "POST /hooks/x?y=1 HTTP/1.1\r\nHost: localhost\r\nX-Event: push\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(
            response.starts_with("HTTP/1.1 202 Accepted\r\n"),
            "{response}"
        );
        let json: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(json["method"], "POST");
        assert_eq!(json["path"], "/hooks/x?y=1");
        assert_eq!(json["event"], "push");
        assert_eq!(json["body"], body);

        let response = raw_http(
            port,
            "POST /hooks/x HTTP/1.1\r\nOrigin: https://evil.example\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");

        // The default service answers 404, and WebSocket RPC is unaffected.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plain = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));
        let response = raw_http(plain, "GET / HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client.call("Echo", serde_json::json!("ok")).await.unwrap();
        assert_eq!(echoed, serde_json::json!("ok"));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_cancels_server_side() {
        let client = memory_client(Arc::new(TestService));
        let items = client
            .subscribe("Never", serde_json::Value::Null)
            .await
            .unwrap();
        drop(items);
        // The next unary call still works — the dead stream didn't wedge the connection.
        let echoed = client.call("Echo", serde_json::json!(2)).await.unwrap();
        assert_eq!(echoed, serde_json::json!(2));
    }
}
