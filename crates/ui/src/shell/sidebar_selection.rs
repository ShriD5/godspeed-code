//! Sidebar multi-select: Cmd-click (Ctrl-click off macOS) toggles a row,
//! Shift-click extends from the anchor, Cmd+A in the focused sidebar takes
//! every visible row and Escape lets go. While rows are selected a bar at the
//! foot of the sidebar runs the row menu's actions on all of them at once,
//! and one undo puts a whole batch back. The selection is plain view state:
//! scrolling keeps it, navigating clears it.

use chrono::{DateTime, Local, Utc};
use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, Pixels, Point, SharedString, Window, div,
    prelude::*, px,
};
use zeron_proto::{ChatIndicator, LifecycleState};

use super::Shell;
use super::thread_lifecycle::{LifecycleUndo, SnoozePreset};
use crate::composer::limit_banner::local_moment_label;
use crate::icons::{self, icon};
use crate::motion;
use crate::popover;
use crate::theme::Theme;

/// How a press on a sidebar row reads, from its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowClick {
    Open,
    Toggle,
    Extend,
}

pub(super) fn row_click(modifiers: &gpui::Modifiers) -> RowClick {
    if modifiers.secondary() {
        RowClick::Toggle
    } else if modifiers.shift {
        RowClick::Extend
    } else {
        RowClick::Open
    }
}

/// The selected rows, in the order they were picked, and the row a
/// Shift-click extends from.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct SidebarSelection {
    ids: Vec<String>,
    anchor: Option<String>,
}

impl SidebarSelection {
    pub(super) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(super) fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|selected| selected == id)
    }

    pub(super) fn toggle(&mut self, id: &str) {
        if let Some(ix) = self.ids.iter().position(|selected| selected == id) {
            self.ids.remove(ix);
            if self.anchor.as_deref() == Some(id) {
                self.anchor = self.ids.last().cloned();
            }
        } else {
            self.ids.push(id.to_owned());
            self.anchor = Some(id.to_owned());
        }
    }

    /// Add every row between the anchor and `id` in `order`. Without an
    /// anchor the range starts at `fallback` (the open chat), as in Finder;
    /// an end that is not in `order` (a shelf row) just adds `id`.
    pub(super) fn extend_to(&mut self, id: &str, order: &[String], fallback: Option<&str>) {
        let anchor = self.anchor.clone().or_else(|| fallback.map(str::to_owned));
        let span = anchor.as_deref().and_then(|anchor| {
            let from = order.iter().position(|row| row == anchor)?;
            let to = order.iter().position(|row| row == id)?;
            Some(from.min(to)..=from.max(to))
        });
        match span {
            Some(span) => {
                for row in &order[span] {
                    if !self.contains(row) {
                        self.ids.push(row.clone());
                    }
                }
                self.anchor = anchor;
            }
            None => {
                if !self.contains(id) {
                    self.ids.push(id.to_owned());
                }
                self.anchor = Some(id.to_owned());
            }
        }
    }

    pub(super) fn select_all(&mut self, order: &[String]) {
        self.ids = order.to_vec();
        self.anchor = order.first().cloned();
    }

    /// Whether anything was selected.
    pub(super) fn clear(&mut self) -> bool {
        let had = !self.ids.is_empty() || self.anchor.is_some();
        self.ids.clear();
        self.anchor = None;
        had
    }

    /// Drop rows that no longer exist. Whether anything changed.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) -> bool {
        let before = self.ids.len();
        self.ids.retain(|id| keep(id));
        if self.anchor.as_deref().is_some_and(|anchor| !keep(anchor)) {
            self.anchor = None;
        }
        before != self.ids.len()
    }

    /// The selection in the sidebar's top-to-bottom order; rows not in
    /// `order` (shelves) follow in pick order.
    pub(super) fn ordered(&self, order: &[String]) -> Vec<String> {
        let mut out: Vec<String> = order
            .iter()
            .filter(|id| self.contains(id))
            .cloned()
            .collect();
        out.extend(
            self.ids
                .iter()
                .filter(|id| !order.contains(id))
                .cloned(),
        );
        out
    }
}

/// One action from the bulk bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum BulkAction {
    Settle,
    Unsettle,
    Snooze(DateTime<Utc>),
    MarkRead,
    MarkUnread,
    Pin,
    Unpin,
    Archive,
}

