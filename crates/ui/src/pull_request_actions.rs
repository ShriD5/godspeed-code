//! Review and merge from the PR pane: submit a review, reply to and resolve
//! threads, merge (confirmed, pinned to the loaded head), auto-merge, ready
//! for review, close and reopen, plus the pull request's watch. Every write
//! is one explicit click: it shows progress, never retries, keeps typed text
//! on failure, shows GitHub's reason, and refreshes the pane after success.
use super::*;
use zeron_proto::pull_request_actions::{
    ChangeRequestAction, ChangeRequestActionResult, ChangeRequestMergeOptions, MergeMethod,
    PullRequestWatch, ReviewEvent, SetPullRequestWatch,
};

pub(super) struct ActionsState {
    /// The running write's progress line; one write at a time.
    pending: Option<&'static str>,
    error: Option<String>,
    notice: Option<String>,
    /// The review form is open with this verdict.
    review_event: Option<ReviewEvent>,
    review_input: Entity<crate::composer::ComposerInput>,
    /// The merge confirmation is showing for this method.
    confirm_merge: Option<MergeMethod>,
    /// The thread whose reply box is open.
    reply_thread: Option<String>,
    reply_input: Entity<crate::composer::ComposerInput>,
    merge_options: Option<ChangeRequestMergeOptions>,
    merge_loading: bool,
    watch: Option<PullRequestWatch>,
    watch_pending: bool,
    task: Option<Task<()>>,
    meta_task: Option<Task<()>>,
}

impl ActionsState {
    pub(super) fn new(cx: &mut Context<PullRequestDetailPage>) -> Self {
        let input = |placeholder: &'static str, cx: &mut Context<PullRequestDetailPage>| {
            cx.new(|cx| {
                crate::composer::ComposerInput::with_context(
                    placeholder,
                    crate::composer::MESSAGE_COMPOSER_CONTEXT,
                    cx,
                )
                .with_viewport_height(96.0)
                .with_tab_stop()
            })
        };
        Self {
            pending: None,
            error: None,
            notice: None,
            review_event: None,
            review_input: input("Leave a review comment…", cx),
            confirm_merge: None,
            reply_thread: None,
            reply_input: input("Reply to this thread…", cx),
            merge_options: None,
            merge_loading: false,
            watch: None,
            watch_pending: false,
            task: None,
            meta_task: None,
        }
    }
}

/// The words to show for a failed write: GitHub's own reason when the
/// device relayed it, otherwise the generic hint.
pub(super) fn action_failure(error: &zeron_rpc::RpcError) -> String {
    match error {
        zeron_rpc::RpcError::Failed(message) | zeron_rpc::RpcError::BadParams(message) => {
            message.clone()
        }
        other => failure_reason(other).to_owned(),
    }
}

/// The confirmation line for a merge: repository, PR, base, head and method.
pub(super) fn merge_confirmation(detail: &ChangeRequestDetail, method: MergeMethod) -> String {
    let repository = detail
        .url
        .split('/')
        .skip(3)
        .take(2)
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "{} #{} in {repository} into {} at {}?",
        method.label(),
        detail.number,
        if detail.base_ref_name.is_empty() {
            "its base"
        } else {
            &detail.base_ref_name
        },
        detail.head_ref_oid.chars().take(7).collect::<String>(),
    )
}

fn button(
    id: impl Into<SharedString>,
    label: &'static str,
    tone: widgets::ActionTone,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    let selector = id.to_string();
    widgets::action_button(theme, tone)
        .id(id)
        .debug_selector(move || selector.clone())
        .role(gpui::Role::Button)
        .aria_label(label)
        .tab_index(0)
        .flex_none()
        .border_1()
        .border_color(gpui::transparent_black())
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .when(!enabled, |el| el.opacity(0.45).cursor_default())
        .child(label)
}

impl PullRequestDetailPage {
    fn action_params(&self) -> serde_json::Value {
        let mut params = serde_json::json!({ "url": self.url });
        if let Some(target) = &self.target {
            params["targetDeviceId"] = target.clone().into();
        }
        params
    }

