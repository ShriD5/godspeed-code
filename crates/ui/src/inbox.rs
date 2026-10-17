//! The "Needs you" inbox page: the engine's attention queue
//! (`WatchAttention`, mirrored on [`AppState::attention`]) grouped by kind.
//! Approvals show the exact request and answer in place; everything else
//! opens its chat. Keyboard-first (see [`navigation`]).

mod navigation;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use serde_json::json;
use zeron_proto::attention::{AttentionAction, AttentionItem, AttentionKind, BatchApproval};
use zeron_proto::{ApprovalRisk, HarnessId};
use zeron_rpc::methods;

use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub(crate) use navigation::InboxCommand;
use navigation::{command_for, display_order, grouped, reconcile, step};

pub enum InboxEvent {
    OpenChat(String),
}

pub struct InboxPage {
    state: Entity<AppState>,
    focus: FocusHandle,
    selected: Option<String>,
    /// Where the selection was, for landing on the next row once answered.
    selected_index: usize,
    scroll: widgets::PageScroll,
    task: Option<Task<()>>,
    error: Option<String>,
    _state: Subscription,
}

impl EventEmitter<InboxEvent> for InboxPage {}

impl Focusable for InboxPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The colour a risk reads in (theme status tokens).
pub(crate) fn risk_color(risk: ApprovalRisk, theme: &Theme) -> gpui::Hsla {
    match risk {
        ApprovalRisk::Low => theme.success,
        ApprovalRisk::Medium => theme.warning,
        ApprovalRisk::High => theme.danger,
    }
}

/// The sidebar badge count: everything waiting.
pub(crate) fn inbox_count(state: &AppState) -> usize {
    state.attention.items.len()
}

