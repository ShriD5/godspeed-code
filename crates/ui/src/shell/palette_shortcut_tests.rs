//! ⌘K opens the command palette from every place the keyboard can be:
//! nothing focused, either composer, the terminal, the find bar, Settings and
//! over Mission Control. Built on the real app keymap, with a keymap shaped
//! like one saved by a current build (`layoutNeutral`, ⌘S shared by Save and
//! the prompt stash).
use super::*;
use gpui::{AppContext, TestAppContext, VisualTestContext};

/// A saved keymap as the settings file stores it today.
const SAVED_KEYMAP: &str = r#"{
    "toggleDictation": "mod-d", "captureAppshot": "ctrl-alt-space",
    "randomWallpaper": "mod-u", "saveFile": "mod-s", "browserReload": "mod-shift-r",
    "toggleSidebar": "mod-b", "toggleChanges": "mod-r", "toggleFiles": "mod-e",
    "toggleTerminal": "mod-j", "newSession": "mod-n", "newProject": "mod-shift-n",
    "openModelPicker": "mod-/", "cycleAccessMode": "mod-shift-m",
    "increaseEffort": "mod-shift-.", "decreaseEffort": "mod-shift-,",
    "nextSession": "ctrl-tab", "prevSession": "ctrl-shift-tab",
    "archiveSession": "mod-shift-a", "settleSession": "mod-shift-d",
    "navigateBack": "mod-[", "navigateForward": "mod-]", "toggleDiffPane": "mod-shift-g",
    "reopenClosedTab": "mod-shift-t", "missionControl": "mod-shift-k", "stashPrompt": "mod-s",
    "jumpSession": ["mod-1", "mod-2", "mod-3", "mod-4", "mod-5", "mod-6", "mod-7", "mod-8", "mod-9"],
    "layoutNeutral": true
}"#;

const CHAT: &str = "palette-chat";

fn saved_settings() -> UiSettings {
    let mut settings = UiSettings::default();
    settings.keymap = serde_json::from_str(SAVED_KEYMAP).unwrap();
    settings.composer_send_behavior = ComposerSendBehavior::Enter;
    settings
}

fn setup(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let saved = saved_settings();
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::app_menus::init(cx);
        crate::history::init(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            cx,
        );
        settings::init(saved, dir.path(), cx);
    });
    let data_dir = dir.path().to_path_buf();
    let (shell, cx) = cx.add_window_view(|_, cx| {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.connection = zeron_proto::view::ConnectionStatus::Ready;
            state.workspace_scope = Some(WorkspaceScope::Local);
            state.local_device_id = Some("local".into());
            state.no_project = true;
            state.chats = vec![
                serde_json::from_value(serde_json::json!({
                    "id": CHAT, "title": "Palette", "deviceId": "local", "archived": false,
                    "createdAt": Utc::now(),
                }))
                .unwrap(),
            ];
            state
        });
        // `Shell::new` applies the saved keymap, as the app does at launch.
        Shell::new(
            state,
            EngineBootConfig {
                data_dir,
                ipc_port: 0,
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zeron_proto::HarnessId::Mock,
            },
            cx,
        )
    });
    shell.update(cx, |shell, _| shell.debug_gate = Some(GatePhase::Ready));
    cx.update(|_, cx| motion::set_reduced_motion(cx, true));
    cx.simulate_resize(gpui::size(px(1400.0), px(900.0)));
    settle(cx);
    (shell, cx, dir)
}

fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear());
    cx.run_until_parked();
}

fn open_chat(shell: &Entity<Shell>, cx: &mut VisualTestContext) {
    shell.update(cx, |shell, cx| {
        shell.open_chat(CHAT.into(), cx);
        shell.active_chat = CHAT.into();
    });
    settle(cx);
}

fn focus_composer(shell: &Entity<Shell>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let focus = shell.read(cx).composer.focus_handle(cx);
        window.focus(&focus, cx);
    });
    settle(cx);
    cx.update(|window, cx| {
        assert!(
            shell.read(cx).composer.focus_handle(cx).is_focused(window),
            "the composer holds the keyboard"
        );
    });
}