    /// The repository's merge settings, read when the merge controls open
    /// (and again after a write), so opening a PR costs no extra request.
    pub(super) fn load_merge_options(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.actions.error = Some("Connect to the pull request’s device first.".into());
            return;
        };
        self.actions.merge_loading = true;
        let params = self.action_params();
        let url = self.url.clone();
        self.actions.meta_task = Some(cx.spawn(async move |page, cx| {
            let result = engine
                .client()
                .call(methods::GET_CHANGE_REQUEST_MERGE_OPTIONS, params)
                .await
                .map_err(|error| {
                    format!(
                        "Couldn’t read the repository’s merge settings. {}",
                        failure_reason(&error)
                    )
                })
                .and_then(|value| {
                    serde_json::from_value::<ChangeRequestMergeOptions>(value)
                        .map_err(|_| "Update Godspeed on the selected device.".to_owned())
                });
            let _ = page.update(cx, |page, cx| {
                page.actions.merge_loading = false;
                if page.url != url {
                    return;
                }
                match result {
                    Ok(options) => page.actions.merge_options = Some(options),
                    Err(error) => page.actions.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// A loaded detail carries its engine's watch.
    pub(super) fn adopt_watch(&mut self, detail: &ChangeRequestDetail) {
        if !self.actions.watch_pending {
            self.actions.watch = detail.watch.clone();
        }
    }

    /// Run one write. Ignored while another runs (no double submits).
    pub(super) fn perform_action(&mut self, action: ChangeRequestAction, cx: &mut Context<Self>) {
        if self.actions.pending.is_some() || self.detail.is_none() {
            return;
        }
        if let Err(error) = action.validate() {
            self.actions.error = Some(error);
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.actions.error = Some("Connect to the pull request’s device first.".into());
            cx.notify();
            return;
        };
        self.actions.pending = Some(action.progress_label());
        self.actions.error = None;
        self.actions.notice = None;
        let mut params = self.action_params();
        params["action"] = serde_json::to_value(&action).unwrap_or_default();
        let url = self.url.clone();
        self.actions.task = Some(cx.spawn(async move |page, cx| {
            let result = engine
                .client()
                .call(methods::PERFORM_CHANGE_REQUEST_ACTION, params)
                .await
                .map_err(|error| action_failure(&error))
                .map(|value| {
                    serde_json::from_value::<ChangeRequestActionResult>(value)
                        .map(|result| result.message)
                        .unwrap_or_else(|_| action.done_label().to_owned())
                });
            let _ = page.update(cx, |page, cx| {
                page.finish_action(&url, &action, result, cx);
            });
        }));
        cx.notify();
    }

    pub(super) fn finish_action(
        &mut self,
        url: &str,
        action: &ChangeRequestAction,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        // A result belongs to the pull request it was sent for.
        if self.url != url {
            return;
        }
        self.actions.pending = None;
        self.actions.task = None;
        match result {
            Ok(message) => {
                self.actions.notice = Some(message);
                self.actions.confirm_merge = None;
                match action {
                    ChangeRequestAction::Review { body, .. } => {
                        if self.actions.review_input.read(cx).text() == body {
                            self.actions
                                .review_input
                                .update(cx, |input, cx| input.set_text("", cx));
                        }
                        self.actions.review_event = None;
                    }
                    ChangeRequestAction::ReplyToThread { body, .. } => {
                        if self.actions.reply_input.read(cx).text() == body {
                            self.actions
                                .reply_input
                                .update(cx, |input, cx| input.set_text("", cx));
                        }
                        self.actions.reply_thread = None;
                    }
                    _ => {}
                }
                self.cache.borrow_mut().evict(&self.target, url);
                self.refresh(cx);
                if self.actions.merge_options.is_some() {
                    self.load_merge_options(cx);
                }
            }
            // Typed text stays put; nothing retries on its own.
            Err(error) => self.actions.error = Some(error),
        }
        cx.notify();
    }

    fn owning_chat(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        state
            .chats
            .iter()
            .filter(|chat| !chat.archived)
            .find(|chat| {
                state
                    .change_request_for_chat(chat)
                    .is_some_and(|pr| pr.url == self.url)
            })
            .map(|chat| chat.id.clone())
    }

    pub(super) fn set_watch(
        &mut self,
        enabled: bool,
        auto_merge: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if self.actions.watch_pending {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = SetPullRequestWatch {
            url: self.url.clone(),
            chat_id: self
                .actions
                .watch
                .as_ref()
                .and_then(|watch| watch.chat_id.clone())
                .or_else(|| self.owning_chat(cx)),
            enabled,
            auto_merge,
        };
        let mut params = serde_json::to_value(&request).unwrap_or_default();
        if let Some(target) = &self.target {
            params["targetDeviceId"] = target.clone().into();
        }
        self.actions.watch_pending = true;
        let url = self.url.clone();
        cx.spawn(async move |page, cx| {
            let result = engine
                .client()
                .call(methods::SET_PULL_REQUEST_WATCH, params)
                .await
                .map_err(|error| action_failure(&error))
                .and_then(|value| {
                    serde_json::from_value::<Vec<PullRequestWatch>>(value)
                        .map_err(|_| "Update Godspeed on the selected device.".to_owned())
                });
            let _ = page.update(cx, |page, cx| {
                page.actions.watch_pending = false;
                if page.url != url {
                    return;
                }
                match result {
                    Ok(watches) => {
                        page.actions.watch = watches.iter().find(|watch| watch.url == url).cloned();
                        if page.target.is_none() {
                            page.state
                                .update(cx, |state, cx| state.set_pull_request_watches(watches, cx));
                        }
                    }
                    Err(error) => page.actions.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn submit_review(&mut self, cx: &mut Context<Self>) {
        let Some(event) = self.actions.review_event else {
            return;
        };
        let body = self.actions.review_input.read(cx).text().to_owned();
        self.perform_action(ChangeRequestAction::Review { event, body }, cx);
    }

    fn submit_reply(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.actions.reply_thread.clone() else {
            return;
        };
        let body = self.actions.reply_input.read(cx).text().to_owned();
        self.perform_action(ChangeRequestAction::ReplyToThread { thread_id, body }, cx);
    }

    /// The Summary tab's "Review and merge" card.
    pub(super) fn actions_card(
        &self,
        detail: &ChangeRequestDetail,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.actions.pending.is_some();
        let open = detail.state == "OPEN";
        let mut card = widgets::section_card(theme)
            .mt(px(CARD_GAP))
            .id("pr-actions")
            .debug_selector(|| "pr-actions".into());
        if let Some(label) = self.actions.pending {
            card = card.child(
                div()
                    .id("pr-action-pending")
                    .debug_selector(|| "pr-action-pending".into())
                    .px(px(16.0))
                    .pt(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(crate::loaders::mini_glyph_spinner(
                        "pr-action-spinner",
                        1.5,
                        theme.glyph,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(label),
            );
        }
        if let Some(error) = &self.actions.error {
            card = card.child(
                div()
                    .id("pr-action-error")
                    .debug_selector(|| "pr-action-error".into())
                    .px(px(16.0))
                    .pt(px(12.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        } else if let Some(notice) = &self.actions.notice {
            card = card.child(
                div()
                    .id("pr-action-notice")
                    .debug_selector(|| "pr-action-notice".into())
                    .px(px(16.0))
                    .pt(px(12.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(notice.clone()),
            );
        }
        if open {
            card = card.child(self.review_row(busy, theme, cx));
            card = card.child(self.merge_row(detail, busy, theme, cx));
        } else if detail.state == "CLOSED" {
            card = card.child(
                widgets::card_row(theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(180.0))
                            .child(widgets::row_title(theme, "Closed without merging")),
                    )
                    .child(
                        button("pr-reopen", "Reopen", widgets::ActionTone::Outlined, !busy, theme)
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.perform_action(ChangeRequestAction::Reopen, cx)
                            })),
                    ),
            );
        }
        card.child(self.watch_row(open, theme, cx)).into_any_element()
    }

    fn review_row(&self, busy: bool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut row = widgets::card_row(theme, true)
            .id("pr-review-row")
            .debug_selector(|| "pr-review-row".into())
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.0))
                    .child(widgets::row_title(theme, "Review")),
            );
        let verdicts = [
            (ReviewEvent::Approve, "pr-review-approve"),
            (ReviewEvent::RequestChanges, "pr-review-request-changes"),
            (ReviewEvent::Comment, "pr-review-comment"),
        ];
        row = row.child(
            div()
                .flex_none()
                .max_w_full()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .children(verdicts.into_iter().map(|(event, id)| {
                    let selected = self.actions.review_event == Some(event);
                    button(
                        id,
                        event.label(),
                        if selected {
                            widgets::ActionTone::Filled
                        } else {
                            widgets::ActionTone::Outlined
                        },
                        !busy,
                        theme,
                    )
                    .aria_selected(selected)
                    .on_click(cx.listener(move |page, _, window, cx| {
                        if page.actions.pending.is_some() {
                            return;
                        }
                        page.actions.review_event =
                            (page.actions.review_event != Some(event)).then_some(event);
                        page.actions.error = None;
                        if page.actions.review_event.is_some() {
                            window.focus(&page.actions.review_input.read(cx).focus_handle(cx), cx);
                        }
                        cx.notify();
                    }))
                })),
        );
        let Some(event) = self.actions.review_event else {
            return row.into_any_element();
        };
        let submit_label = match event {
            ReviewEvent::Approve => "Submit approval",
            ReviewEvent::RequestChanges => "Submit request for changes",
            ReviewEvent::Comment => "Submit comment",
        };
        let has_text = !self.actions.review_input.read(cx).text().trim().is_empty();
        let can_submit = !busy && (has_text || !event.needs_body());
        div()
            .flex()
            .flex_col()
            .child(row)
            .child(
                div()
                    .id("pr-review-form")
                    .debug_selector(|| "pr-review-form".into())
                    .mx(px(16.0))
                    .pb(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(8.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .max_h(px(140.0))
                            .overflow_hidden()
                            .child(self.actions.review_input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                button(
                                    "pr-review-submit",
                                    submit_label,
                                    widgets::ActionTone::Solid,
                                    can_submit,
                                    theme,
                                )
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    if can_submit {
                                        page.submit_review(cx);
                                    }
                                })),
                            )
                            .child(
                                button(
                                    "pr-review-cancel",
                                    "Cancel",
                                    widgets::ActionTone::Quiet,
                                    !busy,
                                    theme,
                                )
                                .on_click(cx.listener(|page, _, _, cx| {
                                    if page.actions.pending.is_none() {
                                        page.actions.review_event = None;
                                        cx.notify();
                                    }
                                })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn merge_row(
        &self,
        detail: &ChangeRequestDetail,
        busy: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let options = self.actions.merge_options.clone().unwrap_or_default();
        let mut controls = div()
            .id("pr-merge-actions")
            .debug_selector(|| "pr-merge-actions".into())
            .flex_none()
            .max_w_full()
            .flex()
            .flex_wrap()
            .gap(px(8.0));
        if detail.is_draft {
            controls = controls.child(
                button(
                    "pr-ready",
                    "Ready for review",
                    widgets::ActionTone::Filled,
                    !busy,
                    theme,
                )
                .on_click(cx.listener(|page, _, _, cx| {
                    page.perform_action(ChangeRequestAction::MarkReady, cx)
                })),
            );
        } else if self.actions.merge_options.is_none() {
            let loading = self.actions.merge_loading;
            controls = controls.child(
                button(
                    "pr-merge-open",
                    if loading { "Loading merge options…" } else { "Merge…" },
                    widgets::ActionTone::Filled,
                    !busy && !loading,
                    theme,
                )
                .on_click(cx.listener(move |page, _, _, cx| {
                    if page.actions.pending.is_none() && !page.actions.merge_loading {
                        page.actions.error = None;
                        page.load_merge_options(cx);
                    }
                })),
            );
        } else {
            for method in options.allowed_methods.iter().copied() {
                let id = match method {
                    MergeMethod::Squash => "pr-merge-squash",
                    MergeMethod::Merge => "pr-merge-merge",
                    MergeMethod::Rebase => "pr-merge-rebase",
                };
                controls = controls.child(
                    button(id, method.label(), widgets::ActionTone::Filled, !busy, theme)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            if page.actions.pending.is_none() {
                                page.actions.confirm_merge = Some(method);
                                page.actions.error = None;
                                cx.notify();
                            }
                        })),
                );
            }
            if options.auto_merge_method.is_some() {
                controls = controls.child(
                    button(
                        "pr-auto-merge-off",
                        "Disable auto-merge",
                        widgets::ActionTone::Outlined,
                        !busy,
                        theme,
                    )
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.perform_action(ChangeRequestAction::DisableAutoMerge, cx)
                    })),
                );
            } else if options.auto_merge_allowed
                && let Some(method) = options.preferred_method()
            {
                controls = controls.child(
                    button(
                        "pr-auto-merge-on",
                        "Enable auto-merge",
                        widgets::ActionTone::Outlined,
                        !busy,
                        theme,
                    )
                    .tooltip(widgets::text_tooltip(format!(
                        "{} once required reviews and checks pass",
                        method.label()
                    )))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.perform_action(ChangeRequestAction::EnableAutoMerge { method }, cx)
                    })),
                );
            }
        }
        controls = controls.child(
            button(
                "pr-close",
                "Close",
                widgets::ActionTone::Quiet,
                !busy,
                theme,
            )
            .on_click(cx.listener(|page, _, _, cx| {
                page.perform_action(ChangeRequestAction::Close, cx)
            })),
        );
        let subtitle = if self.actions.merge_options.is_none() {
            match detail.mergeable.as_str() {
                "CONFLICTING" => "Has merge conflicts with the base branch.".to_owned(),
                _ => "Pick a method; GitHub enforces branch protection.".to_owned(),
            }
        } else if options.allowed_methods.is_empty() && !detail.is_draft {
            "This repository allows no merge method you can use.".to_owned()
        } else if let Some(method) = options.auto_merge_method {
            format!("Auto-merge is on ({}).", method.label().to_lowercase())
        } else {
            match detail.mergeable.as_str() {
                "CONFLICTING" => "Has merge conflicts with the base branch.".to_owned(),
                _ => "GitHub enforces branch protection; its answer shows here.".to_owned(),
            }
        };
        let row = widgets::card_row(theme, false)
            .id("pr-merge-row")
            .debug_selector(|| "pr-merge-row".into())
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(widgets::row_title(theme, "Merge"))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(subtitle),
                    ),
            )
            .child(controls);
        let Some(method) = self.actions.confirm_merge else {
            return row.into_any_element();
        };
        let head = detail.head_ref_oid.clone();
        div()
            .flex()
            .flex_col()
            .child(row)
            .child(
                div()
                    .id("pr-merge-confirm")
                    .debug_selector(|| "pr-merge-confirm".into())
                    .mx(px(16.0))
                    .pb(px(12.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(200.0))
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text)
                            .child(merge_confirmation(detail, method)),
                    )
                    .child(
                        button(
                            "pr-merge-confirm-button",
                            "Confirm merge",
                            widgets::ActionTone::Solid,
                            !busy,
                            theme,
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.perform_action(
                                ChangeRequestAction::Merge {
                                    method,
                                    head_ref_oid: head.clone(),
                                },
                                cx,
                            )
                        })),
                    )
                    .child(
                        button(
                            "pr-merge-cancel",
                            "Cancel",
                            widgets::ActionTone::Quiet,
                            !busy,
                            theme,
                        )
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.actions.confirm_merge = None;
                            cx.notify();
                        })),
                    ),
            )
            .into_any_element()
    }

