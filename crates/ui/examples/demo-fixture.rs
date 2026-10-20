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
        serde_json::from_value(json!({"id": "mbp", "name": "MacBook Pro", "platform": "macos", "lastSeenAt": now})).unwrap(),
        serde_json::from_value(json!({"id": "studio", "name": "Studio server", "platform": "linux", "lastSeenAt": now})).unwrap(),
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
    refresh_sessions(state);
    let approval: zeron_proto::ApprovalRequest = serde_json::from_value(json!({
        "kind": "command", "title": "Run git push origin feat/search",
        "command": "git push origin feat/search", "cwd": "~/code/fieldnotes",
        "reason": "Push the search branch so the pull request can open",
        "decisions": [{"decision": "approve"}, {"decision": "approveForSession"}, {"decision": "decline"}],
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
                            size(px(WIDTH), px(HEIGHT)),
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
                    let start = Instant::now();
                    let mut index = 0usize;
                    let mut last = start;
                    let mut step = 0;
                    // (seconds into the recording, action)
                    let script: [(f32, u8); 4] = [(1.3, 1), (4.6, 2), (11.0, 3), (14.4, 4)];
                    loop {
                        let elapsed = start.elapsed().as_secs_f32();
                        while step < script.len() && elapsed >= script[step].0 {
                            match script[step].1 {
                                1 => {
                                    window.update(cx, |s, _, cx| s.fixture_demo_open_receipt(cx))?
                                }
                                2 => window.update(cx, |s, w, cx| {
                                    s.fixture_demo_mission_control(true, w, cx)
                                })?,
                                3 => window.update(cx, |s, w, cx| {
                                    s.fixture_demo_mission_control(false, w, cx);
                                    s.fixture_demo_inbox(cx);
                                })?,
                                _ => {}
                            }
                            step += 1;
                        }
                        if step == script.len() {
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
                        let next = start + FRAME * index as u32;
                        cx.background_executor()
                            .timer(next.saturating_duration_since(Instant::now()))
                            .await;
                    }
                    list.lock().unwrap().push(FRAME.as_secs_f64());
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