/// What a selected row is, for the bar's toggles and the dispatch.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BulkRow {
    pub id: String,
    pub lifecycle: LifecycleState,
    pub unseen: bool,
    pub pinned: bool,
    /// Running or waiting on the user: it can't settle or snooze.
    pub live: bool,
    pub archived: bool,
    /// Side chats are never pinned or archived from the row menu.
    pub side_chat: bool,
}

/// Which way each toggle on the bar points: Settle while any row is not
/// settled, Mark as read while any is unread, Pin while any is unpinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BulkToggles {
    pub settle: bool,
    pub read: bool,
    pub pin: bool,
}

pub(super) fn bulk_toggles(rows: &[BulkRow]) -> BulkToggles {
    BulkToggles {
        settle: rows
            .iter()
            .any(|row| !row.archived && row.lifecycle != LifecycleState::Settled),
        read: rows.iter().any(|row| row.unseen),
        pin: rows.iter().any(|row| !row.side_chat && !row.pinned),
    }
}

/// The rows `action` changes: the ones the row menu would offer it for,
/// minus the ones it already holds for.
pub(super) fn bulk_targets(action: BulkAction, rows: &[BulkRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| match action {
            BulkAction::Settle => {
                !row.archived && !row.live && row.lifecycle != LifecycleState::Settled
            }
            BulkAction::Unsettle => !row.archived && row.lifecycle == LifecycleState::Settled,
            BulkAction::Snooze(until) => {
                !row.archived && !row.live && row.lifecycle != (LifecycleState::Snoozed { until })
            }
            BulkAction::MarkRead => row.unseen,
            BulkAction::MarkUnread => !row.archived && !row.unseen,
            BulkAction::Pin => !row.side_chat && !row.pinned,
            BulkAction::Unpin => !row.side_chat && row.pinned,
            BulkAction::Archive => !row.side_chat && !row.archived,
        })
        .map(|row| row.id.clone())
        .collect()
}

/// "1 session" / "3 sessions".
fn sessions(count: usize) -> String {
    if count == 1 {
        "1 session".to_owned()
    } else {
        format!("{count} sessions")
    }
}

/// A row running or waiting on the user stays in the list.
fn live(status: ChatIndicator) -> bool {
    matches!(
        status,
        ChatIndicator::Working | ChatIndicator::AwaitingInput | ChatIndicator::AwaitingApproval
    )
}

impl Shell {
    /// A Cmd- or Shift-press on a sidebar row: change the selection instead
    /// of opening the chat. Whether the press was taken.
    pub(super) fn select_sidebar_row(
        &mut self,
        chat_id: &str,
        click: RowClick,
        cx: &mut Context<Self>,
    ) -> bool {
        match click {
            RowClick::Open => return false,
            RowClick::Toggle => self.sidebar_selection.toggle(chat_id),
            RowClick::Extend => {
                let order = self.sidebar_visible_order(cx);
                let open = self.state.read(cx).selected_chat.clone();
                self.sidebar_selection
                    .extend_to(chat_id, &order, open.as_deref());
            }
        }
        self.bulk_snooze_menu = None;
        cx.notify();
        true
    }

    /// Cmd+A in the focused sidebar.
    pub(super) fn select_all_sidebar_rows(&mut self, cx: &mut Context<Self>) {
        let order = self.sidebar_visible_order(cx);
        self.sidebar_selection.select_all(&order);
        cx.notify();
    }

    pub(super) fn clear_sidebar_selection(&mut self, cx: &mut Context<Self>) -> bool {
        self.bulk_snooze_menu = None;
        let cleared = self.sidebar_selection.clear();
        if cleared {
            cx.notify();
        }
        cleared
    }