impl InboxPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&state, |page: &mut Self, _, cx| {
            page.reconcile_selection(cx);
            cx.notify();
        });
        let mut page = Self {
            state,
            focus: cx.focus_handle(),
            selected: None,
            selected_index: 0,
            scroll: widgets::PageScroll::default(),
            task: None,
            error: None,
            _state: sub,
        };
        page.reconcile_selection(cx);
        page
    }

    fn items(&self, cx: &gpui::App) -> Vec<AttentionItem> {
        self.state.read(cx).attention.items.clone()
    }

    fn reconcile_selection(&mut self, cx: &mut Context<Self>) {
        let order = display_order(&self.items(cx));
        self.selected = reconcile(&order, self.selected.as_deref(), self.selected_index);
        self.remember_index(&order);
    }

    fn remember_index(&mut self, order: &[String]) {
        if let Some(at) = self
            .selected
            .as_ref()
            .and_then(|id| order.iter().position(|o| o == id))
        {
            self.selected_index = at;
        }
    }

    fn selected_item(&self, cx: &gpui::App) -> Option<AttentionItem> {
        let id = self.selected.as_ref()?;
        self.items(cx).into_iter().find(|item| &item.id == id)
    }

    pub(crate) fn run(&mut self, command: InboxCommand, cx: &mut Context<Self>) {
        let order = display_order(&self.items(cx));
        match command {
            InboxCommand::Next | InboxCommand::Previous => {
                let delta = if command == InboxCommand::Next { 1 } else { -1 };
                self.selected = step(&order, self.selected.as_deref(), delta);
                self.remember_index(&order);
                cx.notify();
            }
            InboxCommand::Approve => {
                if let Some(item) = self.selected_item(cx) {
                    self.answer(&item, true, cx);
                }
            }
            InboxCommand::Decline => {
                let Some(item) = self.selected_item(cx) else {
                    return;
                };
                if item.kind == AttentionKind::Approval {
                    self.answer(&item, false, cx);
                } else if item.supports(AttentionAction::Dismiss) {
                    self.dismiss(&item, cx);
                }
            }
            InboxCommand::Open => {
                if let Some(item) = self.selected_item(cx) {
                    self.open(&item, cx);
                }
            }
            InboxCommand::ApproveLowRisk => self.approve_low_risk(cx),
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let other = modifiers.platform || modifiers.control || modifiers.alt || modifiers.function;
        if let Some(command) = command_for(keystroke.key.as_str(), modifiers.shift, other) {
            self.run(command, cx);
            cx.stop_propagation();
        }
    }

    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut params = params;
        let device = self.state.read(cx).attention.device_id.clone();
        if !device.is_empty() {
            params["targetDeviceId"] = json!(device);
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(reply) if method == methods::APPROVE_LOW_RISK => {
                        let batch: BatchApproval =
                            serde_json::from_value(reply).unwrap_or_default();
                        if batch.approved.is_empty() && !batch.skipped.is_empty() {
                            page.error =
                                Some("Those requests changed; nothing was approved.".into());
                        }
                    }
                    Ok(_) => {}
                    Err(err) => page.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Approve or decline with the decision Enter or Esc would pick on the
    /// approval card.
    fn answer(&mut self, item: &AttentionItem, approve: bool, cx: &mut Context<Self>) {
        let Some(approval) = &item.approval else {
            return;
        };
        let Some(option) = crate::composer::approval_default_index(approval, approve)
            .and_then(|ix| approval.decisions.get(ix))
        else {
            return;
        };
        self.answer_with(item, option.decision, cx);
    }

    fn answer_with(
        &mut self,
        item: &AttentionItem,
        decision: zeron_proto::ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        self.call(
            methods::RESPOND_ATTENTION,
            json!({ "itemId": item.id, "decision": decision }),
            cx,
        );
    }

    fn approve_low_risk(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .items(cx)
            .iter()
            .filter(|item| item.batch_approvable())
            .map(|item| item.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.call(methods::APPROVE_LOW_RISK, json!({ "itemIds": ids }), cx);
    }

    fn dismiss(&mut self, item: &AttentionItem, cx: &mut Context<Self>) {
        self.call(methods::DISMISS_ATTENTION, json!({ "itemId": item.id }), cx);
    }

    /// Open the item's chat; a finished, failed or timed-out item counts as
    /// seen once opened.
    fn open(&mut self, item: &AttentionItem, cx: &mut Context<Self>) {
        if item.supports(AttentionAction::Dismiss) {
            self.dismiss(item, cx);
        }
        if let Some(chat_id) = &item.chat_id {
            cx.emit(InboxEvent::OpenChat(chat_id.clone()));
        }
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn render_row(
        &self,
        item: &AttentionItem,
        ix: usize,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(item.id.as_str());
        let now = chrono::Utc::now();
        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(project) = &item.project {
            meta.push(SharedString::from(project.clone()).into_any_element());
        }
        if let Some(run) = &item.agent_run {
            meta.push(SharedString::from(run.agent_name.clone()).into_any_element());
        } else if let Some(harness) = item.harness {
            meta.push(SharedString::from(harness_label(harness)).into_any_element());
        }
        meta.push(
            SharedString::from(zeron_proto::view::format_time_ago(item.created_at, now))
                .into_any_element(),
        );
        let risk = item.risk.map(|risk| {
            let color = risk_color(risk, theme);
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.0))
                .px(px(8.0))
                .py(px(2.0))
                .rounded_full()
                .bg(color.opacity(0.12))
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(color)
                .child(div().size(px(6.0)).rounded_full().bg(color))
                .child(SharedString::from(risk.label()))
        });
        let heading = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, item.title.clone()).truncate())
                    .child(widgets::meta_line(theme, meta)),
            )
            .children(risk);
        let summary = (item.approval.is_none() && !item.summary.is_empty()).then(|| {
            div()
                .w_full()
                .min_w_0()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text_muted)
                .truncate()
                .child(SharedString::from(item.summary.clone()))
        });
        let details = item
            .approval
            .as_ref()
            .map(|approval| {
                let mut out = vec![
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from(approval.title.clone()))
                        .into_any_element(),
                ];
                out.extend(crate::approval_view::approval_details(
                    approval,
                    theme,
                    &format!("inbox-{ix}"),
                ));
                out
            })
            .unwrap_or_default();
        let actions = self.render_actions(item, ix, selected, theme, cx);
        let id = item.id.clone();
        widgets::card_row(theme, first)
            .id(("inbox-row", ix))
            .flex_col()
            .items_start()
            .gap(px(8.0))
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.wash(0.06)).rounded(px(8.0)))
            .on_click(cx.listener(move |page, _, window, cx| {
                page.selected = Some(id.clone());
                window.focus(&page.focus, cx);
                cx.notify();
            }))
            .child(heading)
            .children(summary)
            .child(div().w_full().min_w_0().flex().flex_col().children(details))
            .child(actions)
            .into_any_element()
    }

    fn render_actions(
        &self,
        item: &AttentionItem,
        ix: usize,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hint = |key: &'static str| {
            div()
                .when(!selected, |el| el.opacity(0.0))
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(theme.text_muted.opacity(0.6))
                .child(SharedString::from(key))
        };
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(8.0));
        if item.kind == AttentionKind::Approval {
            if item.supports(AttentionAction::Approve) {
                let target = item.clone();
                row = row.child(
                    popover::btn_primary(theme, "Approve")
                        .id(("inbox-approve", ix))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(hint("A"))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            cx.stop_propagation();
                            page.answer(&target, true, cx);
                        })),
                );
            }
            let target = item.clone();
            row = row.child(
                popover::btn_ghost(theme, "Decline", format!("inbox-decline-{ix}"))
                    .id(("inbox-decline", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(hint("D"))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.answer(&target, false, cx);
                    })),
            );
        }
        if item.chat_id.is_some() {
            let target = item.clone();
            row = row.child(
                widgets::text_action(theme, widgets::ActionTone::Quiet, "Open chat")
                    .id(("inbox-open", ix))
                    .child(hint("↵"))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.open(&target, cx);
                    })),
            );
        }
        if item.supports(AttentionAction::Dismiss) {
            let target = item.clone();
            row = row.child(
                widgets::text_action(theme, widgets::ActionTone::Quiet, "Mark seen")
                    .id(("inbox-dismiss", ix))
                    .child(hint("D"))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.dismiss(&target, cx);
                    })),
            );
        }
        row.into_any_element()
    }

    fn render_header(
        &self,
        items: &[AttentionItem],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let low_risk = items.iter().filter(|i| i.batch_approvable()).count();
        let batch = (low_risk > 0).then(|| {
            widgets::action_button(theme, widgets::ActionTone::Outlined)
                .id("inbox-approve-low-risk")
                .flex_none()
                .on_click(cx.listener(|page, _, _, cx| page.approve_low_risk(cx)))
                .child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(risk_color(ApprovalRisk::Low, theme)),
                )
                .child(SharedString::from(format!("Approve {low_risk} low risk")))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from("⇧A")),
                )
        });
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(16.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(widgets::page_header(
                        theme,
                        "Inbox",
                        (!items.is_empty()).then_some(items.len()),
                    ))
                    .child(widgets::page_subtitle(
                        theme,
                        "J/K to move · A approve · D decline or mark seen · Return opens the chat",
                    )),
            )
            .children(batch)
            .into_any_element()
    }
}

