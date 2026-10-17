//! Mission Control's place in the shell: the full-window overlay it mounts
//! as, the sidebar entry under Inbox, and opening and closing it.

use gpui::{
    AnyElement, Context, FocusHandle, Pixels, SharedString, Subscription, Window, div, prelude::*,
    px,
};

use super::Shell;
use crate::mission_control::{MissionControl, MissionControlEvent};
use crate::motion;
use crate::popover;
use crate::theme::Theme;

const HOVER_KEY: &str = "sidebar-mission-control";

pub(super) struct MissionControlOverlay {
    view: gpui::Entity<MissionControl>,
    /// Where the keyboard was, for handing it back on Esc.
    previous_focus: Option<FocusHandle>,
    _events: Subscription,
}

impl Shell {
    pub(super) fn toggle_mission_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mission_control.is_some() {
            self.close_mission_control(window, cx);
        } else {
            self.open_mission_control(window, cx);
        }
    }

    pub(super) fn open_mission_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mission_control.is_some() {
            return;
        }
        if self.command_palette.is_some() {
            self.close_command_palette(window, cx);
        }
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        let previous_focus = window.focused(cx);
        let diffs = self.diffs.values().cloned().collect();
        let view = cx.new(|cx| MissionControl::new(self.state.clone(), diffs, cx));
        let events = cx.subscribe_in(
            &view,
            window,
            |this: &mut Shell, _, event: &MissionControlEvent, window, cx| match event {
                MissionControlEvent::OpenChat(chat_id) => {
                    // The chat takes the keyboard; nothing to hand back.
                    this.mission_control = None;
                    this.open_chat(chat_id.clone(), cx);
                }
                MissionControlEvent::Close => this.close_mission_control(window, cx),
            },
        );
        self.mission_control = Some(MissionControlOverlay {
            view,
            previous_focus,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn close_mission_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(overlay) = self.mission_control.take() {
            match overlay.previous_focus {
                Some(focus) => window.focus(&focus, cx),
                None => self.focus_composer(cx),
            }
            cx.notify();
        }
    }

    /// The overlay: the window blurred under the palette's glass, with the
    /// board as a cached view so its own animation frames repaint only it.
    pub(super) fn render_mission_control_overlay(
        &self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.mission_control.as_ref()?.view.clone();
        let theme = Theme::of(cx).for_popup();
        let glass = div()
            .w(viewport.width)
            .h(viewport.height)
            .bg(popover::surface_bg(&theme))
            .child(view.cached(gpui::StyleRefinement::default().size_full()));
        let glass = crate::frost::frosted(0.0, crate::frost::MENU_BLUR * 1.5, glass);
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(px(0.0), px(0.0)))
                    .child(
                        div()
                            .occlude()
                            .w(viewport.width)
                            .h(viewport.height)
                            .bg(popover::scrim_alpha(0.35))
                            .child(motion::fade_in(
                                "mission-control-in",
                                div().size_full().child(glass),
                            )),
                    ),
            )
            .priority(1)
            .into_any_element(),
        )
    }

    /// The sidebar row under Inbox, with a live count of working agents.
    pub(super) fn render_mission_control_entry(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = chrono::Utc::now();
        let (working, waiting) =
            crate::speedster::session_activity(&self.state.read(cx).sessions, now);
        let active = self.mission_control.is_some();
        let entry = div()
            .id("sidebar-mission-control")
            .role(gpui::Role::Button)
            .aria_label("Mission Control")
            .tab_index(0)
            .flex_1()
            .min_w_0()
            .h(px(29.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .text_size(crate::typography::ui_rems(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(if active {
                theme.text
            } else {
                motion::hover_blend(HOVER_KEY, theme.text.opacity(0.8), theme.text)
            })
            .bg(if active {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    HOVER_KEY,
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                )
            })
            .on_hover(motion::hover_listener(HOVER_KEY))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| this.open_mission_control(window, cx)))
            .child(
                crate::icons::icon(crate::icons::WIDGET)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(SharedString::from("Mission Control")),
            )
            .when(working + waiting > 0, |el| {
                el.child(
                    super::spaces::sidebar_count_badge(theme, working + waiting).text_color(
                        if waiting > 0 {
                            theme.warning
                        } else {
                            theme.busy
                        },
                    ),
                )
            });
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .px(px(Theme::SPACE_SM))
            .pt(px(2.0))
            .child(entry)
            .into_any_element()
    }
}