    /// Keep the selection honest as the app state moves: rows that were
    /// deleted fall out, and opening another chat (by any route) clears it.
    pub(super) fn reconcile_sidebar_selection(&mut self, cx: &mut Context<Self>) {
        let selected_chat = self.state.read(cx).selected_chat.clone();
        let navigated = self.sidebar_selection_origin != selected_chat;
        self.sidebar_selection_origin = selected_chat;
        if self.sidebar_selection.is_empty() {
            return;
        }
        if navigated {
            self.clear_sidebar_selection(cx);
            return;
        }
        let state = self.state.read(cx);
        if !state.chats_synced {
            return;
        }
        let known: std::collections::HashSet<&str> =
            state.chats.iter().map(|chat| chat.id.as_str()).collect();
        let pruned = self.sidebar_selection.retain(|id| known.contains(id));
        if pruned && self.sidebar_selection.is_empty() {
            self.bulk_snooze_menu = None;
        }
    }

    /// What each selected row is, in sidebar order.
    pub(super) fn bulk_rows(&self, cx: &Context<Self>) -> Vec<BulkRow> {
        let order = self.sidebar_visible_order(cx);
        let pins = self.active_sidebar_pins(cx);
        let now = Utc::now();
        let state = self.state.read(cx);
        self.sidebar_selection
            .ordered(&order)
            .into_iter()
            .filter_map(|id| {
                let chat = state.chats.iter().find(|chat| chat.id == id)?;
                Some(BulkRow {
                    lifecycle: chat.lifecycle.at(now),
                    unseen: chat.unseen(),
                    pinned: pins.contains(&chat.id),
                    live: live(state.display_status_for(chat, now)),
                    archived: chat.archived,
                    side_chat: chat.parent_chat_id.is_some(),
                    id,
                })
            })
            .collect()
    }

    /// Run `action` on every selected row it applies to. Lifecycle changes
    /// and Mark as unread undo as one step, as they do one row at a time.
    pub(super) fn apply_bulk_action(&mut self, action: BulkAction, cx: &mut Context<Self>) {
        self.bulk_snooze_menu = None;
        let rows = self.bulk_rows(cx);
        let targets = bulk_targets(action, &rows);
        let skipped_live = match action {
            BulkAction::Settle | BulkAction::Snooze(_) => rows
                .iter()
                .filter(|row| row.live && !row.archived)
                .count(),
            _ => 0,
        };
        let now = Utc::now();
        let mut undos = Vec::new();
        match action {
            BulkAction::Settle | BulkAction::Unsettle | BulkAction::Snooze(_) => {
                let next = match action {
                    BulkAction::Settle => LifecycleState::Settled,
                    BulkAction::Snooze(until) => LifecycleState::Snoozed { until },
                    _ => LifecycleState::Active,
                };
                for id in &targets {
                    undos.extend(self.apply_lifecycle(id.clone(), next, now, cx));
                }
            }
            BulkAction::MarkRead => {
                for id in &targets {
                    self.state
                        .update(cx, |state, cx| state.mark_chat_seen(id, cx));
                }
            }
            BulkAction::MarkUnread => {
                for id in &targets {
                    let seen_at = self
                        .state
                        .update(cx, |state, cx| state.mark_chat_unread(id, cx));
                    if let Some(seen_at) = seen_at {
                        undos.push(LifecycleUndo::Unread {
                            chat_id: id.clone(),
                            seen_at,
                        });
                    }
                }
            }
            BulkAction::Pin | BulkAction::Unpin => {
                let pinned = action == BulkAction::Pin;
                for id in &targets {
                    self.set_chat_pinned(id.clone(), pinned, cx);
                }
            }
            BulkAction::Archive => {
                for id in &targets {
                    self.set_chat_archived(id.clone(), true, cx);
                }
            }
        }
        let count = sessions(targets.len());
        let mut notice = match action {
            BulkAction::Settle => format!("Settled {count}"),
            BulkAction::Unsettle => format!("{count} back in the list"),
            BulkAction::Snooze(until) => {
                format!("Snoozed {count} until {}", local_moment_label(until, now))
            }
            BulkAction::MarkRead => format!("Marked {count} as read"),
            BulkAction::MarkUnread => format!("Marked {count} as unread"),
            BulkAction::Pin => format!("Pinned {count}"),
            BulkAction::Unpin => format!("Unpinned {count}"),
            BulkAction::Archive => format!("Archived {count}"),
        };
        if skipped_live > 0 {
            notice.push_str(&format!(
                " · skipped {} still running",
                sessions(skipped_live)
            ));
        }
        // Archived rows leave the list; a selection pointing into the
        // Archived shelf would only confuse the next action.
        if action == BulkAction::Archive {
            self.sidebar_selection.clear();
        }
        match undos.len() {
            0 => crate::toast::info(cx, notice),
            1 => self.remember_lifecycle_undo(undos.remove(0), notice, cx),
            _ => self.remember_lifecycle_undo(LifecycleUndo::Batch(undos), notice, cx),
        }
        cx.notify();
    }

