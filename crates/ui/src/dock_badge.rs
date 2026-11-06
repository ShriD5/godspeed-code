//! The Dock icon's badge: how many chats are waiting on you, read from the
//! same attention snapshot as the "Needs you" inbox. It sits on
//! `NSApp.dockTile.badgeLabel`, so it works with or without the animated
//! speedster swapping the icon image underneath it. Settings → Appearance
//! "Show badge on Dock icon" turns it off.

use std::collections::HashSet;

use zeron_proto::attention::{AttentionKind, AttentionSnapshot};

/// Chats paused on an approval or a question. A chat with several pending
/// requests counts once; an item without a chat counts on its own.
pub fn awaiting_chats(attention: &AttentionSnapshot) -> usize {
    let mut chats = HashSet::new();
    attention
        .items
        .iter()
        .filter(|item| matches!(item.kind, AttentionKind::Approval | AttentionKind::Question))
        .filter(|item| chats.insert(item.chat_id.as_deref().unwrap_or(item.id.as_str())))
        .count()
}

/// What the badge reads: nothing at zero or while the setting is off.
pub fn badge_label(awaiting: usize, enabled: bool) -> Option<String> {
    (enabled && awaiting > 0).then(|| awaiting.to_string())
}

/// Start the Dock badge driver. Off outside macOS and in tests.
pub fn init(state: gpui::Entity<crate::state::AppState>, cx: &mut gpui::App) {
    #[cfg(all(target_os = "macos", not(test)))]
    dock::init(state, cx);
    #[cfg(not(all(target_os = "macos", not(test))))]
    let _ = (state, cx);
}

#[cfg(all(target_os = "macos", not(test)))]
mod dock {
    use gpui::{App, BorrowAppContext, Entity, Global, WeakEntity};
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    use super::{awaiting_chats, badge_label};
    use crate::state::AppState;

    struct DockBadge {
        state: WeakEntity<AppState>,
        /// What the Dock shows now; touched only when it changes.
        shown: Option<String>,
    }

    impl Global for DockBadge {}

    pub(super) fn init(state: Entity<AppState>, cx: &mut App) {
        cx.set_global(DockBadge {
            state: state.downgrade(),
            shown: None,
        });
        cx.observe(&state, |_, cx| sync(cx)).detach();
        cx.observe_global::<crate::settings::SettingsStore>(sync)
            .detach();
        sync(cx);
    }

    fn sync(cx: &mut App) {
        if !cx.has_global::<DockBadge>() {
            return;
        }
        let enabled = crate::settings::dock_badge(cx);
        let awaiting = cx
            .global::<DockBadge>()
            .state
            .upgrade()
            .map_or(0, |state| awaiting_chats(&state.read(cx).attention));
        let label = badge_label(awaiting, enabled);
        cx.update_global::<DockBadge, _>(|badge, _| badge.show(label));
    }

    impl DockBadge {
        fn show(&mut self, label: Option<String>) {
            if self.shown == label {
                return;
            }
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let text = label.as_deref().map(NSString::from_str);
            NSApplication::sharedApplication(mtm)
                .dockTile()
                .setBadgeLabel(text.as_deref());
            self.shown = label;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::attention::AttentionItem;

    fn item(id: &str, kind: AttentionKind, chat: Option<&str>) -> AttentionItem {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": kind,
            "chatId": chat,
            "title": "t",
            "createdAt": "2026-10-28T00:00:00Z",
            "summary": "",
            "actions": [],
        }))
        .unwrap()
    }

    fn snapshot(items: Vec<AttentionItem>) -> AttentionSnapshot {
        AttentionSnapshot {
            items,
            ..Default::default()
        }
    }

    #[test]
    fn counts_chats_waiting_on_an_approval_or_answer_once_each() {
        let attention = snapshot(vec![
            item("approval:a:1", AttentionKind::Approval, Some("a")),
            item("approval:a:2", AttentionKind::Approval, Some("a")),
            item("question:b", AttentionKind::Question, Some("b")),
            item("chat:c", AttentionKind::Finished, Some("c")),
            item("chat:d", AttentionKind::Failed, Some("d")),
            item("chat:e", AttentionKind::Limited, Some("e")),
            item("chat:f", AttentionKind::TimedOut, Some("f")),
            item("run:1", AttentionKind::Question, None),
        ]);
        assert_eq!(awaiting_chats(&attention), 3);
        assert_eq!(awaiting_chats(&snapshot(vec![])), 0);
    }

    #[test]
    fn the_label_clears_at_zero_and_when_turned_off() {
        assert_eq!(badge_label(3, true).as_deref(), Some("3"));
        assert_eq!(badge_label(0, true), None);
        assert_eq!(badge_label(3, false), None);
    }

    #[test]
    fn the_setting_defaults_on_for_old_settings_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            crate::settings::UiSettings::path(dir.path()),
            r#"{"animatedDockIcon": false}"#,
        )
        .unwrap();
        let loaded = crate::settings::UiSettings::load(dir.path());
        assert!(loaded.dock_badge);
        assert!(!loaded.animated_dock_icon);
    }
}
