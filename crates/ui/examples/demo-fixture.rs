//! README demo recording: the production shell with invented demo data
//! (no engine, no agents, no messages sent), driven by timers through a chat
//! with its receipt, Mission Control and the Inbox, while the window renders
//! itself to PNG frames.
//!
//! The window never takes focus: the app runs as an accessory (no Dock icon,
//! never activates) and the window stays hidden unless GODSPEED_DEMO_SHOW is
//! set, in which case it is ordered behind every other window. Frames come
//! from gpui's own renderer, so nothing outside the window is captured and no
//! Screen Recording permission is needed.
//!
//! `cargo run -p zeron-ui --example demo-fixture --features demo-fixture -- <frames dir>`
//! writes `frame_NNNNN.png` (premultiplied RGBA, device pixels) plus
//! `frames.txt`, an ffmpeg concat list with each frame's real duration. The
//! frosted window is translucent: composite it over a backdrop with
//! premultiplied alpha, e.g.
//! `ffmpeg -f concat -i frames.txt -loop 1 -i backdrop.png -filter_complex
//! "[0:v]scale=1200:-1:flags=lanczos,format=rgba[f];[1:v][f]overlay=alpha=premultiplied:shortest=1"`.
//! Capture on an idle machine: frames render in real time.
//!
//! A second argument `promo` records the release-promo script instead (a
//! receipt, Mission Control, the Inbox, an agent driving the preview, and a
//! zoomie made from Home) in a 1280×800 window, and writes
//! `scenes.txt` (`<scene> <first frame>`) so each beat can be cut out.
//! The preview page itself is a native web view, which the window's own
//! renderer does not draw: serve `GODSPEED_DEMO_PREVIEW_URL` (default
//! `http://localhost:5173/pricing`) so the tab loads, and composite the
//! page over the blank area afterwards.
//!
//! `stills` instead writes one named PNG per feature (the session card's
//! Usage rows, PR review and merge with its watch, More agents from the ACP
//! registry, saved servers, sidebar multi-select) for promo stills.
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use serde_json::json;
use std::{
    io::Write as _,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use zeron_ui::*;

const WIDTH: f32 = 1440.0;
const HEIGHT: f32 = 900.0;
const FRAME: Duration = Duration::from_millis(66);

async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

/// Hide the process from the Dock and app switcher so nothing can activate it.
#[cfg(target_os = "macos")]
fn become_accessory() {
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        // NSApplicationActivationPolicyAccessory
        let _: bool = msg_send![app, setActivationPolicy: 1_i64];
    }
}

/// Show the window behind everything else, never as key.
#[cfg(target_os = "macos")]
fn order_back(window: &gpui::Window) {
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    if let RawWindowHandle::AppKit(handle) = handle.as_raw() {
        unsafe {
            let view = handle.ns_view.as_ptr() as *mut objc::runtime::Object;
            let ns_window: *mut objc::runtime::Object = msg_send![view, window];
            let nil: *mut objc::runtime::Object = std::ptr::null_mut();
            let _: () = msg_send![ns_window, orderBack: nil];
        }
    }
}

fn chat(
    id: &str,
    device: &str,
    space: &str,
    title: &str,
    harness: &str,
    model: &str,
    branch: &str,
    preview: &str,
    minutes_ago: i64,
    seen: bool,
) -> zeron_proto::Chat {
    let now = chrono::Utc::now();
    let last = now - chrono::Duration::seconds(minutes_ago * 60 + 20);
    let mut chat: zeron_proto::Chat = serde_json::from_value(json!({
        "id": id, "deviceId": device, "spaceId": space, "title": title, "archived": false,
        "cwd": null, "branch": branch, "checkoutId": null,
        "config": {"harness": harness, "model": model, "reasoning": null, "sandbox": "workspace-write"},
        "lastMessagePreview": preview, "lastMessageAt": last,
        "createdAt": now - chrono::Duration::hours(3) - chrono::Duration::minutes(minutes_ago),
    }))
    .unwrap();
    chat.last_seen_at = if seen {
        Some(last)
    } else {
        Some(last - chrono::Duration::minutes(5))
    };
    chat
}

fn session(
    chat: &str,
    device: &str,
    status: &str,
    minutes: i64,
    subagents: u32,
    approval: bool,
) -> zeron_proto::Session {
    let now = chrono::Utc::now();
    serde_json::from_value(json!({
        "chatId": chat, "deviceId": device, "status": status,
        "startedAt": now - chrono::Duration::seconds(minutes * 60 + 14),
        "updatedAt": now, "runningSubagents": subagents, "awaitingApproval": approval,
    }))
    .unwrap()
}

/// The chat on screen: one finished turn, closed by its receipt.
fn transcript(device: &str) -> serde_json::Value {
    let start = chrono::Utc::now().timestamp_millis() - 5 * 60_000;
    let tool = |id: &str,
                call: serde_json::Value,
                output: Option<&str>,
                stats: Option<serde_json::Value>| {
        let mut part = json!({"id": id, "kind": "tool", "call": call, "resolved": true});
        if let Some(output) = output {
            part["output"] = json!(output);
        }
        if let Some(stats) = stats {
            part["diffStats"] = stats;
        }
        part
    };
    let files = json!([
        {"path": "src/editor/theme.ts", "additions": 48, "deletions": 12},
        {"path": "src/editor/tokens.dark.ts", "additions": 64, "deletions": 0},
        {"path": "src/editor/Editor.tsx", "additions": 9, "deletions": 3},
        {"path": "src/editor/theme.test.ts", "additions": 31, "deletions": 0},
    ]);
    let earlier = start - 9 * 60_000;
    json!([
        {"id": "u0", "role": "user", "createdAt": earlier, "deviceId": device, "parts": [
            {"id": "u0t", "kind": "text", "text": "Where does the editor get its colors from?"}
        ]},
        {"id": "a0", "role": "assistant", "createdAt": earlier + 2_000, "deviceId": device, "status": "complete",
         "durationMs": 38_000,
         "turnStats": {"harness": "claude-code", "model": "claude-opus-5-5", "effort": "high",
            "usage": {"inputTokens": 12_840, "outputTokens": 1_102, "cacheReadTokens": 310_200}},
         "parts": [
            tool("a0s", json!({"kind": "search", "pattern": "createTheme", "path": "src/editor"}), Some("3 matches in 2 files"), None),
            tool("a0r", json!({"kind": "readFile", "path": "src/editor/theme.ts"}), None, None),
            {"id": "a0t", "kind": "text", "text": "From `src/editor/theme.ts`. It builds one light token set at startup and never looks at the app appearance again, so the editor stays light after you switch to dark."}
         ]},
        {"id": "u1", "role": "user", "createdAt": start, "deviceId": device, "parts": [
            {"id": "u1t", "kind": "text", "text": "The editor still renders light inside the dark theme. Make it follow the app appearance and keep syntax colors readable."}
        ]},
        {"id": "a1", "role": "assistant", "createdAt": start + 2_000, "deviceId": device, "status": "complete",
         "durationMs": 252_000,
         "turnStats": {"harness": "claude-code", "model": "claude-opus-5-5", "effort": "high",
            "usage": {"inputTokens": 48_210, "outputTokens": 9_634, "cacheReadTokens": 1_203_400},
            "files": files},
         "parts": [
            {"id": "a1t0", "kind": "text", "text": "I'll find where the editor picks its colors, then make it follow the app appearance."},
            tool("a1s", json!({"kind": "search", "pattern": "editorTheme", "path": "src"}), Some("6 matches in 3 files"), None),
            tool("a1r", json!({"kind": "readFile", "path": "src/editor/theme.ts"}), None, None),
            tool("a1e1", json!({"kind": "editFile", "path": "src/editor/theme.ts"}), None,
                Some(json!([{"path": "src/editor/theme.ts", "additions": 48, "deletions": 12}]))),
            tool("a1w", json!({"kind": "writeFile", "path": "src/editor/tokens.dark.ts"}), None,
                Some(json!([{"path": "src/editor/tokens.dark.ts", "additions": 64, "deletions": 0}]))),
            tool("a1e2", json!({"kind": "editFile", "path": "src/editor/Editor.tsx"}), None,
                Some(json!([{"path": "src/editor/Editor.tsx", "additions": 9, "deletions": 3}]))),
            tool("a1e3", json!({"kind": "writeFile", "path": "src/editor/theme.test.ts"}), None,
                Some(json!([{"path": "src/editor/theme.test.ts", "additions": 31, "deletions": 0}]))),
            tool("a1x1", json!({"kind": "exec", "command": "pnpm test editor"}), Some("Tests  41 passed (41)"), None),
            tool("a1x2", json!({"kind": "exec", "command": "pnpm typecheck"}), Some("Found 0 errors."), None),
            {"id": "a1t1", "kind": "text", "text": "The editor now follows the app appearance, live, without a reload.\n\n- `theme.ts` reads the appearance and swaps token sets when it changes\n- the new `tokens.dark.ts` keeps every syntax color at 4.5:1 contrast or better\n- `Editor.tsx` re-themes open buffers in place, so the cursor and scroll position stay put\n\nAll 41 editor tests pass, including three new ones for the switch, and the typecheck is clean."}
         ]}
    ])
}

