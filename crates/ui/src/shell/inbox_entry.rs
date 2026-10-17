//! The Inbox entry pinned at the top of the sidebar, with the live count of
//! what waits on the user, and the Inbox route it opens.

use gpui::{AnyElement, Context, Focusable as _, SharedString, Window, div, prelude::*, px};

use super::{NavEntry, Route, Shell};
use crate::inbox::{InboxEvent, InboxPage, inbox_count};
use crate::motion;
use crate::theme::Theme;

const HOVER_KEY: &str = "sidebar-inbox";

impl Shell {
    pub(super) fn open_inbox(&mut self, cx: &mut Context<Self>) {
        self.enter_inbox_route(cx);
        if self.nav.current() != &NavEntry::Inbox {
            self.nav.push(NavEntry::Inbox);
        }
        cx.notify();
    }

    /// Show the inbox without recording a navigation.
    pub(super) fn enter_inbox_route(&mut self, cx: &mut Context<Self>) {
        self.inbox_focus_pending = true;
        if matches!(self.route, Route::Inbox) {
            return;
        }
        self.set_route(Route::Inbox, cx);
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn inbox_page(&mut self, cx: &mut Context<Self>) -> gpui::Entity<InboxPage> {
        if let Some(page) = &self.inbox_page {
            return page.clone();
        }
        let page = cx.new(|cx| InboxPage::new(self.state.clone(), cx));
        self.inbox_page_sub = Some(cx.subscribe(
            &page,
            |this: &mut Shell, _, event: &InboxEvent, cx| match event {
                InboxEvent::OpenChat(chat_id) => this.open_chat(chat_id.clone(), cx),
            },
        ));
        self.inbox_page = Some(page.clone());
        page
    }

    /// The main outlet on the Inbox route.
    pub(super) fn render_inbox_outlet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = self.inbox_page(cx);
        if std::mem::take(&mut self.inbox_focus_pending) {
            window.focus(&page.focus_handle(cx), cx);
        }
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(page))
            .into_any_element()
    }

    pub(super) fn render_inbox_entry(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let count = inbox_count(self.state.read(cx));
        let active = matches!(self.route, Route::Inbox);
        let urgent = self
            .state
            .read(cx)
            .attention
            .items
            .iter()
            .any(|item| item.risk == Some(zeron_proto::ApprovalRisk::High));
        let entry = div()
            .id("sidebar-inbox")
            .role(gpui::Role::Button)
            .aria_label("Inbox")
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
            .on_click(cx.listener(|this, _, _, cx| this.open_inbox(cx)))
            .child(
                crate::icons::icon(crate::icons::INBOX)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            )
            .child(div().flex_1().min_w_0().child(SharedString::from("Inbox")))
            .when(count > 0, |el| {
                el.child(
                    super::spaces::sidebar_count_badge(theme, count)
                        .when(urgent, |badge| badge.text_color(theme.danger)),
                )
            });
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .child(entry)
            .into_any_element()
    }
}