/// Press ⌘K and report whether the palette is open; then press it again and
/// check it closes, so the toggle works from inside the palette too.
#[track_caller]
fn assert_cmd_k_toggles_palette(shell: &Entity<Shell>, cx: &mut VisualTestContext, place: &str) {
    assert!(
        shell.read_with(cx, |shell, _| shell.command_palette.is_none()),
        "{place}: the palette starts closed"
    );
    cx.simulate_keystrokes(&platform_combo("mod-k"));
    settle(cx);
    assert!(
        shell.read_with(cx, |shell, _| shell.command_palette.is_some()),
        "{place}: ⌘K opens the palette"
    );
    cx.update(|window, cx| {
        let shell = shell.read(cx);
        let search = shell
            .command_palette
            .as_ref()
            .unwrap()
            .search
            .focus_handle(cx);
        assert!(
            search.is_focused(window),
            "{place}: the palette search has the keyboard"
        );
    });
    cx.simulate_keystrokes(&platform_combo("mod-k"));
    settle(cx);
    assert!(
        shell.read_with(cx, |shell, _| shell.command_palette.is_none()),
        "{place}: ⌘K again closes the palette"
    );
}

#[gpui::test]
fn cmd_k_opens_the_palette_with_nothing_focused(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    cx.update(|window, _| window.blur());
    settle(cx);
    assert_cmd_k_toggles_palette(&shell, cx, "nothing focused");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_the_home_composer(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    shell.update(cx, |shell, cx| shell.open_new_session(None, cx));
    settle(cx);
    assert!(shell.read_with(cx, |shell, cx| shell.state.read(cx).selected_chat.is_none()));
    focus_composer(&shell, cx);
    assert_cmd_k_toggles_palette(&shell, cx, "home composer");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_the_chat_composer(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    focus_composer(&shell, cx);
    assert_cmd_k_toggles_palette(&shell, cx, "chat composer");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_the_terminal(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.toggle_terminal(window, cx)));
    settle(cx);
    cx.update(|window, cx| {
        let panel = shell.read(cx).terminal.clone().unwrap();
        assert!(
            panel.read(cx).focus_handle().contains_focused(window, cx),
            "the terminal holds the keyboard"
        );
    });
    assert_cmd_k_toggles_palette(&shell, cx, "terminal");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_the_find_bar(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_find_bar(window, cx)));
    settle(cx);
    cx.update(|window, cx| {
        let shell = shell.read(cx);
        let input = shell.find_bar.as_ref().unwrap().input.focus_handle(cx);
        assert!(input.is_focused(window), "the find bar holds the keyboard");
    });
    assert_cmd_k_toggles_palette(&shell, cx, "find bar");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_settings(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    shell.update(cx, |shell, cx| {
        shell.open_settings(SettingsSection::General, cx)
    });
    settle(cx);
    assert_cmd_k_toggles_palette(&shell, cx, "settings");
}

#[gpui::test]
fn cmd_k_opens_the_palette_over_mission_control(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_mission_control(window, cx)));
    settle(cx);
    assert!(shell.read_with(cx, |shell, _| shell.mission_control.is_some()));
    assert_cmd_k_toggles_palette(&shell, cx, "mission control");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_the_inbox(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    shell.update(cx, |shell, cx| shell.open_inbox(cx));
    settle(cx);
    assert_cmd_k_toggles_palette(&shell, cx, "inbox");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_pull_requests(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    shell.update(cx, |shell, cx| shell.open_pull_requests(cx));
    settle(cx);
    assert_cmd_k_toggles_palette(&shell, cx, "pull requests");
}

/// The preview page's native key monitor forwards ⌘K by focusing the
/// browser surface and dispatching it there; this is that dispatch.
#[gpui::test]
fn cmd_k_opens_the_palette_from_the_preview_browser(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            if !shell.right_pane_open(cx) {
                shell.toggle_right_pane(cx);
            }
            shell.add_browser_surface(None, window, cx);
        })
    });
    settle(cx);
    assert!(
        shell.read_with(cx, |shell, cx| shell.right_pane_shown(cx)),
        "the preview is on screen"
    );
    cx.update(|window, cx| {
        let shell = shell.read(cx);
        let browser = shell.browsers[&shell.browser_seq].focus_handle(cx);
        window.focus(&browser, cx);
    });
    settle(cx);
    cx.update(|window, cx| {
        let shell = shell.read(cx);
        let browser = shell.browsers[&shell.browser_seq].focus_handle(cx);
        assert!(
            browser.contains_focused(window, cx),
            "the browser holds the keyboard"
        );
    });
    assert_cmd_k_toggles_palette(&shell, cx, "preview browser");
}

#[gpui::test]
fn cmd_k_opens_the_palette_from_a_file_editor(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = setup(cx);
    open_chat(&shell, cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.add_file_surface("README.md".into(), window, cx)
        })
    });
    settle(cx);
    assert_cmd_k_toggles_palette(&shell, cx, "file editor");
}