    /// The bar's Delete: one confirmation for every selected row.
    pub(super) fn request_bulk_delete(&mut self, cx: &mut Context<Self>) {
        self.bulk_snooze_menu = None;
        let order = self.sidebar_visible_order(cx);
        let ids = self.sidebar_selection.ordered(&order);
        if !ids.is_empty() {
            self.request_delete_chats(ids, cx);
        }
    }

    /// Escape: close the bar's Snooze menu, then let go of the selection.
    pub(super) fn escape_sidebar_selection(&mut self, cx: &mut Context<Self>) -> bool {
        if self.bulk_snooze_menu.take().is_some() {
            cx.notify();
            return true;
        }
        self.clear_sidebar_selection(cx)
    }

    /// Cmd+A and Escape while the sidebar holds keyboard focus.
    pub(super) fn on_sidebar_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if keystroke.key == "a"
            && modifiers.secondary()
            && !modifiers.shift
            && !modifiers.alt
            && !self.overlay_owns_keyboard(cx)
        {
            self.select_all_sidebar_rows(cx);
            cx.stop_propagation();
        }
    }

    /// Focus the sidebar on a press inside it, so Cmd+A selects rows. A field
    /// inside it (rename, typed snooze) still takes focus for itself.
    pub(super) fn capture_sidebar_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::navigation_focus::focus_navigation_scope(&self.sidebar_focus, window, cx);
    }

    /// The bar at the foot of the sidebar while rows are selected.
    pub(super) fn render_sidebar_bulk_bar(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.sidebar_selection.is_empty() {
            return None;
        }
        let rows = self.bulk_rows(cx);
        let toggles = bulk_toggles(&rows);
        let button = |id: &'static str,
                      glyph: &'static str,
                      label: &'static str,
                      danger: bool,
                      enabled: bool| {
            let fade = format!("sidebar-bulk-{id}");
            let rest = if danger { theme.danger } else { theme.text_muted };
            let hover = if danger { theme.danger } else { theme.text };
            div()
                .id(SharedString::from(fade.clone()))
                .debug_selector({
                    let fade = fade.clone();
                    move || fade.clone()
                })
                .role(gpui::Role::Button)
                .aria_label(label)
                .tab_index(0)
                .focus_visible(|style| style.border_2().border_color(theme.accent))
                .size(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .when(!enabled, |el| el.opacity(0.4).cursor_default())
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .bg(motion::hover_blend(
                            &fade,
                            theme.glass_hover().opacity(0.0),
                            theme.glass_hover(),
                        ))
                        .on_hover(motion::hover_listener(fade.clone()))
                })
                .tooltip(crate::settings::widgets::text_tooltip_above(label))
                .child(icon(glyph).size(px(15.0)).text_color(if enabled {
                    motion::hover_blend(&fade, rest, hover)
                } else {
                    rest
                }))
        };
        let settle_action = if toggles.settle {
            BulkAction::Settle
        } else {
            BulkAction::Unsettle
        };
        let settle_enabled = !bulk_targets(settle_action, &rows).is_empty();
        let snooze_enabled = rows.iter().any(|row| !row.archived && !row.live);
        let read_action = if toggles.read {
            BulkAction::MarkRead
        } else {
            BulkAction::MarkUnread
        };
        let pin_action = if toggles.pin {
            BulkAction::Pin
        } else {
            BulkAction::Unpin
        };
        let pin_enabled = !bulk_targets(pin_action, &rows).is_empty();
        let archive_enabled = !bulk_targets(BulkAction::Archive, &rows).is_empty();
        let count = self.sidebar_selection.len();
        let bar = div()
            .id("sidebar-bulk-bar")
            .debug_selector(|| "sidebar-bulk-bar".into())
            .mx(px(Theme::SPACE_SM))
            .mb(px(Theme::SPACE_SM))
            .pl(px(Theme::SPACE_SM))
            .pr(px(4.0))
            .py(px(4.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(theme.accent_wash)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(format!("{count} selected"))),
            )
            .child(
                button(
                    "settle",
                    icons::CHECKLIST,
                    if toggles.settle { "Settle" } else { "Unsettle" },
                    false,
                    settle_enabled,
                )
                .when(settle_enabled, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_bulk_action(settle_action, cx)
                    }))
                }),
            )
            .child(
                button(
                    "snooze",
                    icons::CLOCK_CIRCLE,
                    "Snooze",
                    false,
                    snooze_enabled,
                )
                .when(snooze_enabled, |el| {
                    el.on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        this.bulk_snooze_menu = match this.bulk_snooze_menu {
                            Some(_) => None,
                            None => Some(event.position()),
                        };
                        cx.notify();
                    }))
                }),
            )
            .child(
                button(
                    "read",
                    if toggles.read {
                        icons::EYE
                    } else {
                        icons::EYE_CLOSED
                    },
                    if toggles.read {
                        "Mark as read"
                    } else {
                        "Mark as unread"
                    },
                    false,
                    true,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.apply_bulk_action(read_action, cx)),
                ),
            )
            .child(
                button(
                    "pin",
                    icons::PIN,
                    if toggles.pin { "Pin" } else { "Unpin" },
                    false,
                    pin_enabled,
                )
                .when(pin_enabled, |el| {
                    el.on_click(
                        cx.listener(move |this, _, _, cx| this.apply_bulk_action(pin_action, cx)),
                    )
                }),
            )
            .child(
                button(
                    "archive",
                    icons::ARCHIVE_MINIMALISTIC,
                    "Archive",
                    false,
                    archive_enabled,
                )
                .when(archive_enabled, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| {
                        this.apply_bulk_action(BulkAction::Archive, cx)
                    }))
                }),
            )
            .child(
                button(
                    "delete",
                    icons::TRASH_BIN_MINIMALISTIC,
                    "Delete…",
                    true,
                    true,
                )
                .on_click(cx.listener(|this, _, _, cx| this.request_bulk_delete(cx))),
            )
            .child(
                button("clear", icons::CLOSE, "Clear selection", false, true).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.clear_sidebar_selection(cx);
                    }),
                ),
            );
        Some(bar.into_any_element())
    }

    /// The bar's Snooze presets, floating where the button was pressed.
    pub(super) fn render_bulk_snooze_menu(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let position: Point<Pixels> = self.bulk_snooze_menu?;
        let now = Utc::now();
        let local = Local::now();
        let mut card = popover::popover_card(theme)
            .w(px(244.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.bulk_snooze_menu = None;
                cx.notify();
            }));
        for preset in SnoozePreset::ALL {
            let Some(until) = preset.until(&local) else {
                continue;
            };
            let slug = preset.label().to_lowercase().replace(' ', "-");
            card = card.child(
                popover::menu_row(theme, false, format!("sidebar-bulk-snooze-{slug}"))
                    .id(SharedString::from(format!("sidebar-bulk-snooze-{slug}")))
                    .debug_selector({
                        let slug = slug.clone();
                        move || format!("sidebar-bulk-snooze-{slug}")
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_bulk_action(BulkAction::Snooze(until), cx)
                    }))
                    .child(div().flex_1().child(SharedString::from(preset.label())))
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(local_moment_label(until, now))),
                    ),
            );
        }
        Some(popover::menu_at(
            "sidebar-bulk-snooze-menu",
            position,
            card.into_any_element(),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn modifiers_pick_toggle_extend_or_open() {
        let mut toggle = gpui::Modifiers::none();
        if cfg!(target_os = "macos") {
            toggle.platform = true;
        } else {
            toggle.control = true;
        }
        assert_eq!(row_click(&toggle), RowClick::Toggle);
        assert_eq!(row_click(&gpui::Modifiers::shift()), RowClick::Extend);
        assert_eq!(row_click(&gpui::Modifiers::none()), RowClick::Open);
    }

    #[test]
    fn toggle_adds_and_removes_and_moves_the_anchor() {
        let mut selection = SidebarSelection::default();
        selection.toggle("a");
        selection.toggle("c");
        assert_eq!(selection.ids, ids(&["a", "c"]));
        assert_eq!(selection.anchor.as_deref(), Some("c"));
        selection.toggle("c");
        assert_eq!(selection.ids, ids(&["a"]));
        assert_eq!(selection.anchor.as_deref(), Some("a"));
        selection.toggle("a");
        assert!(selection.is_empty());
        assert_eq!(selection.anchor, None);
    }

    #[test]
    fn shift_extends_from_the_anchor_in_screen_order_either_way() {
        let order = ids(&["a", "b", "c", "d", "e"]);
        let mut selection = SidebarSelection::default();
        selection.toggle("b");
        selection.extend_to("d", &order, None);
        assert_eq!(selection.ordered(&order), ids(&["b", "c", "d"]));
        // A second Shift-click keeps the anchor and grows the other way too.
        selection.extend_to("a", &order, None);
        assert_eq!(selection.ordered(&order), ids(&["a", "b", "c", "d"]));
        assert_eq!(selection.anchor.as_deref(), Some("b"));
    }

    #[test]
    fn shift_without_an_anchor_starts_at_the_open_chat() {
        let order = ids(&["a", "b", "c", "d"]);
        let mut selection = SidebarSelection::default();
        selection.extend_to("c", &order, Some("a"));
        assert_eq!(selection.ordered(&order), ids(&["a", "b", "c"]));
        // Nothing open and no anchor: only the row itself.
        let mut selection = SidebarSelection::default();
        selection.extend_to("c", &order, None);
        assert_eq!(selection.ordered(&order), ids(&["c"]));
        // A shelf row (not in the visible order) is added on its own.
        selection.extend_to("settled", &order, None);
        assert_eq!(selection.ordered(&order), ids(&["c", "settled"]));
    }

    #[test]
    fn select_all_clear_and_retain() {
        let order = ids(&["a", "b", "c"]);
        let mut selection = SidebarSelection::default();
        selection.select_all(&order);
        assert_eq!(selection.len(), 3);
        assert!(selection.retain(|id| id != "b"));
        assert_eq!(selection.ordered(&order), ids(&["a", "c"]));
        assert!(!selection.retain(|_| true));
        assert!(selection.clear());
        assert!(!selection.clear(), "clearing twice reports nothing");
    }

    fn row(id: &str) -> BulkRow {
        BulkRow {
            id: id.to_owned(),
            lifecycle: LifecycleState::Active,
            unseen: false,
            pinned: false,
            live: false,
            archived: false,
            side_chat: false,
        }
    }

    #[test]
    fn toggles_point_at_what_some_row_still_needs() {
        let mut settled = row("a");
        settled.lifecycle = LifecycleState::Settled;
        settled.pinned = true;
        let mut other = row("b");
        other.lifecycle = LifecycleState::Settled;
        other.pinned = true;
        assert_eq!(
            bulk_toggles(&[settled.clone(), other.clone()]),
            BulkToggles {
                settle: false,
                read: false,
                pin: false,
            }
        );
        other.lifecycle = LifecycleState::Active;
        other.unseen = true;
        other.pinned = false;
        assert_eq!(
            bulk_toggles(&[settled, other]),
            BulkToggles {
                settle: true,
                read: true,
                pin: true,
            }
        );
    }

    #[test]
    fn targets_skip_rows_the_row_menu_would_not_offer() {
        let mut running = row("running");
        running.live = true;
        let mut settled = row("settled");
        settled.lifecycle = LifecycleState::Settled;
        let mut side = row("side");
        side.side_chat = true;
        let mut archived = row("archived");
        archived.archived = true;
        let rows = vec![row("idle"), running, settled, side, archived];
        assert_eq!(
            bulk_targets(BulkAction::Settle, &rows),
            ids(&["idle", "side"])
        );
        assert_eq!(bulk_targets(BulkAction::Unsettle, &rows), ids(&["settled"]));
        let until = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(
            bulk_targets(BulkAction::Snooze(until), &rows),
            ids(&["idle", "settled", "side"])
        );
        assert_eq!(
            bulk_targets(BulkAction::Pin, &rows),
            ids(&["idle", "running", "settled", "archived"])
        );
        assert_eq!(
            bulk_targets(BulkAction::Archive, &rows),
            ids(&["idle", "running", "settled"])
        );
    }

    mod shell {
        use super::super::*;
        use crate::settings::SidebarOrganization;
        use crate::state::{AppState, EngineBootConfig};
        use gpui::TestAppContext;
        use zeron_proto::WorkspaceScope;

        fn test_shell(
            cx: &mut TestAppContext,
            path: &std::path::Path,
        ) -> gpui::WindowHandle<Shell> {
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
            });
            cx.add_window(|_, cx| {
                let state = cx.new(|_| AppState::new());
                Shell::new(
                    state,
                    EngineBootConfig {
                        data_dir: path.into(),
                        ipc_port: 0,
                        edge_url: "http://127.0.0.1:1".into(),
                        edge_token: None,
                        org_id: None,
                        workos_client_id: None,
                        default_harness: zeron_proto::HarnessId::Mock,
                    },
                    cx,
                )
            })
        }

        fn chat(id: &str, minutes_ago: i64) -> zeron_proto::Chat {
            let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
            serde_json::from_value(serde_json::json!({
                "id": id,
                "title": id,
                "deviceId": "local",
                "archived": false,
                "createdAt": at,
                "lastMessageAt": at,
                "lastSeenAt": at,
            }))
            .unwrap()
        }

        fn prepare(shell: &mut Shell, cx: &mut Context<Shell>) {
            shell.state.update(cx, |state, _| {
                state.workspace_scope = Some(WorkspaceScope::Local);
                state.local_device_id = Some("local".into());
                state.chats_synced = true;
                state.chats = vec![chat("a", 1), chat("b", 2), chat("c", 3), chat("d", 4)];
            });
            shell.settings.space_filter = None;
            shell.settings.sidebar_organization = SidebarOrganization::InOneList;
        }

        fn lifecycle(shell: &Shell, id: &str, cx: &gpui::App) -> LifecycleState {
            shell
                .state
                .read(cx)
                .chats
                .iter()
                .find(|chat| chat.id == id)
                .map(|chat| chat.lifecycle.at(Utc::now()))
                .unwrap()
        }

        fn toggle() -> RowClick {
            RowClick::Toggle
        }

        #[gpui::test]
        fn clicks_select_and_escape_or_navigation_clear(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let window = test_shell(cx, dir.path());
            window
                .update(cx, |shell, _, cx| {
                    prepare(shell, cx);
                    assert_eq!(shell.sidebar_visible_order(cx), ["a", "b", "c", "d"]);
                    assert!(!shell.select_sidebar_row("b", RowClick::Open, cx));
                    assert!(shell.select_sidebar_row("b", toggle(), cx));
                    assert!(shell.select_sidebar_row("d", RowClick::Extend, cx));
                    let order = shell.sidebar_visible_order(cx);
                    assert_eq!(shell.sidebar_selection.ordered(&order), ["b", "c", "d"]);
                    assert!(shell.escape_sidebar_selection(cx));
                    assert!(shell.sidebar_selection.is_empty());
                    assert!(!shell.escape_sidebar_selection(cx), "nothing left to clear");

                    shell.select_all_sidebar_rows(cx);
                    assert_eq!(shell.sidebar_selection.len(), 4);
                    // Opening a chat by any route clears the selection.
                    shell
                        .state
                        .update(cx, |state, cx| state.select_chat(Some("c".into()), cx));
                    shell.reconcile_sidebar_selection(cx);
                    assert!(shell.sidebar_selection.is_empty());

                    // Deleted rows fall out; the rest stay.
                    shell.select_sidebar_row("a", toggle(), cx);
                    shell.select_sidebar_row("b", toggle(), cx);
                    shell
                        .state
                        .update(cx, |state, _| state.chats.retain(|chat| chat.id != "a"));
                    shell.reconcile_sidebar_selection(cx);
                    assert_eq!(shell.sidebar_selection.ordered(&[]), ["b"]);
                })
                .unwrap();
        }

        #[gpui::test]
        fn bulk_settle_skips_running_rows_and_undoes_as_one_step(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let window = test_shell(cx, dir.path());
            window
                .update(cx, |shell, _, cx| {
                    prepare(shell, cx);
                    shell.debug_gate = Some(crate::state::GatePhase::Ready);
                    for id in ["a", "b", "c"] {
                        shell.select_sidebar_row(id, toggle(), cx);
                    }
                    shell.apply_bulk_action(BulkAction::Settle, cx);
                    for id in ["a", "b", "c"] {
                        assert_eq!(lifecycle(shell, id, cx), LifecycleState::Settled);
                    }
                    assert_eq!(lifecycle(shell, "d", cx), LifecycleState::Active);
                    let Some(LifecycleUndo::Batch(undos)) = shell.lifecycle_undo.clone() else {
                        panic!("one batch undo, got {:?}", shell.lifecycle_undo);
                    };
                    assert_eq!(undos.len(), 3);
                })
                .unwrap();
            let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
            cx.run_until_parked();
            window
                .update(&mut cx, |shell, window, cx| {
                    window.focus(&shell.unfocused, cx)
                })
                .unwrap();
            cx.dispatch_action(crate::composer::Undo);
            cx.run_until_parked();
            window
                .update(&mut cx, |shell, _, cx| {
                    for id in ["a", "b", "c"] {
                        assert_eq!(lifecycle(shell, id, cx), LifecycleState::Active);
                    }
                    assert!(shell.lifecycle_undo.is_none(), "the batch is one undo");
                })
                .unwrap();
        }

        #[gpui::test]
        fn bulk_snooze_and_unread_group_their_undo_and_delete_confirms_once(
            cx: &mut TestAppContext,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let window = test_shell(cx, dir.path());
            window
                .update(cx, |shell, _, cx| {
                    prepare(shell, cx);
                    shell.select_sidebar_row("a", toggle(), cx);
                    shell.select_sidebar_row("c", toggle(), cx);
                    let until = Utc::now() + chrono::Duration::hours(3);
                    shell.apply_bulk_action(BulkAction::Snooze(until), cx);
                    assert_eq!(
                        lifecycle(shell, "c", cx),
                        LifecycleState::Snoozed { until }
                    );
                    shell.undo_lifecycle_action(cx);
                    assert_eq!(lifecycle(shell, "a", cx), LifecycleState::Active);
                    assert_eq!(lifecycle(shell, "c", cx), LifecycleState::Active);

                    shell.apply_bulk_action(BulkAction::MarkUnread, cx);
                    let unseen = |shell: &Shell, cx: &gpui::App| {
                        shell
                            .state
                            .read(cx)
                            .chats
                            .iter()
                            .filter(|chat| chat.unseen())
                            .map(|chat| chat.id.clone())
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(unseen(shell, cx), ["a", "c"]);
                    assert!(matches!(
                        shell.lifecycle_undo,
                        Some(LifecycleUndo::Batch(ref undos)) if undos.len() == 2
                    ));
                    shell.undo_lifecycle_action(cx);
                    assert!(unseen(shell, cx).is_empty());

                    // Mark as read has no undo, one row at a time or many.
                    shell.apply_bulk_action(BulkAction::MarkUnread, cx);
                    shell.apply_bulk_action(BulkAction::MarkRead, cx);
                    assert!(unseen(shell, cx).is_empty());

                    shell.request_bulk_delete(cx);
                    let confirm = shell.delete_confirm.as_ref().expect("one confirmation");
                    assert_eq!(confirm.chat_ids, ["a", "c"]);
                })
                .unwrap();
        }

        #[gpui::test]
        fn bulk_pin_keeps_screen_order_and_archive_clears(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let window = test_shell(cx, dir.path());
            window
                .update(cx, |shell, _, cx| {
                    prepare(shell, cx);
                    shell.select_sidebar_row("c", toggle(), cx);
                    shell.select_sidebar_row("b", toggle(), cx);
                    shell.apply_bulk_action(BulkAction::Pin, cx);
                    assert_eq!(shell.active_sidebar_pins(cx), ["b", "c"]);
                    let rows = shell.bulk_rows(cx);
                    assert!(!bulk_toggles(&rows).pin, "every row is pinned: Unpin");
                    shell.apply_bulk_action(BulkAction::Unpin, cx);
                    assert!(shell.active_sidebar_pins(cx).is_empty());
                    shell.apply_bulk_action(BulkAction::Archive, cx);
                    assert!(shell.sidebar_selection.is_empty());
                })
                .unwrap();
        }
    }
}