fn harness_label(harness: HarnessId) -> &'static str {
    crate::shell::harness_updates::agent_name(harness)
}

impl popover::ScrollRailHost for InboxPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for InboxPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).clone();
        let items = self.items(cx);
        let header = self.render_header(&items, &theme, cx);
        let mut body: Vec<AnyElement> = Vec::new();
        let mut ix = 0;
        for (kind, group) in grouped(&items) {
            let mut card = widgets::section_card(&theme).mt(px(0.0));
            for (row, item) in group.into_iter().enumerate() {
                card = card.child(self.render_row(item, ix, row == 0, &theme, cx));
                ix += 1;
            }
            body.push(widgets::section(&theme, kind.label(), card).into_any_element());
        }
        if body.is_empty() {
            body.push(
                div()
                    .id("inbox-empty")
                    .py(px(64.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(18.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Nothing needs you")),
                    )
                    .into_any_element(),
            );
        }
        let scrollbar = popover::rail(self, "inbox-page-scrollbar", &theme, cx);
        div()
            .id("inbox-page-host")
            .key_context("Inbox")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("inbox-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(widgets::page_column().child(header).children(body)),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use gpui::TestAppContext;
    use zeron_proto::attention::AttentionSnapshot;

    fn item(id: &str, kind: AttentionKind) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind,
            chat_id: Some(format!("chat-{id}")),
            title: id.into(),
            space_id: None,
            project: None,
            harness: None,
            created_at: Utc.with_ymd_and_hms(2026, 10, 17, 9, 0, 0).unwrap(),
            risk: None,
            summary: String::new(),
            actions: vec![AttentionAction::Open, AttentionAction::Dismiss],
            request_id: None,
            approval: None,
            agent_run: None,
        }
    }

    fn snapshot(items: Vec<AttentionItem>) -> AttentionSnapshot {
        AttentionSnapshot {
            device_id: "dev".into(),
            items,
        }
    }

    #[gpui::test]
    fn the_selection_walks_the_queue_and_survives_answered_items(cx: &mut TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.apply_attention(snapshot(vec![
                item("a", AttentionKind::Question),
                item("b", AttentionKind::Failed),
                item("c", AttentionKind::Finished),
            ]));
        });
        let page = cx.new(|cx| InboxPage::new(state.clone(), cx));
        page.update(cx, |page, cx| {
            assert_eq!(page.selected.as_deref(), Some("a"));
            page.run(InboxCommand::Next, cx);
            page.run(InboxCommand::Next, cx);
            page.run(InboxCommand::Next, cx);
            assert_eq!(page.selected.as_deref(), Some("c"));
            page.run(InboxCommand::Previous, cx);
            assert_eq!(page.selected.as_deref(), Some("b"));
        });
        state.update(cx, |state, cx| {
            state.apply_attention(snapshot(vec![
                item("a", AttentionKind::Question),
                item("c", AttentionKind::Finished),
            ]));
            cx.notify();
        });
        cx.run_until_parked();
        page.update(cx, |page, _| {
            assert_eq!(page.selected.as_deref(), Some("c"))
        });
    }
}