fn seed(state: &mut state::AppState, project: &std::path::Path) {
    let now = chrono::Utc::now();
    state.connection = zeron_proto::view::ConnectionStatus::Ready;
    state.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
    state.local_device_id = Some("mbp".into());
    state.devices = vec![
        serde_json::from_value(json!({"id": "mbp", "name": "MacBook Pro", "platform": "macos", "lastSeenAt": now, "capabilities": ["project-transfer-v1"]})).unwrap(),
        serde_json::from_value(json!({"id": "studio", "name": "Studio server", "platform": "linux", "lastSeenAt": now, "capabilities": ["project-transfer-v1"]})).unwrap(),
    ];
    let space = |id: &str, device: &str, path: String, name: Option<&str>| -> zeron_proto::Space {
        serde_json::from_value(json!({
            "id": id, "deviceId": device, "path": path, "name": name, "gitDetected": true,
            "checkoutId": format!("checkout-{id}"), "repositoryId": format!("github.com/fieldnotes-app/{id}"),
            "createdAt": now - chrono::Duration::days(20),
        }))
        .unwrap()
    };
    state.spaces = vec![
        space(
            "fieldnotes",
            "mbp",
            project.join("fieldnotes").to_string_lossy().into_owned(),
            None,
        ),
        space("api", "studio", "/srv/fieldnotes-api".into(), None),
    ];
    state.chats = vec![
        chat(
            "search",
            "mbp",
            "fieldnotes",
            "Add search to notes",
            "claude-code",
            "claude-opus-5-5",
            "feat/search",
            "Search is in. Ready to push feat/search",
            3,
            true,
        ),
        chat(
            "sync",
            "mbp",
            "fieldnotes",
            "Fix sync on reconnect",
            "codex",
            "gpt-5.6-sol",
            "fix/sync-conflicts",
            "Replaying the offline queue against the merge fixtures",
            0,
            true,
        ),
        chat(
            "dark",
            "mbp",
            "fieldnotes",
            "Ship editor dark mode",
            "claude-code",
            "claude-opus-5-5",
            "feat/dark-editor",
            "The editor now follows the app appearance, live, without a reload.",
            1,
            true,
        ),
        chat(
            "pricing",
            "mbp",
            "fieldnotes",
            "Fix the yearly pricing toggle",
            "claude-code",
            "claude-opus-5-5",
            "fix/pricing-toggle",
            "Checking the toggle in the preview",
            0,
            true,
        ),
        chat(
            "astro",
            "mbp",
            "fieldnotes",
            "Move docs to Astro 5",
            "cursor",
            "claude-sonnet-5",
            "chore/astro-5",
            "Converting 42 pages to content collections",
            0,
            true,
        ),
        chat(
            "profile",
            "studio",
            "api",
            "Speed up /search",
            "claude-code",
            "claude-opus-5-5",
            "perf/search-index",
            "EXPLAIN shows a seq scan on notes_fts; adding a GIN index",
            0,
            true,
        ),
        chat(
            "embed",
            "studio",
            "api",
            "Backfill embeddings",
            "codex",
            "gpt-5.6-sol",
            "jobs/embeddings",
            "Batch 18 of 40 embedded, about 6 minutes left",
            0,
            true,
        ),
        chat(
            "limits",
            "studio",
            "api",
            "Add API rate limits",
            "codex",
            "gpt-5.6-sol",
            "feat/rate-limits",
            "Limiter is wired; the load test is next",
            34,
            true,
        ),
        chat(
            "audit",
            "studio",
            "api",
            "Dependency audit",
            "claude-code",
            "claude-sonnet-5",
            "main",
            "No vulnerable packages. Three minor updates available",
            47,
            false,
        ),
    ];
    let limits = state.chats.iter_mut().find(|c| c.id == "limits").unwrap();
    limits.limit = Some(
        serde_json::from_value(json!({
            "limitedAt": now - chrono::Duration::minutes(34),
            "resetAt": now + chrono::Duration::minutes(72),
            "resetSource": "provider", "window": "5-hour", "recovery": "resumeAtReset",
        }))
        .unwrap(),
    );
    // "Add search to notes" runs in its own worktree (worktree environment beat).
    let worktree = project
        .join("fieldnotes-worktrees/feat-search")
        .to_string_lossy()
        .into_owned();
    let search = state.chats.iter_mut().find(|c| c.id == "search").unwrap();
    search.cwd = Some(worktree.clone());
    search.source_context = Some(zeron_proto::ConversationSourceContext {
        checkout_id: "fieldnotes-feat-search".into(),
        repo_root: project.join("fieldnotes").to_string_lossy().into_owned(),
        cwd: worktree,
        branch: "feat/search".into(),
        head_sha: None,
        observed_at: now,
    });
    // A conductor: one main chat steering three sub-agents.
    let mut main = chat(
        "launch",
        "mbp",
        "fieldnotes",
        "Ship shared notebooks",
        "claude-code",
        "claude-opus-5-5",
        "feat/sharing",
        "Three sub-agents are on it; I'll merge when the tests pass",
        0,
        true,
    );
    main.created_at = now - chrono::Duration::minutes(18);
    state.chats.push(main);
    for (i, (id, title, harness, model, preview)) in [
        (
            "launch-api",
            "Sharing API and permissions",
            "codex",
            "gpt-5.6-sol",
            "Added share links with read and edit roles",
        ),
        (
            "launch-ui",
            "Share sheet UI",
            "claude-code",
            "claude-opus-5-5",
            "Building the share sheet with the invite list",
        ),
        (
            "launch-tests",
            "End-to-end tests for sharing",
            "cursor",
            "claude-sonnet-5",
            "12 of 14 sharing tests pass; fixing the invite expiry",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut child = chat(
            id,
            "mbp",
            "fieldnotes",
            title,
            harness,
            model,
            "feat/sharing",
            preview,
            0,
            true,
        );
        child.parent_chat_id = Some("launch".into());
        child.created_at = now - chrono::Duration::minutes(15 - i as i64);
        state.chats.push(child);
    }
    state.conductors = vec![serde_json::from_value(json!({
        "id": "conductor-sharing", "name": "Shared notebooks",
        "folder": project.join("fieldnotes").to_string_lossy(), "deviceId": "mbp",
        "mainChatId": "launch", "spaceIds": ["fieldnotes"], "createdAt": now - chrono::Duration::minutes(18),
    })).unwrap()];
    refresh_sessions(state);
    let approval: zeron_proto::ApprovalRequest = serde_json::from_value(json!({
        "kind": "command", "title": "Run git push origin feat/search",
        "command": "git push origin feat/search", "cwd": "~/code/fieldnotes",
        "reason": "Push the search branch so the pull request can open",
        "decisions": [{"decision": "approve", "label": "Allow once"}, {"decision": "approveForSession", "label": "Allow for session"}, {"decision": "decline", "label": "Deny"}],
    }))
    .unwrap_or_else(|_| {
        let mut request = zeron_proto::ApprovalRequest::new(zeron_proto::ApprovalKind::Command, "Run git push origin feat/search");
        request.command = Some("git push origin feat/search".into());
        request.cwd = Some("~/code/fieldnotes".into());
        request.reason = Some("Push the search branch so the pull request can open".into());
        request
    });
    let item = |id: &str,
                kind: &str,
                chat: &str,
                title: &str,
                project: &str,
                space: &str,
                harness: &str,
                minutes: i64,
                summary: &str,
                actions: serde_json::Value|
     -> zeron_proto::attention::AttentionItem {
        serde_json::from_value(json!({
            "id": id, "kind": kind, "chatId": chat, "title": title, "spaceId": space, "project": project,
            "harness": harness, "createdAt": now - chrono::Duration::seconds(minutes * 60 + 20),
            "summary": summary, "actions": actions,
        }))
        .unwrap()
    };
    let mut push = item(
        "approval:search:push",
        "approval",
        "search",
        "Add search to notes",
        "fieldnotes",
        "fieldnotes",
        "claude-code",
        3,
        "git push origin feat/search",
        json!(["approve", "decline", "open"]),
    );
    push.risk = Some(zeron_proto::ApprovalRisk::Medium);
    push.request_id = Some("push".into());
    push.approval = Some(approval);
    state.attention = zeron_proto::attention::AttentionSnapshot {
        device_id: "mbp".into(),
        items: vec![
            push,
            item(
                "chat:limits",
                "limited",
                "limits",
                "Add API rate limits",
                "fieldnotes-api",
                "api",
                "codex",
                34,
                "5-hour limit, resumes on its own in 1h 12m",
                json!(["open", "dismiss"]),
            ),
            item(
                "chat:audit",
                "finished",
                "audit",
                "Dependency audit",
                "fieldnotes-api",
                "api",
                "claude-code",
                47,
                "No vulnerable packages. Three minor updates available",
                json!(["open", "dismiss"]),
            ),
        ],
    };
    state.selected_chat = Some("dark".into());
    state.selected_space = Some("fieldnotes".into());
    state.auto_selected = true;
    state.chats_synced = true;
    state.spaces_synced = true;
}

/// Fresh heartbeats, so every live session stays live for the recording.
fn refresh_sessions(state: &mut state::AppState) {
    let started = |chat: &str| {
        state
            .sessions
            .iter()
            .find(|s| s.chat_id == chat)
            .and_then(|s| s.started_at)
    };
    let mut sessions = vec![
        session("sync", "mbp", "working", 12, 2, false),
        session("pricing", "mbp", "working", 2, 0, false),
        session("launch", "mbp", "working", 18, 3, false),
        session("launch-api", "mbp", "working", 15, 0, false),
        session("launch-ui", "mbp", "working", 14, 0, false),
        session("launch-tests", "mbp", "working", 13, 0, false),
        session("astro", "mbp", "working", 4, 0, false),
        session("profile", "studio", "working", 27, 3, false),
        session("embed", "studio", "working", 41, 1, false),
        session("search", "mbp", "awaitingInput", 9, 0, true),
    ];
    for session in &mut sessions {
        if let Some(at) = started(&session.chat_id) {
            session.started_at = Some(at);
        }
    }
    let now = chrono::Utc::now();
    for device in &mut state.devices {
        device.last_seen_at = Some(now);
    }
    state.sessions = sessions;
}

/// "Add search to notes": the turn stops on a push that needs approval.
fn search_transcript(device: &str) -> serde_json::Value {
    let start = chrono::Utc::now().timestamp_millis() - 9 * 60_000;
    json!([
        {"id": "su0", "role": "user", "createdAt": start, "deviceId": device, "parts": [
            {"id": "su0t", "kind": "text", "text": "Add full-text search to notes, then push the branch so I can open a PR."}
        ]},
        {"id": "sa0", "role": "assistant", "createdAt": start + 2_000, "deviceId": device, "status": "streaming",
         "parts": [
            {"id": "sa0r", "kind": "tool", "resolved": true, "call": {"kind": "search", "pattern": "notes_fts", "path": "src"}, "output": "2 matches in 1 file"},
            {"id": "sa0e", "kind": "tool", "resolved": true, "call": {"kind": "editFile", "path": "src/search/index.ts"},
             "diffStats": [{"path": "src/search/index.ts", "additions": 96, "deletions": 4}]},
            {"id": "sa0x", "kind": "tool", "resolved": true, "call": {"kind": "exec", "command": "pnpm test search"}, "output": "Tests  18 passed (18)"},
            {"id": "sa0t", "kind": "text", "text": "Search is in and all 18 search tests pass. Pushing `feat/search` next."},
            {"id": "sa0q", "kind": "input", "requestId": "push", "resolved": false, "questions": [{
                "id": "push", "header": "Approval", "question": "Run git push origin feat/search",
                "options": [],
                "approval": {
                    "kind": "command", "title": "Run git push origin feat/search",
                    "command": "git push origin feat/search", "cwd": "~/code/fieldnotes",
                    "reason": "Push the search branch so the pull request can open",
                    "decisions": [{"decision": "approve", "label": "Allow once"}, {"decision": "approveForSession", "label": "Allow for session"}, {"decision": "decline", "label": "Deny"}]
                }
            }]}
         ]}
    ])
}

/// The conductor's main chat: the plan, then its sub-agents' cards.
fn launch_transcript(device: &str) -> serde_json::Value {
    let start = chrono::Utc::now().timestamp_millis() - 18 * 60_000;
    json!([
        {"id": "lu0", "role": "user", "createdAt": start, "deviceId": device, "parts": [
            {"id": "lu0t", "kind": "text", "text": "Ship shared notebooks: the API, the share sheet and end-to-end tests. Split it up and keep me posted."}
        ]},
        {"id": "la0", "role": "assistant", "createdAt": start + 2_000, "deviceId": device, "status": "streaming",
         "parts": [
            {"id": "la0t", "kind": "text", "text": "Splitting this three ways so they run in parallel:\n\n- **API**: share links with read and edit roles\n- **UI**: the share sheet and invite list\n- **Tests**: end-to-end coverage for both\n\nI'll review each branch and merge when the tests pass."}
         ]}
    ])
}

/// Plan windows the composer's tachometer and Settings → Usage show.
fn accounts() -> zeron_proto::AgentAccountsSnapshot {
    let now = chrono::Utc::now();
    let window = |label: &str, used: f32, minutes: i64| {
        json!({
            "label": label, "usedFraction": used, "resetsAt": now + chrono::Duration::minutes(minutes),
        })
    };
    serde_json::from_value(json!({
        "accounts": [
            {"id": "claude-1", "harness": "claude-code", "email": "dev@example.com", "planLabel": "Max 20x",
             "active": true, "usageWindows": [window("Session", 0.64, 134), window("Week", 0.38, 4560)],
             "usageFetchedAt": now.timestamp_millis() - 120_000, "authKind": "oauth", "switchable": true},
            {"id": "codex-1", "harness": "codex", "email": "dev@example.com", "planLabel": "Pro",
             "active": true, "usageWindows": [window("5h", 0.22, 180), window("Week", 0.81, 1800)],
             "usageFetchedAt": now.timestamp_millis() - 120_000, "authKind": "oauth", "switchable": true},
        ],
        "warnings": [],
    }))
    .unwrap()
}

/// A month of usage, deterministic (the iOS demo host's numbers).
fn usage_history() -> zeron_proto::UsageHistory {
    use zeron_proto::{
        UsageAmount, UsageBucket, UsageBucketSize, UsagePoint, UsageRange, UsageSlice,
        UsageSources, UsageTokens,
    };
    let now = chrono::Utc::now();
    let count = 30;
    let start = now - chrono::Duration::days(count as i64 - 1);
    let amount = |seed: u64, scale: f64| {
        let wave = ((seed * 2_654_435_761) % 1000) as f64 / 1000.0;
        let output = ((40_000.0 + 160_000.0 * wave) * scale) as u64;
        let input = output / 3;
        let cache_read = output * 9;
        let cost = output as f64 * 25e-6 + input as f64 * 5e-6 + cache_read as f64 * 0.5e-6;
        UsageAmount {
            tokens: UsageTokens {
                input,
                output,
                cache_read,
                cache_write: output / 4,
            },
            cost_usd: (cost * 100.0).round() / 100.0,
            unpriced_tokens: 0,
            cache_savings_usd: cache_read as f64 * 4.5e-6,
            turns: 3 + seed % 9,
            calls: 0,
        }
    };
    let mut total = UsageAmount::default();
    let mut claude = UsageAmount::default();
    let mut codex = UsageAmount::default();
    let buckets = (0..count)
        .map(|i| {
            let at = start + chrono::Duration::days(i as i64);
            let a = amount(i as u64 + 1, 1.0);
            let b = amount(i as u64 + 101, 0.4);
            total.add(&a);
            total.add(&b);
            claude.add(&a);
            codex.add(&b);
            UsageBucket {
                start: at,
                day: at.date_naive(),
                hour: None,
                points: vec![
                    UsagePoint {
                        provider: "claude-code".into(),
                        model: "claude-opus-5-5".into(),
                        amount: a,
                    },
                    UsagePoint {
                        provider: "codex".into(),
                        model: "gpt-5.6-sol".into(),
                        amount: b,
                    },
                ],
            }
        })
        .collect();
    let slice = |key: &str, label: &str, amount: UsageAmount| UsageSlice {
        key: key.into(),
        label: label.into(),
        amount,
    };
    zeron_proto::UsageHistory {
        range: UsageRange::Month,
        bucket: UsageBucketSize::Day,
        time_zone: "UTC".into(),
        start: Some(start),
        end: Some(now),
        buckets,
        total,
        sessions: 64,
        by_model: vec![
            slice("claude-opus-5-5", "Claude Opus 5.5", claude),
            slice("gpt-5.6-sol", "GPT-5.6 Sol", codex),
        ],
        by_provider: vec![
            slice("claude-code", "Claude Code", claude),
            slice("codex", "Codex", codex),
        ],
        by_project: Vec::new(),
        top_chats: Vec::new(),
        sources: UsageSources {
            godspeed_turns: 412,
            claude_files: 88,
            codex_files: 31,
            deduplicated: 0,
            indexing: false,
        },
        prices_checked: "2026-10-18".into(),
    }
}

/// The pull request beside "Add search to notes", approved and green.
fn pull_request() -> (
    zeron_proto::ChangeRequestDetail,
    zeron_proto::pull_request_actions::ChangeRequestMergeOptions,
) {
    let detail = serde_json::from_value(json!({
        "title": "Add full-text search to notes",
        "body": "Adds a `notes_fts` index and a search box in the sidebar.\n\n- GIN index on title and body\n- debounced search as you type\n- 18 new tests",
        "url": "https://github.com/fieldnotes-app/fieldnotes/pull/214",
        "number": 214, "author": {"login": "maya-builds"},
        "baseRefName": "main", "baseRefOid": "4f1c2a9be0d8c7a6b5f4e3d2c1b0a9f8e7d6c5b4",
        "headRefName": "feat/search", "headRefOid": "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b",
        "ci": {"state": "success", "totalCount": 3},
        "viewerDidAuthor": false, "state": "OPEN", "isDraft": false,
        "reviewDecision": "APPROVED", "mergeable": "MERGEABLE",
        "additions": 412, "deletions": 37,
        "reviews": [{"id": "r1", "author": {"login": "sam-reviews"}, "state": "APPROVED",
                     "body": "Fast and tidy. Ship it.", "createdAt": "2026-10-28T09:12:00Z", "submittedAt": "2026-10-28T09:12:00Z"}],
        "reviewThreads": [],
        "files": [
            {"path": "src/search/index.ts", "additions": 96, "deletions": 4},
            {"path": "src/search/SearchBox.tsx", "additions": 141, "deletions": 0},
            {"path": "db/migrations/0042_notes_fts.sql", "additions": 22, "deletions": 0},
            {"path": "src/search/search.test.ts", "additions": 153, "deletions": 33},
        ],
        "statusCheckRollup": [
            {"name": "build", "conclusion": "SUCCESS", "status": "COMPLETED"},
            {"name": "test", "conclusion": "SUCCESS", "status": "COMPLETED"},
            {"name": "e2e", "conclusion": "SUCCESS", "status": "COMPLETED"},
        ],
        "viewerLogin": "fieldnotes-dev",
        "watch": {"url": "https://github.com/fieldnotes-app/fieldnotes/pull/214", "chatId": "search",
                  "enabled": true, "autoMerge": true, "status": "watching",
                  "lastEvent": "Checks passed: build, test, e2e"},
    }))
    .unwrap();
    let options = serde_json::from_value(json!({
        "allowedMethods": ["squash", "merge", "rebase"], "autoMergeAllowed": true,
        "autoMergeMethod": null, "viewerPermission": "ADMIN",
    }))
    .unwrap();
    (detail, options)
}

/// What a scan of this Mac finds in the other apps' histories.
fn import_previews(project: &std::path::Path) -> Vec<zeron_proto::chat_import::ChatImportPreview> {
    let now = chrono::Utc::now();
    let titles = [
        "Fix flaky sync test",
        "Add tags to notes",
        "Dark mode for the editor",
        "Speed up cold start",
        "Migrate to Vite 7",
        "Offline queue retries",
        "Note templates",
        "Export to Markdown",
        "Keyboard shortcuts sheet",
        "Trim bundle size",
        "Login with passkeys",
        "Paste images into notes",
    ];
    let mut out = Vec::new();
    for (k, (source, location, harness, count, imported)) in [
        ("t3", "~/.t3", "claude-code", 12, 0),
        ("claude", "~/.claude/projects", "claude-code", 9, 2),
        ("codex", "~/.codex/sessions", "codex", 7, 0),
        (
            "cursor",
            "~/Library/Application Support/Cursor",
            "cursor",
            5,
            0,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let chats: Vec<_> = (0..count)
            .map(|i| json!({
                "sourceKey": format!("{source}:{i}"), "title": titles[(i + k * 3) % titles.len()],
                "createdAt": now - chrono::Duration::days(40 - i as i64 * 3),
                "updatedAt": now - chrono::Duration::days(38 - i as i64 * 3),
                "harness": harness, "resumable": true,
                "state": if i < imported { "imported" } else { "new" },
            }))
            .collect();
        out.push(serde_json::from_value(json!({
            "source": source, "found": true, "location": location,
            "projects": [{"path": project.join("fieldnotes").to_string_lossy(), "title": "fieldnotes", "hasSpace": true, "chats": chats}],
            "warnings": [],
        })).unwrap());
    }
    out
}

/// The preview beat's chat: an agent checking its fix in the preview.
fn pricing_transcript(device: &str) -> serde_json::Value {
    let start = chrono::Utc::now().timestamp_millis() - 2 * 60_000;
    let tool = |id: &str, call: serde_json::Value, output: Option<&str>| {
        let mut part = json!({"id": id, "kind": "tool", "call": call, "resolved": true});
        if let Some(output) = output {
            part["output"] = json!(output);
        }
        part
    };
    let preview = |id: &str, tool_name: &str, input: serde_json::Value, output: &str| {
        tool(
            id,
            json!({"kind": "mcp", "server": "godspeed", "tool": tool_name, "input": input}),
            Some(output),
        )
    };
    json!([
        {"id": "pu0", "role": "user", "createdAt": start, "deviceId": device, "parts": [
            {"id": "pu0t", "kind": "text", "text": "The Yearly toggle on /pricing doesn't change the prices. Fix it, then check it in the preview."}
        ]},
        {"id": "pa0", "role": "assistant", "createdAt": start + 2_000, "deviceId": device, "status": "streaming",
         "parts": [
            tool("pa0s", json!({"kind": "search", "pattern": "billingPeriod", "path": "src/pricing"}), Some("4 matches in 2 files")),
            tool("pa0r", json!({"kind": "readFile", "path": "src/pricing/PlanCard.tsx"}), None),
            {"id": "pa0t0", "kind": "text", "text": "`PlanCard` reads the period once on mount, so flipping the toggle never re-renders the price. I moved it into the shared store."},
            tool("pa0e", json!({"kind": "editFile", "path": "src/pricing/PlanCard.tsx"}), None),
            {"id": "pa0t1", "kind": "text", "text": "Now checking it in the preview."},
            preview("pa0p1", "preview_open", json!({"url": "http://localhost:5173/pricing"}), "Opened Pricing · fieldnotes"),
            preview("pa0p2", "preview_click", json!({"ref": "e12"}), "Clicked \"Yearly\""),
            preview("pa0p3", "preview_snapshot", json!({}), "Pro: $8/mo billed yearly"),
         ]}
    ])
}

fn zoomie(
    id: &str,
    name: &str,
    space: &str,
    instructions: &str,
    trigger: serde_json::Value,
    next_in_minutes: Option<i64>,
    last: Option<(&str, i64, &str)>,
    running: bool,
) -> zeron_proto::agents::AgentSummary {
    let now = chrono::Utc::now();
    let mut agent = json!({
        "id": id, "name": name, "harness": "claude-code", "model": "claude-sonnet-5",
        "spaceId": space, "instructions": instructions, "enabled": true,
        "triggers": [trigger],
        "workspacePath": format!("/projects/{space}"), "memoryPath": format!("/agents/{id}/MEMORY.md"),
        "webhookPath": format!("/agents/{id}/hook"), "consecutiveFailures": 0,
        "createdAt": now - chrono::Duration::days(6), "updatedAt": now - chrono::Duration::days(1),
    });
    if let Some(minutes) = next_in_minutes {
        agent["nextFireAt"] = json!(now + chrono::Duration::minutes(minutes));
    }
    if running {
        agent["activeRunId"] = json!(format!("{id}-run"));
    }
    if let Some((status, minutes_ago, summary)) = last {
        agent["lastRun"] = json!({
            "id": format!("{id}-run"), "agentId": id, "agentName": name, "trigger": "cron",
            "scheduledAt": now - chrono::Duration::minutes(minutes_ago),
            "startedAt": now - chrono::Duration::minutes(minutes_ago),
            "status": status, "summary": summary, "collapsed": 0, "read": true, "archived": false,
            "createdAt": now - chrono::Duration::minutes(minutes_ago),
        });
    }
    serde_json::from_value(agent).unwrap()
}

/// The sidebar's Zoomies, before (`made` = None) and after Send.
fn zoomies(made: Option<zeron_proto::agents::AgentSummary>) -> zeron_proto::agents::AgentsSnapshot {
    let tz = zeron_ui::zoomies::local_timezone();
    let mut agents = vec![
        zoomie(
            "triage",
            "Triage new issues",
            "fieldnotes",
            "Label and dedupe new GitHub issues",
            json!({"kind": "cron", "expr": "0 9 * * *", "timezone": tz}),
            Some(160),
            Some((
                "succeeded",
                21 * 60,
                "Labelled 6 issues, closed 2 duplicates",
            )),
            false,
        ),
        zoomie(
            "deps",
            "Nightly dependency check",
            "api",
            "Check for vulnerable or outdated packages",
            json!({"kind": "cron", "expr": "0 2 * * *", "timezone": tz}),
            None,
            Some(("running", 1, "")),
            true,
        ),
        zoomie(
            "changelog",
            "Update changelog on merge",
            "fieldnotes",
            "Add the merged PR to CHANGELOG.md",
            json!({"kind": "webhook"}),
            None,
            Some(("succeeded", 95, "Added #214 to CHANGELOG.md")),
            false,
        ),
    ];
    agents.extend(made);
    zeron_proto::agents::AgentsSnapshot {
        agents,
        unread_runs: 0,
        needs_approval_runs: 0,
    }
}

/// The zoomie Send makes in the promo: its first run starts right away.
fn made_zoomie(prompt: &str) -> zeron_proto::agents::AgentSummary {
    let tz = zeron_ui::zoomies::local_timezone();
    zoomie(
        "prs",
        &zeron_ui::zoomies::derive_name(prompt),
        "fieldnotes",
        prompt,
        json!({"kind": "cron", "expr": "0 18 * * 1-5", "timezone": tz}),
        None,
        Some(("running", 0, "")),
        true,
    )
}

const ZOOMIE_PROMPT: &str = "Summarise new PRs in fieldnotes and post a short digest";
const ZOOMIE_WHEN: &str = "every weekday at 6pm";

/// One step of the promo script.
enum Act {
    /// Start a named beat (`scenes.txt`).
    Mark(&'static str),
    Receipt,
    Mission(bool),
    Inbox,
    /// Open a chat and load its transcript.
    Chat(&'static str),
    Preview,
    /// Home; `true` turns Zoomie mode on.
    Home(bool),
    Prompt(usize),
    When(usize),
    Creating,
    Created,
    AccessPicker,
    Worktree(bool),
    HideInfo,
    PullRequest,
    ClosePane,
    Usage,
    CloseSettings,
    Transfer(Option<(&'static str, u8)>),
    CloseTransfer,
    Import,
}

fn promo_script() -> Vec<(f32, Act)> {
    let mut script = vec![
        (0.0, Act::Mark("mission")),
        (0.0, Act::Mission(true)),
        (3.8, Act::Mission(false)),
        (3.8, Act::Mark("inbox")),
        (3.8, Act::Inbox),
        (7.4, Act::Mark("approval")),
        (7.4, Act::Chat("search")),
        (9.2, Act::AccessPicker),
        (10.9, Act::AccessPicker),
        (11.3, Act::Mark("worktree")),
        (11.3, Act::Worktree(false)),
        (13.3, Act::Worktree(true)),
        (15.0, Act::HideInfo),
        (15.0, Act::Mark("pr")),
        (15.0, Act::PullRequest),
        (19.0, Act::ClosePane),
        (19.0, Act::Mark("receipt")),
        (19.0, Act::Chat("dark")),
        (19.8, Act::Receipt),
        (22.6, Act::Mark("usage")),
        (22.6, Act::Usage),
        (26.2, Act::CloseSettings),
        (26.2, Act::Mark("conductor")),
        (26.2, Act::Chat("launch")),
        (30.2, Act::Mark("transfer")),
        (30.2, Act::Transfer(None)),
        (32.4, Act::Transfer(Some(("Receiving objects", 34)))),
        (33.6, Act::Transfer(Some(("Resolving deltas", 81)))),
        (34.8, Act::CloseTransfer),
        (34.8, Act::Mark("import")),
        (34.8, Act::Import),
        (38.4, Act::CloseSettings),
        (38.4, Act::Mark("preview")),
        (38.4, Act::Chat("pricing")),
        (39.1, Act::Preview),
        (43.6, Act::ClosePane),
        (43.6, Act::Mark("zoomie")),
        (43.6, Act::Home(false)),
        (44.5, Act::Home(true)),
    ];
    // Typed a character at a time; the When field reads every keystroke.
    let typing = 45.1;
    let prompt = ZOOMIE_PROMPT.chars().count();
    for i in 1..=prompt {
        script.push((typing + i as f32 * 0.032, Act::Prompt(i)));
    }
    let when_start = typing + prompt as f32 * 0.032 + 0.5;
    let when: Vec<char> = ZOOMIE_WHEN.chars().collect();
    for i in 1..=when.len() {
        script.push((when_start + i as f32 * 0.05, Act::When(i)));
    }
    let sent = when_start + when.len() as f32 * 0.05 + 2.0;
    script.push((sent, Act::Creating));
    script.push((sent + 0.5, Act::Created));
    script.push((sent + 4.0, Act::Mark("end")));
    script
}

/// Settings → Providers → More agents: the public ACP registry, one installed.
fn acp_registry() -> zeron_proto::AcpRegistrySnapshot {
    serde_json::from_value(json!({
        "agents": [
            {"id": "github-copilot", "name": "GitHub Copilot CLI", "version": "1.4.0",
             "description": "GitHub's coding agent in your terminal, over ACP.",
             "publisher": "GitHub", "distribution": "npm", "installSize": 38_400_000,
             "installedVersion": "1.4.0"},
            {"id": "factory-droid", "name": "Factory Droid", "version": "0.22.3",
             "description": "Factory's Droid agent for end-to-end software tasks.",
             "publisher": "Factory", "distribution": "binary", "installSize": 61_200_000},
            {"id": "gemini-cli", "name": "Gemini CLI", "version": "0.12.0",
             "description": "Google's open-source agent that brings Gemini to the terminal.",
             "publisher": "Google", "distribution": "npm", "installSize": 24_700_000},
            {"id": "kimi-cli", "name": "Kimi CLI", "version": "0.48.0",
             "description": "Moonshot AI's terminal agent, powered by Kimi models.",
             "publisher": "Moonshot AI", "distribution": "binary", "installSize": 18_900_000},
            {"id": "qwen-code", "name": "Qwen Code", "version": "0.1.4",
             "description": "A command-line agent tuned for Qwen coder models.",
             "publisher": "Qwen", "distribution": "npm", "installSize": 21_300_000},
        ],
        "fetchedAt": chrono::Utc::now().timestamp_millis() - 300_000,
    }))
    .unwrap()
}

/// One step of the stills script; `Snap` writes `<name>.png`.
enum Still {
    Chat(&'static str),
    Accounts,
    SessionUsage,
    Worktree,
    HideInfo,
    PullRequest,
    PullRequestScroll(f32),
    ClosePane,
    Providers(f32),
    Servers(f32),
    CloseSettings,
    Select(&'static [&'static str]),
    Snap(&'static str),
}

fn stills_script() -> Vec<Still> {
    use Still::*;
    vec![
        Accounts,
        Chat("search"),
        SessionUsage,
        Snap("usage"),
        Worktree,
        Snap("usage-worktree"),
        HideInfo,
        PullRequest,
        Snap("pr-0"),
        PullRequestScroll(220.0),
        Snap("pr-220"),
        PullRequestScroll(420.0),
        Snap("pr-420"),
        PullRequestScroll(640.0),
        Snap("pr-640"),
        ClosePane,
        Providers(560.0),
        Snap("agents-560"),
        Providers(2000.0),
        Snap("agents-end"),
        CloseSettings,
        Servers(0.0),
        Snap("servers-0"),
        Servers(2000.0),
        Snap("servers-end"),
        CloseSettings,
        Chat("dark"),
        Select(&["dark", "sync", "astro"]),
        Snap("multi"),
    ]
}

/// An ffmpeg concat list: each frame for as long as it was on screen.
fn write_list(output: &std::path::Path, durations: &[f64]) -> std::io::Result<()> {
    let mut concat = std::fs::File::create(output.join("frames.txt"))?;
    for (index, duration) in durations.iter().enumerate() {
        writeln!(
            concat,
            "file 'frame_{index:05}.png'\nduration {duration:.4}"
        )?;
    }
    if let Some(last) = durations.len().checked_sub(1) {
        writeln!(concat, "file 'frame_{last:05}.png'")?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let output = PathBuf::from(std::env::args().nth(1).expect("frames directory"));
    std::fs::create_dir_all(&output)?;
    let stills = std::env::args().nth(2).as_deref() == Some("stills");
    let promo = stills || std::env::args().nth(2).as_deref() == Some("promo");
    let (width, height) = if promo {
        (1280.0, 800.0)
    } else {
        (WIDTH, HEIGHT)
    };
    let frame_every = if promo {
        Duration::from_millis(33)
    } else {
        FRAME
    };
    let preview_url = std::env::var("GODSPEED_DEMO_PREVIEW_URL")
        .unwrap_or_else(|_| "http://localhost:5173/pricing".into());
    let show = std::env::var_os("GODSPEED_DEMO_SHOW").is_some();
    let temp = tempfile::tempdir()?;
    let data = temp.path().to_path_buf();
    let projects = data.join("projects");
    for (name, color, glyph) in [("fieldnotes", "#668cf5", "M7 6h11v3h-8v3h6v3h-6v4H7Z")] {
        std::fs::create_dir_all(projects.join(name).join("public"))?;
        std::fs::write(
            projects.join(name).join("public/favicon.svg"),
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="1" y="1" width="22" height="22" rx="6" fill="{color}"/><path d="{glyph}" fill="white"/></svg>"#
            ),
        )?;
    }

    // PNG encoding happens off the UI thread.
    let (tx, rx) = mpsc::sync_channel::<(usize, image::RgbaImage)>(12);
    let rx = Arc::new(Mutex::new(rx));
    let writers: Vec<_> = (0..3)
        .map(|_| {
            let rx = rx.clone();
            let output = output.clone();
            std::thread::spawn(move || {
                loop {
                    let next = rx.lock().unwrap().recv();
                    let Ok((index, frame)) = next else { break };
                    let file = std::fs::File::create(output.join(format!("frame_{index:05}.png")))
                        .unwrap();
                    let encoder = image::codecs::png::PngEncoder::new_with_quality(
                        std::io::BufWriter::new(file),
                        image::codecs::png::CompressionType::Fast,
                        image::codecs::png::FilterType::Sub,
                    );
                    image::ImageEncoder::write_image(
                        encoder,
                        frame.as_raw(),
                        frame.width(),
                        frame.height(),
                        image::ExtendedColorType::Rgba8,
                    )
                    .unwrap();
                }
            })
        })
        .collect();

    let list = Arc::new(Mutex::new(Vec::<f64>::new()));
    let failure = Arc::new(Mutex::new(None::<String>));
    let result = failure.clone();
    gpui_platform::application()
        .with_assets(icons::Assets)
        .run(move |cx| {
            #[cfg(target_os = "macos")]
            become_accessory();
            gpui_tokio::init(cx);
            gpui_base::init(cx);
            let mut settings = settings::UiSettings::default();
            settings.sidebar_show_branch = true;
            settings.sidebar_show_project_label = true;
            settings.sidebar_width = 300.0;
            settings.surface = zeron_theme::SurfacePreference::Frosted;
            settings.reduce_motion = motion::ReduceMotion::Off;
            settings.pause_animations_in_background = false;
            settings.show_receipts = true;
            settings.session_info_hidden = true;
            settings.github_star_banner_dismissed = true;
            if promo {
                // The release promo runs in Godspeed's gold.
                settings.accent =
                    zeron_theme::AccentSelection::Preset(zeron_theme::AccentPreset::Amber);
                settings.sidebar_show_zoomies = true;
                settings.sidebar_zoomies_open = true;
                settings.sidebar_width = 272.0;
                settings.conductor_preview = true;
            }
            settings.save(&data).unwrap();
            settings::init(settings.clone(), data.clone(), cx);
            let fonts = typography::register_fonts(cx);
            typography::init(
                settings.ui_font_family.clone(),
                settings.ui_font_size,
                settings.terminal_font_family.clone(),
                settings.terminal_font_size,
                settings.code_font_family.clone(),
                settings.code_font_size,
                fonts,
                cx,
            );
            theme_library::init(data.clone(), cx);
            appearance::init(
                appearance::AppearanceMode::Dark,
                settings.theme_selection,
                settings.accent,
                settings.surface,
                cx,
            );
            history::init(
                settings.git_history_columns,
                settings.git_history_column_widths,
                settings.git_history_column_order,
                settings.git_history_author_display,
                cx,
            );
            motion::init(
                settings.reduce_motion,
                settings.pause_animations_in_background,
                cx,
            );
            composer::init(cx, settings.composer_send_behavior);
            terminal::panel::init(cx);
            app_menus::init(cx);
            let projects_for_import = projects.clone();
            let state = cx.new(|_| {
                let mut s = state::AppState::new();
                seed(&mut s, &projects);
                s
            });
            let boot = EngineBootConfig {
                data_dir: data.clone(),
                ipc_port: 0,
                edge_url: String::new(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: HarnessId::ClaudeCode,
            };
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            gpui::point(px(40.), px(40.)),
                            size(px(width), px(height)),
                        ))),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: None,
                            appears_transparent: true,
                            traffic_light_position: Some(gpui::point(px(14.), px(14.))),
                        }),
                        app_owns_titlebar_drag: true,
                        focus: false,
                        show,
                        window_background: theme::Theme::of(cx).window_background_appearance(),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx)),
                )
                .unwrap();
            appearance::reapply_window_background(cx);
            #[cfg(target_os = "macos")]
            if show {
                let _ = window.update(cx, |_, w, _| order_back(w));
            }
            state.update(cx, |_, cx| cx.notify());

            // Heartbeats: the live sessions and the server stay fresh.
            let beat = state.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    pause(cx, 4000).await;
                    if beat
                        .update(cx, |s, cx| {
                            refresh_sessions(s);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();

            cx.spawn(async move |cx| {
                let run: anyhow::Result<()> = async {
                    pause(cx, 400).await;
                    state.update(cx, |s, cx| {
                        let device = s.local_device_id.clone().unwrap();
                        s.receive_transcript_frame(
                            zeron_doc::TranscriptFrame::Reset {
                                reset: serde_json::from_value(transcript(&device)).unwrap(),
                            },
                            cx,
                        )
                        .unwrap();
                        cx.notify();
                    });
                    // Fonts, icons and layout settle before the first frame.
                    pause(cx, 2000).await;
                    if promo {
                        window.update(cx, |s, _, cx| {
                            s.fixture_demo_zoomies(zoomies(None), cx);
                            s.fixture_demo_accounts(accounts(), cx);
                        })?;
                        pause(cx, 600).await;
                    }
                    if stills {
                        for step in stills_script() {
                            match step {
                                Still::Snap(name) => {
                                    // Let springs, fades and late layout settle.
                                    pause(cx, 1500).await;
                                    let any: gpui::AnyWindowHandle = window.into();
                                    let frame = any.update(cx, |_, w, cx| {
                                        w.draw(cx).clear();
                                        w.render_to_image()
                                    })??;
                                    frame.save(output.join(format!("{name}.png")))?;
                                    eprintln!("still {name} {}x{}", frame.width(), frame.height());
                                    continue;
                                }
                                Still::Chat(id) => {
                                    window
                                        .update(cx, |s, _, cx| s.fixture_demo_open_chat(id, cx))?;
                                    state.update(cx, |s, cx| {
                                        let device = s.local_device_id.clone().unwrap();
                                        let reset = match id {
                                            "search" => search_transcript(&device),
                                            _ => transcript(&device),
                                        };
                                        s.receive_transcript_frame(
                                            zeron_doc::TranscriptFrame::Reset {
                                                reset: serde_json::from_value(reset).unwrap(),
                                            },
                                            cx,
                                        )
                                        .unwrap();
                                        cx.notify();
                                    });
                                }
                                Still::Accounts => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_accounts(accounts(), cx)
                                })?,
                                Still::SessionUsage => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_session_usage(
                                        zeron_proto::ContextUsage {
                                            tokens: Some(84_300),
                                            window: Some(200_000),
                                        },
                                        cx,
                                    )
                                })?,
                                Still::Worktree => window
                                    .update(cx, |s, _, cx| s.fixture_demo_worktree(false, cx))?,
                                Still::HideInfo => window
                                    .update(cx, |s, _, cx| s.fixture_demo_hide_session_info(cx))?,
                                Still::PullRequest => window.update(cx, |s, w, cx| {
                                    let (detail, options) = pull_request();
                                    s.fixture_demo_pull_request(detail, options, w, cx)
                                })?,
                                Still::PullRequestScroll(y) => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_pull_request_scroll(y, cx)
                                })?,
                                Still::ClosePane => {
                                    window.update(cx, |s, _, cx| s.fixture_demo_close_pane(cx))?
                                }
                                Still::Providers(y) => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_providers(acp_registry(), y, cx)
                                })?,
                                Still::Servers(y) => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_servers(
                                        vec![
                                            (
                                                "dev@build-box".into(),
                                                "build-box".into(),
                                                "0.5.10".into(),
                                                3 * 86_400 + 4 * 3_600,
                                            ),
                                            (
                                                "ops@hetzner-arm".into(),
                                                "hetzner-arm".into(),
                                                "0.5.10".into(),
                                                11 * 3_600,
                                            ),
                                        ],
                                        y,
                                        cx,
                                    )
                                })?,
                                Still::CloseSettings => window
                                    .update(cx, |s, _, cx| s.fixture_demo_close_settings(cx))?,
                                Still::Select(ids) => window
                                    .update(cx, |s, _, cx| s.fixture_demo_select_rows(ids, cx))?,
                            }
                            pause(cx, 300).await;
                        }
                        return Ok(());
                    }
                    let start = Instant::now();
                    let mut index = 0usize;
                    let mut last = start;
                    let mut step = 0;
                    // (seconds into the recording, action)
                    let script: Vec<(f32, Act)> = if promo {
                        promo_script()
                    } else {
                        vec![
                            (1.3, Act::Receipt),
                            (4.6, Act::Mission(true)),
                            (11.0, Act::Mission(false)),
                            (11.0, Act::Inbox),
                            (14.4, Act::Mark("end")),
                        ]
                    };
                    let mut scenes = String::new();
                    loop {
                        let elapsed = start.elapsed().as_secs_f32();
                        while step < script.len() && elapsed >= script[step].0 {
                            match &script[step].1 {
                                Act::Mark(name) => scenes.push_str(&format!("{name} {index}\n")),
                                Act::Receipt => {
                                    window.update(cx, |s, _, cx| s.fixture_demo_open_receipt(cx))?
                                }
                                Act::Mission(open) => window.update(cx, |s, w, cx| {
                                    s.fixture_demo_mission_control(*open, w, cx)
                                })?,
                                Act::Inbox => {
                                    window.update(cx, |s, _, cx| s.fixture_demo_inbox(cx))?
                                }
                                Act::Chat(id) => {
                                    window
                                        .update(cx, |s, _, cx| s.fixture_demo_open_chat(id, cx))?;
                                    let id = id.to_string();
                                    state.update(cx, |s, cx| {
                                        let device = s.local_device_id.clone().unwrap();
                                        let reset = match id.as_str() {
                                            "pricing" => pricing_transcript(&device),
                                            "search" => search_transcript(&device),
                                            "launch" => launch_transcript(&device),
                                            _ => transcript(&device),
                                        };
                                        s.receive_transcript_frame(
                                            zeron_doc::TranscriptFrame::Reset {
                                                reset: serde_json::from_value(reset).unwrap(),
                                            },
                                            cx,
                                        )
                                        .unwrap();
                                        cx.notify();
                                    });
                                }
                                Act::Preview => window.update(cx, |s, w, cx| {
                                    s.fixture_demo_agent_preview(preview_url.clone(), w, cx)
                                })?,
                                Act::Home(zoomie) => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_zoomie_composer(*zoomie, cx)
                                })?,
                                Act::Prompt(chars) => {
                                    let text: String = ZOOMIE_PROMPT.chars().take(*chars).collect();
                                    window.update(cx, |s, _, cx| {
                                        s.fixture_demo_zoomie_text(Some(&text), None, cx)
                                    })?
                                }
                                Act::When(chars) => {
                                    let text: String = ZOOMIE_WHEN.chars().take(*chars).collect();
                                    window.update(cx, |s, _, cx| {
                                        s.fixture_demo_zoomie_text(None, Some(&text), cx)
                                    })?
                                }
                                Act::Creating => window
                                    .update(cx, |s, _, cx| s.fixture_demo_zoomie_send(None, cx))?,
                                Act::AccessPicker => window
                                    .update(cx, |s, w, cx| s.fixture_demo_access_picker(w, cx))?,
                                Act::Worktree(sleeping) => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_worktree(*sleeping, cx)
                                })?,
                                Act::HideInfo => window
                                    .update(cx, |s, _, cx| s.fixture_demo_hide_session_info(cx))?,
                                Act::PullRequest => window.update(cx, |s, w, cx| {
                                    let (detail, options) = pull_request();
                                    s.fixture_demo_pull_request(detail, options, w, cx)
                                })?,
                                Act::ClosePane => {
                                    window.update(cx, |s, _, cx| s.fixture_demo_close_pane(cx))?
                                }
                                Act::Usage => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_usage(usage_history(), cx)
                                })?,
                                Act::CloseSettings => window
                                    .update(cx, |s, _, cx| s.fixture_demo_close_settings(cx))?,
                                Act::Transfer(cloning) => window.update(cx, |s, _, cx| {
                                    let files = vec![
                                        (".env".to_string(), 412),
                                        (".env.local".to_string(), 188),
                                        ("config/dev.secrets.json".to_string(), 1_204),
                                        ("certs/localhost.pem".to_string(), 2_310),
                                    ];
                                    s.fixture_demo_add_to_device(
                                        "fieldnotes",
                                        "studio",
                                        files,
                                        cloning.map(|(stage, pct)| (stage.to_string(), pct)),
                                        cx,
                                    )
                                })?,
                                Act::CloseTransfer => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_close_add_to_device(cx)
                                })?,
                                Act::Import => window.update(cx, |s, _, cx| {
                                    s.fixture_demo_import(import_previews(&projects_for_import), cx)
                                })?,
                                Act::Created => window.update(cx, |s, _, cx| {
                                    let made = made_zoomie(ZOOMIE_PROMPT);
                                    s.fixture_demo_zoomie_send(Some(made.clone()), cx);
                                    s.fixture_demo_zoomies(zoomies(Some(made)), cx);
                                })?,
                            }
                            step += 1;
                        }
                        if step == script.len() {
                            std::fs::write(output.join("scenes.txt"), &scenes)?;
                            break;
                        }
                        let any: gpui::AnyWindowHandle = window.into();
                        let frame = any.update(cx, |_, w, cx| {
                            w.draw(cx).clear();
                            w.render_to_image()
                        })??;
                        let now = Instant::now();
                        if index > 0 {
                            list.lock().unwrap().push((now - last).as_secs_f64());
                        }
                        last = now;
                        tx.send((index, frame))?;
                        index += 1;
                        let next = start + frame_every * index as u32;
                        cx.background_executor()
                            .timer(next.saturating_duration_since(Instant::now()))
                            .await;
                    }
                    list.lock().unwrap().push(frame_every.as_secs_f64());
                    eprintln!(
                        "captured {index} frames in {:.1}s",
                        start.elapsed().as_secs_f32()
                    );
                    Ok(())
                }
                .await;
                if let Err(error) = run {
                    eprintln!("demo fixture failed: {error:#}");
                    *result.lock().unwrap() = Some(format!("{error:#}"));
                }
                // Quitting ends the process: flush the frames and the list first.
                drop(tx);
                for writer in writers {
                    let _ = writer.join();
                }
                if let Err(error) = write_list(&output, &list.lock().unwrap()) {
                    eprintln!("demo fixture failed: {error:#}");
                }
                let _ = window.update(cx, |_, w, _| w.remove_window());
                pause(cx, 100).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