    fn watch_row(&self, open: bool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let watch = self.actions.watch.as_ref();
        let enabled = watch.is_some_and(|watch| watch.enabled);
        let auto_merge = watch.is_some_and(|watch| watch.auto_merge);
        let mut fragments = vec![match watch {
            Some(watch) => watch.status.label().to_owned(),
            None => "Off".to_owned(),
        }];
        if let Some(watch) = watch {
            if watch.chat_id.is_none() && watch.enabled {
                fragments.push("no chat linked; events go to Needs you".into());
            }
            if let Some(event) = &watch.last_event {
                fragments.push(event.clone());
            }
            if let Some(error) = &watch.last_error {
                fragments.push(error.clone());
            }
        }
        let pending = self.actions.watch_pending;
        widgets::card_row(theme, false)
            .id("pr-watch-row")
            .debug_selector(|| "pr-watch-row".into())
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(widgets::row_title(theme, "Watch"))
                    .child(
                        div()
                            .id("pr-watch-status")
                            .debug_selector(|| "pr-watch-status".into())
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(fragments.join(" · ")),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .max_w_full()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(
                        button(
                            "pr-watch-toggle",
                            if enabled { "Stop watching" } else { "Watch" },
                            widgets::ActionTone::Outlined,
                            !pending && (open || enabled),
                            theme,
                        )
                        .tooltip(widgets::text_tooltip(
                            "Wake the PR’s chat on failed checks, new review comments and merge conflicts",
                        ))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            if open || enabled {
                                page.set_watch(!enabled, None, cx)
                            }
                        })),
                    )
                    .when(enabled && open, |el| {
                        el.child(
                            button(
                                "pr-watch-auto-merge",
                                if auto_merge {
                                    "Auto-merge when green: on"
                                } else {
                                    "Auto-merge when green: off"
                                },
                                if auto_merge {
                                    widgets::ActionTone::Filled
                                } else {
                                    widgets::ActionTone::Quiet
                                },
                                !pending,
                                theme,
                            )
                            .aria_selected(auto_merge)
                            .tooltip(widgets::text_tooltip(
                                "Turn on GitHub auto-merge once checks pass and the PR is approved. Never approves for you.",
                            ))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.set_watch(true, Some(!auto_merge), cx)
                            })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Reply and resolve controls under the last comment of a review thread.
    pub(super) fn thread_controls(
        &self,
        thread: &zeron_proto::ChangeRequestReviewThread,
        index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.actions.pending.is_some();
        let thread_id = thread.id.clone();
        let resolved = thread.is_resolved;
        let replying = self.actions.reply_thread.as_deref() == Some(thread.id.as_str());
        let reply_id = thread_id.clone();
        let mut column = div().flex().flex_col().gap(px(8.0)).child(
            div()
                .flex()
                .gap(px(8.0))
                .child(
                    button(
                        format!("pr-thread-reply-{index}"),
                        "Reply",
                        widgets::ActionTone::Quiet,
                        !busy,
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, window, cx| {
                        if page.actions.pending.is_some() {
                            return;
                        }
                        if page.actions.reply_thread.as_deref() != Some(reply_id.as_str()) {
                            page.actions.reply_thread = Some(reply_id.clone());
                            page.actions
                                .reply_input
                                .update(cx, |input, cx| input.set_text("", cx));
                        }
                        page.actions.error = None;
                        window.focus(&page.actions.reply_input.read(cx).focus_handle(cx), cx);
                        cx.notify();
                    })),
                )
                .child(
                    button(
                        format!("pr-thread-resolve-{index}"),
                        if resolved { "Unresolve" } else { "Resolve" },
                        widgets::ActionTone::Quiet,
                        !busy,
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        let thread_id = thread_id.clone();
                        page.perform_action(
                            if resolved {
                                ChangeRequestAction::UnresolveThread { thread_id }
                            } else {
                                ChangeRequestAction::ResolveThread { thread_id }
                            },
                            cx,
                        )
                    })),
                ),
        );
        if replying {
            let can_send = !busy && !self.actions.reply_input.read(cx).text().trim().is_empty();
            column = column
                .child(
                    div()
                        .id(SharedString::from(format!("pr-thread-reply-form-{index}")))
                        .debug_selector(move || format!("pr-thread-reply-form-{index}"))
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .max_h(px(140.0))
                        .overflow_hidden()
                        .child(self.actions.reply_input.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            button(
                                format!("pr-thread-reply-send-{index}"),
                                "Send reply",
                                widgets::ActionTone::Solid,
                                can_send,
                                theme,
                            )
                            .on_click(cx.listener(move |page, _, _, cx| {
                                if can_send {
                                    page.submit_reply(cx)
                                }
                            })),
                        )
                        .child(
                            button(
                                format!("pr-thread-reply-cancel-{index}"),
                                "Cancel",
                                widgets::ActionTone::Quiet,
                                !busy,
                                theme,
                            )
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.actions.reply_thread = None;
                                cx.notify();
                            })),
                        ),
                );
        }
        if let Some(error) = self.actions.error.as_ref().filter(|_| replying) {
            column = column.child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        column.into_any_element()
    }

    #[cfg(test)]
    pub(crate) fn fixture_action_state(&self) -> (Option<&'static str>, Option<String>, Option<String>) {
        (
            self.actions.pending,
            self.actions.error.clone(),
            self.actions.notice.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_confirmation_names_repository_pull_request_base_head_and_method() {
        let detail = ChangeRequestDetail {
            url: "https://github.com/acme/widgets/pull/12".into(),
            number: 12,
            base_ref_name: "main".into(),
            head_ref_oid: "0123456789abcdef0123456789abcdef01234567".into(),
            ..Default::default()
        };
        assert_eq!(
            merge_confirmation(&detail, MergeMethod::Squash),
            "Squash and merge #12 in acme/widgets into main at 0123456?"
        );
    }

    #[test]
    fn failures_show_githubs_reason_when_relayed() {
        assert_eq!(
            action_failure(&zeron_rpc::RpcError::Failed(
                "Review required by branch protection.\ngh: …".into()
            )),
            "Review required by branch protection.\ngh: …"
        );
        assert_eq!(
            action_failure(&zeron_rpc::RpcError::UnknownMethod("x".into())),
            "Update Godspeed on the selected device."
        );
    }
}
