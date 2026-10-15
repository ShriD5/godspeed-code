//! Git write actions in the UI (T3 parity §12): the Changes pane's commit
//! box and Push / Pull / Sync row, the Create pull request dialog, and the
//! Init / Publish affordances for folders without a repository or remote.
//!
//! Every action is an engine RPC addressed to the chat's checkout (relay-
//! forwarded to the host device for remote chats), so this module only
//! gathers input, shows progress, and reports the engine's actionable error
//! text. Results land as toasts; a created pull request toasts with an
//! "Open" link and shows up in the chat's PR badge once the engine's
//! refreshed lookup reports it.

use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, Focusable as _, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use serde::de::DeserializeOwned;

use zeron_proto::{
    CreatedPullRequest, GeneratedCommitMessage, GeneratedPullRequest, GitBranchStatus,
    GitCommitResult, GitSyncResult, PublishedRepository, RepositoryVisibility,
};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::popover::{self, Popup};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// The checkout an action addresses: the selected chat's cwd on its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTarget {
    pub chat_id: String,
    pub cwd: String,
    /// The chat's host device when it is not the connected engine.
    pub target_device_id: Option<String>,
    pub branch: Option<String>,
}

impl GitTarget {
    /// The selected chat's checkout, if it has one.
    pub fn for_selected_chat(state: &AppState) -> Option<Self> {
        let chat = state.selected_chat_row()?;
        let cwd = chat.cwd.clone().filter(|cwd| !cwd.is_empty())?;
        let target_device_id = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        Some(Self {
            chat_id: chat.id.clone(),
            cwd,
            target_device_id,
            branch: chat.branch.clone(),
        })
    }

    fn key(&self) -> String {
        format!(
            "{}:{}",
            self.target_device_id.as_deref().unwrap_or("local"),
            self.cwd
        )
    }

    /// `extra` plus this target's `cwd` (or `path`) and `targetDeviceId`.
    fn params(&self, cwd_key: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut params = match extra {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        params.insert(cwd_key.into(), self.cwd.clone().into());
        if let Some(target) = &self.target_device_id {
            params.insert("targetDeviceId".into(), target.clone().into());
        }
        serde_json::Value::Object(params)
    }
}

/// One git action RPC; the error is the engine's user-facing text.
pub async fn git_call<T: DeserializeOwned>(
    engine: &EngineHandle,
    target: &GitTarget,
    method: &str,
    extra: serde_json::Value,
) -> Result<T, String> {
    let cwd_key = if method == methods::GIT_INIT {
        "path"
    } else {
        "cwd"
    };
    let value = engine
        .client()
        .call(method, target.params(cwd_key, extra))
        .await
        .map_err(|error| match error {
            zeron_rpc::RpcError::UnknownMethod(_) => {
                "This chat's device is running an older Godspeed — update it to use git actions"
                    .to_string()
            }
            other => other.to_string(),
        })?;
    serde_json::from_value(value).map_err(|error| format!("unexpected reply: {error}"))
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Toast copy for a finished push/pull/sync.
pub fn sync_summary(method: &str, result: &GitSyncResult) -> String {
    let upstream = result
        .status
        .upstream
        .clone()
        .unwrap_or_else(|| "the remote".into());
    if result.published_branch {
        return format!("Published branch to {upstream}");
    }
    match (method, result.pulled, result.pushed) {
        (_, 0, 0) => "Already up to date".into(),
        (_, pulled, 0) => format!("Pulled {}", plural(pulled, "commit")),
        (_, 0, pushed) => format!("Pushed {} to {upstream}", plural(pushed, "commit")),
        (_, pulled, pushed) => format!(
            "Synced: pulled {}, pushed {}",
            plural(pulled, "commit"),
            plural(pushed, "commit")
        ),
    }
}

/// Fire-and-forget push/pull/sync with toasts (command palette).
pub fn run_sync_action(engine: EngineHandle, target: GitTarget, method: &'static str, cx: &mut App) {
    cx.spawn(async move |cx| {
        let result: Result<GitSyncResult, String> =
            git_call(&engine, &target, method, serde_json::json!({})).await;
        cx.update(|cx| match result {
            Ok(result) => crate::toast::success(cx, sync_summary(method, &result)),
            Err(error) => crate::toast::error(cx, error),
        });
    })
    .detach();
}

/// What follows a successful commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterCommit {
    Nothing,
    Push,
    CreatePullRequest,
}

pub enum GitPanelEvent {
    /// Open the Create pull request dialog for this checkout.
    CreatePullRequest(GitTarget),
}

/// The Changes pane's git strip: branch and ahead/behind with Pull / Push /
/// Sync, a commit message box with a generate button, and Commit with a
/// menu for Commit & push / Commit & create PR. Folders without a repository
/// offer Init; repositories without a remote offer Publish.
pub struct GitPanel {
    state: Entity<AppState>,
    message: Entity<ComposerInput>,
    target: Option<GitTarget>,
    status: Option<GitBranchStatus>,
    /// Target key + diff checksum the status was read for.
    status_key: Option<String>,
    status_task: Option<Task<()>>,
    /// The running action's progress label ("Committing…").
    busy: Option<&'static str>,
    action_task: Option<Task<()>>,
    commit_menu: Popup<()>,
    publish_menu: Popup<()>,
    _message_events: Subscription,
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

impl GitPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| {
            ComposerInput::new("Commit message (leave empty to generate)", cx)
                .with_text_metrics(12.5, 17.0)
                .with_accessibility_role(gpui::Role::TextInput)
        });
        let events = cx.subscribe(&message, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit(AfterCommit::Nothing, cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        Self {
            state,
            message,
            target: None,
            status: None,
            status_key: None,
            status_task: None,
            busy: None,
            action_task: None,
            commit_menu: Popup::default(),
            publish_menu: Popup::default(),
            _message_events: events,
        }
    }

    /// Follow the selected checkout; a new target or a moved working tree
    /// (`checksum`: HEAD rides it) re-reads the branch status.
    pub fn set_context(
        &mut self,
        target: Option<GitTarget>,
        checksum: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if target.as_ref().map(GitTarget::key) != self.target.as_ref().map(GitTarget::key) {
            self.status = None;
        }
        let key = target
            .as_ref()
            .map(|target| format!("{}#{}", target.key(), checksum.unwrap_or("")));
        self.target = target;
        if key != self.status_key {
            self.status_key = key;
            self.refresh(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(engine)) =
            (self.target.clone(), self.state.read(cx).engine().cloned())
        else {
            return;
        };
        let key = self.status_key.clone();
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<GitBranchStatus, String> = git_call(
                &engine,
                &target,
                methods::GIT_BRANCH_STATUS,
                serde_json::json!({}),
            )
            .await;
            this.update(cx, |panel, cx| {
                if panel.status_key != key {
                    return;
                }
                match result {
                    Ok(status) => panel.status = Some(status),
                    Err(error) => {
                        tracing::debug!(%error, "git status unavailable");
                        panel.status = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The commit message field's focus handle.
    pub fn message_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.message.read(cx).focus_handle(cx)
    }

    /// Run one action; `finish` gets the result on the UI thread.
    fn run<T, F>(
        &mut self,
        label: &'static str,
        method: &'static str,
        extra: serde_json::Value,
        cx: &mut Context<Self>,
        finish: F,
    ) where
        T: DeserializeOwned + 'static,
        F: FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    {
        if self.busy.is_some() {
            return;
        }
        let (Some(target), Some(engine)) =
            (self.target.clone(), self.state.read(cx).engine().cloned())
        else {
            return;
        };
        self.busy = Some(label);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = git_call::<T>(&engine, &target, method, extra).await;
            this.update(cx, |panel, cx| {
                panel.busy = None;
                finish(panel, result, cx);
                panel.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn commit(&mut self, after: AfterCommit, cx: &mut Context<Self>) {
        let message = self.message.read(cx).text().to_string();
        self.run::<GitCommitResult, _>(
            if message.trim().is_empty() {
                "Generating and committing…"
            } else {
                "Committing…"
            },
            methods::GIT_COMMIT,
            serde_json::json!({ "message": message }),
            cx,
            move |panel, result, cx| match result {
                Ok(commit) => {
                    panel.message.update(cx, |input, cx| input.set_text("", cx));
                    let short: String = commit.sha.chars().take(7).collect();
                    crate::toast::success(cx, format!("Committed {short} {}", commit.subject));
                    match after {
                        AfterCommit::Nothing => {}
                        AfterCommit::Push => panel.sync_action(methods::GIT_PUSH, cx),
                        AfterCommit::CreatePullRequest => panel.request_pull_request(cx),
                    }
                }
                Err(error) => crate::toast::error(cx, error),
            },
        );
    }

    fn generate(&mut self, cx: &mut Context<Self>) {
        self.run::<GeneratedCommitMessage, _>(
            "Generating message…",
            methods::GENERATE_COMMIT_MESSAGE,
            serde_json::json!({}),
            cx,
            |panel, result, cx| match result {
                Ok(generated) => panel
                    .message
                    .update(cx, |input, cx| input.set_text(generated.message, cx)),
                Err(error) => crate::toast::error(cx, error),
            },
        );
    }

    fn sync_action(&mut self, method: &'static str, cx: &mut Context<Self>) {
        let label = match method {
            methods::GIT_PUSH => "Pushing…",
            methods::GIT_PULL => "Pulling…",
            _ => "Syncing…",
        };
        self.run::<GitSyncResult, _>(
            label,
            method,
            serde_json::json!({}),
            cx,
            move |_, result, cx| match result {
                Ok(result) => crate::toast::success(cx, sync_summary(method, &result)),
                Err(error) => crate::toast::error(cx, error),
            },
        );
    }

    fn init(&mut self, cx: &mut Context<Self>) {
        self.run::<GitBranchStatus, _>(
            "Initializing…",
            methods::GIT_INIT,
            serde_json::json!({}),
            cx,
            |_, result, cx| match result {
                Ok(_) => crate::toast::success(cx, "Initialized a git repository"),
                Err(error) => crate::toast::error(cx, error),
            },
        );
    }

    fn publish(&mut self, visibility: RepositoryVisibility, cx: &mut Context<Self>) {
        self.close_publish_menu(cx);
        self.run::<PublishedRepository, _>(
            "Publishing…",
            methods::PUBLISH_REPOSITORY,
            serde_json::json!({ "visibility": visibility }),
            cx,
            |_, result, cx| match result {
                Ok(published) => crate::toast::success_with_link(
                    cx,
                    "Published to GitHub",
                    published.url,
                ),
                Err(error) => crate::toast::error(cx, error),
            },
        );
    }

    fn request_pull_request(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.target.clone() {
            cx.emit(GitPanelEvent::CreatePullRequest(target));
        }
    }

    fn close_commit_menu(&mut self, cx: &mut Context<Self>) {
        if self.commit_menu.begin_close() {
            popover::reap_popup(cx, |panel: &mut Self| &mut panel.commit_menu);
        }
    }

    fn close_publish_menu(&mut self, cx: &mut Context<Self>) {
        if self.publish_menu.begin_close() {
            popover::reap_popup(cx, |panel: &mut Self| &mut panel.publish_menu);
        }
    }

    fn render_commit_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let rows: [(&str, &str, AfterCommit); 2] = [
            ("git-commit-push", "Commit & push", AfterCommit::Push),
            (
                "git-commit-pr",
                "Commit & create PR",
                AfterCommit::CreatePullRequest,
            ),
        ];
        popover::popover_card(theme)
            .w(px(190.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_commit_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(rows.into_iter().map(|(id, label, after)| {
                        popover::menu_row(theme, false, id)
                            .id(id)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_commit_menu(cx);
                                this.commit(after, cx);
                            }))
                            .child(div().flex_1().child(label))
                    })),
            )
            .into_any_element()
    }

    fn render_publish_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let rows: [(&str, &str, RepositoryVisibility); 2] = [
            (
                "git-publish-private",
                "Private repository",
                RepositoryVisibility::Private,
            ),
            (
                "git-publish-public",
                "Public repository",
                RepositoryVisibility::Public,
            ),
        ];
        popover::popover_card(theme)
            .w(px(190.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_publish_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(rows.into_iter().map(|(id, label, visibility)| {
                        popover::menu_row(theme, false, id)
                            .id(id)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.publish(visibility, cx)
                            }))
                            .child(div().flex_1().child(label))
                    })),
            )
            .into_any_element()
    }
}

/// A compact text button for the strip. Disabled buttons dim and ignore
/// clicks (the caller only attaches a handler when enabled).
fn strip_button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(24.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(5.0))
        .rounded(px(6.0))
        .text_size(px(12.0))
        .map(|el| {
            if primary {
                el.bg(theme.text)
                    .text_color(theme.on_solid)
                    .font_weight(gpui::FontWeight::MEDIUM)
            } else {
                el.bg(motion::hover_blend(
                    id,
                    crate::theme::wash(0.06),
                    crate::theme::wash(0.14),
                ))
                .on_hover(motion::hover_listener(id))
                .text_color(theme.text)
            }
        })
        .when(enabled, |el| el.cursor_pointer())
        .when(!enabled, |el| el.opacity(0.4))
        .child(label.into())
}

fn strip_icon_button(
    id: &'static str,
    icon: &'static str,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(motion::hover_blend(
            id,
            crate::theme::wash(0.0),
            crate::theme::wash(0.14),
        ))
        .on_hover(motion::hover_listener(id))
        .when(enabled, |el| el.cursor_pointer())
        .when(!enabled, |el| el.opacity(0.4))
        .child(
            crate::icons::icon(icon)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let Some(status) = self.status.clone() else {
            return div().into_any_element();
        };
        let idle = self.busy.is_none();
        let frame = div()
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(Theme::SPACE_LG))
            .py(px(10.0))
            .border_t_1()
            .border_color(crate::theme::hairline(0.06))
            .text_size(px(12.0));

        if !status.is_repo {
            return frame
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(theme.text_muted)
                                .child("This folder isn't a git repository."),
                        )
                        .child(
                            strip_button(
                                "git-init",
                                self.busy.unwrap_or("Initialize repository"),
                                true,
                                idle,
                                &theme,
                            )
                            .when(idle, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.init(cx)))
                            }),
                        ),
                )
                .into_any_element();
        }

        // Branch + ahead/behind, then the remote actions.
        let branch = status
            .branch
            .clone()
            .unwrap_or_else(|| "detached HEAD".into());
        let counts = (status.upstream.is_some() || status.ahead > 0).then(|| {
            let mut parts = Vec::new();
            if status.ahead > 0 {
                parts.push(format!("↑{}", status.ahead));
            }
            if status.behind > 0 {
                parts.push(format!("↓{}", status.behind));
            }
            if status.upstream.is_none() {
                parts.push("unpublished".into());
            }
            parts.join(" ")
        });
        let has_remote = status.remote.is_some();
        let can_push = idle && status.branch.is_some() && status.has_commits;
        let can_pull = idle && status.upstream.is_some();
        let mut remote_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                crate::icons::icon(crate::icons::GIT_BRANCH)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.5))
                    .text_color(theme.text)
                    .child(SharedString::from(branch)),
            )
            .children(counts.filter(|c| !c.is_empty()).map(|counts| {
                div()
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(counts))
            }))
            .child(div().flex_1());
        if has_remote {
            remote_row = remote_row
                .child(
                    strip_button("git-pull", "Pull", false, can_pull, &theme)
                        .when(can_pull, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| {
                                this.sync_action(methods::GIT_PULL, cx)
                            }))
                        }),
                )
                .child(
                    strip_button("git-push", "Push", false, can_push, &theme)
                        .when(can_push, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| {
                                this.sync_action(methods::GIT_PUSH, cx)
                            }))
                        }),
                )
                .child(
                    strip_button("git-sync", "Sync", false, can_push, &theme)
                        .when(can_push, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| {
                                this.sync_action(methods::GIT_SYNC, cx)
                            }))
                        }),
                );
        } else {
            let can_publish = idle && status.has_commits;
            let trigger = strip_button(
                "git-publish",
                "Publish to GitHub",
                false,
                can_publish,
                &theme,
            )
            .when(can_publish, |el| {
                el.on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| this.publish_menu.note_trigger_press()),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.publish_menu.take_press_was_open() {
                        this.close_publish_menu(cx);
                    } else {
                        this.publish_menu.open(());
                    }
                    cx.notify();
                }))
            });
            let trigger = if self.publish_menu.get().is_some() {
                let closing = self.publish_menu.closing_since();
                let menu = self.render_publish_menu(&theme, cx);
                trigger
                    .relative()
                    .child(popover::anchored_menu_above(
                        "git-publish-menu",
                        menu,
                        closing,
                    ))
                    .into_any_element()
            } else {
                trigger.into_any_element()
            };
            remote_row = remote_row.child(trigger);
        }

        // Commit box.
        let has_changes = status.changed_files > 0;
        let can_commit = idle && has_changes && !status.conflicted;
        let message_box = div()
            .w_full()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(4.0))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(crate::theme::hairline(0.08))
            .bg(crate::theme::ink(0.03))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .max_h(px(120.0))
                    .overflow_hidden()
                    .child(self.message.clone()),
            )
            .child(
                strip_icon_button(
                    "git-generate-message",
                    crate::icons::MAGIC_STICK_3,
                    can_commit,
                    &theme,
                )
                .tooltip(|_, cx| cx.new(|_| Tooltip("Generate commit message")).into())
                .tooltip_show_delay(Duration::from_millis(350))
                .when(can_commit, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.generate(cx)))
                }),
            );

        let commit_label: SharedString = match self.busy {
            Some(label) => label.into(),
            None if status.conflicted => "Resolve conflicts first".into(),
            None if !has_changes => "No changes".into(),
            None => format!("Commit {}", plural(status.changed_files, "file")).into(),
        };
        let menu_trigger = strip_icon_button(
            "git-commit-more",
            crate::icons::ALT_ARROW_DOWN,
            can_commit,
            &theme,
        )
        .when(can_commit, |el| {
            el.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.commit_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.commit_menu.take_press_was_open() {
                    this.close_commit_menu(cx);
                } else {
                    this.commit_menu.open(());
                }
                cx.notify();
            }))
        });
        let menu_trigger = if self.commit_menu.get().is_some() {
            let closing = self.commit_menu.closing_since();
            let menu = self.render_commit_menu(&theme, cx);
            menu_trigger
                .relative()
                .child(popover::anchored_menu_above("git-commit-menu", menu, closing))
                .into_any_element()
        } else {
            menu_trigger.into_any_element()
        };
        let can_open_pr = idle
            && has_remote
            && status.has_commits
            && status.branch.is_some()
            && (status.ahead > 0 || status.upstream.is_some());
        let commit_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(
                strip_button("git-commit", commit_label, true, can_commit, &theme).when(
                    can_commit,
                    |el| {
                        el.on_click(
                            cx.listener(|this, _, _, cx| this.commit(AfterCommit::Nothing, cx)),
                        )
                    },
                ),
            )
            .child(menu_trigger)
            .child(div().flex_1())
            .when(has_remote, |row| {
                row.child(
                    strip_button("git-create-pr", "Create PR", false, can_open_pr, &theme).when(
                        can_open_pr,
                        |el| {
                            el.on_click(
                                cx.listener(|this, _, _, cx| this.request_pull_request(cx)),
                            )
                        },
                    ),
                )
            });

        frame
            .child(remote_row)
            .child(message_box)
            .child(commit_row)
            .into_any_element()
    }
}

struct Tooltip(&'static str);

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(11.0))
            .text_color(theme.text)
            .child(self.0)
    }
}

// ---------------------------------------------------------------------------
// Create pull request dialog
// ---------------------------------------------------------------------------

pub enum CreatePullRequestEvent {
    /// Cancelled, or created (after the toast).
    Close,
}

/// Generated, editable title and body; base branch picker (repository
/// default preselected); draft toggle. Creating pushes first when needed.
pub struct CreatePullRequestDialog {
    state: Entity<AppState>,
    target: GitTarget,
    title: Entity<ComposerInput>,
    body: Entity<ComposerInput>,
    base: Option<String>,
    branches: Vec<String>,
    template: Option<String>,
    draft: bool,
    /// Generating text (the fields stay editable meanwhile).
    generating: bool,
    creating: bool,
    error: Option<SharedString>,
    base_menu: Popup<()>,
    _tasks: Vec<Task<()>>,
    create_task: Option<Task<()>>,
}

impl EventEmitter<CreatePullRequestEvent> for CreatePullRequestDialog {}

impl CreatePullRequestDialog {
    pub fn new(state: Entity<AppState>, target: GitTarget, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| {
            ComposerInput::new("Title", cx)
                .with_single_line()
                .with_text_metrics(13.0, 17.0)
                .with_accessibility_role(gpui::Role::TextInput)
        });
        let body = cx.new(|cx| {
            ComposerInput::new("Description", cx)
                .with_text_metrics(12.5, 18.0)
                .with_accessibility_role(gpui::Role::TextInput)
        });
        let mut dialog = Self {
            state,
            target,
            title,
            body,
            base: None,
            branches: Vec::new(),
            template: None,
            draft: false,
            generating: true,
            creating: false,
            error: None,
            base_menu: Popup::default(),
            _tasks: Vec::new(),
            create_task: None,
        };
        dialog.load(cx);
        dialog
    }

    /// The title field's focus handle.
    pub fn title_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.title.read(cx).focus_handle(cx)
    }

    /// Branch list (base picker) and generated text, side by side.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.generating = false;
            self.error = Some("Engine is not connected.".into());
            return;
        };
        let target = self.target.clone();
        let branches_engine = engine.clone();
        let branches_target = target.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), branches_target.cwd.clone().into());
            if let Some(device) = &branches_target.target_device_id {
                params.insert("targetDeviceId".into(), device.clone().into());
            }
            let branches = branches_engine
                .client()
                .call(methods::LIST_BRANCHES, serde_json::Value::Object(params))
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
                .unwrap_or_default();
            this.update(cx, |dialog, cx| {
                dialog.branches = branches;
                cx.notify();
            })
            .ok();
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result: Result<GeneratedPullRequest, String> = git_call(
                &engine,
                &target,
                methods::GENERATE_PULL_REQUEST,
                serde_json::json!({}),
            )
            .await;
            this.update(cx, |dialog, cx| {
                dialog.generating = false;
                match result {
                    Ok(generated) => {
                        // Never clobber text the user already typed.
                        if dialog.title.read(cx).text().trim().is_empty() {
                            dialog
                                .title
                                .update(cx, |input, cx| input.set_text(generated.title, cx));
                        }
                        if dialog.body.read(cx).text().trim().is_empty() {
                            dialog
                                .body
                                .update(cx, |input, cx| input.set_text(generated.body, cx));
                        }
                        dialog.base.get_or_insert(generated.base);
                        dialog.template = generated.template;
                    }
                    Err(error) => dialog.error = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.target.clone();
        let params = serde_json::json!({
            "title": self.title.read(cx).text(),
            "body": self.body.read(cx).text(),
            "base": self.base,
            "draft": self.draft,
        });
        self.creating = true;
        self.error = None;
        self.create_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<CreatedPullRequest, String> =
                git_call(&engine, &target, methods::CREATE_PULL_REQUEST, params).await;
            this.update(cx, |dialog, cx| {
                dialog.creating = false;
                match result {
                    Ok(created) => {
                        let label = match created.number {
                            Some(number) if created.draft => {
                                format!("Draft pull request #{number} created")
                            }
                            Some(number) => format!("Pull request #{number} created"),
                            None => "Pull request created".to_string(),
                        };
                        crate::toast::success_with_link(cx, label, created.url);
                        cx.emit(CreatePullRequestEvent::Close);
                    }
                    Err(error) => dialog.error = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_base_menu(&mut self, cx: &mut Context<Self>) {
        if self.base_menu.begin_close() {
            popover::reap_popup(cx, |dialog: &mut Self| &mut dialog.base_menu);
        }
    }

    fn render_base_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let head = self.target.branch.clone();
        let current = self.base.clone();
        popover::popover_card(theme)
            .w(px(240.0))
            .max_h(px(260.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_base_menu(cx)))
            .child(
                div()
                    .id("create-pr-base-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(250.0))
                    .overflow_y_scroll()
                    .children(
                        self.branches
                            .iter()
                            .filter(|branch| Some(*branch) != head.as_ref())
                            .take(200)
                            .enumerate()
                            .map(|(ix, branch)| {
                                let pick = branch.clone();
                                popover::menu_row(
                                    theme,
                                    current.as_deref() == Some(branch.as_str()),
                                    format!("create-pr-base-{ix}"),
                                )
                                .id(("create-pr-base", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.base = Some(pick.clone());
                                    this.close_base_menu(cx);
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(theme.font_mono.clone())
                                        .text_size(px(12.0))
                                        .child(SharedString::from(branch.clone())),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for CreatePullRequestDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let head = self
            .target
            .branch
            .clone()
            .unwrap_or_else(|| "current branch".into());
        let base_label: SharedString = self
            .base
            .clone()
            .unwrap_or_else(|| "default branch".into())
            .into();
        let base_trigger = div()
            .id("create-pr-base-trigger")
            .h(px(26.0))
            .px(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "create-pr-base-trigger",
                crate::theme::wash(0.06),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener("create-pr-base-trigger"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.base_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.base_menu.take_press_was_open() {
                    this.close_base_menu(cx);
                } else if !this.branches.is_empty() {
                    this.base_menu.open(());
                }
                cx.notify();
            }))
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(base_label),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted),
            );
        let base_trigger = if self.base_menu.get().is_some() {
            let closing = self.base_menu.closing_since();
            let menu = self.render_base_menu(&theme, cx);
            base_trigger
                .relative()
                .child(popover::anchored_menu_below("create-pr-base-menu", menu, closing))
                .into_any_element()
        } else {
            base_trigger.into_any_element()
        };
        let draft = self.draft;
        let draft_toggle = div()
            .id("create-pr-draft")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.draft = !this.draft;
                cx.notify();
            }))
            .child(
                div()
                    .size(px(14.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(crate::theme::hairline(0.2))
                    .when(draft, |el| el.bg(theme.text))
                    .when(draft, |el| {
                        el.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(11.0))
                                .text_color(theme.on_solid),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child("Create as draft"),
            );
        let busy_label = if self.creating {
            Some("Creating…")
        } else if self.generating {
            Some("Writing title and description…")
        } else {
            None
        };
        let can_create = !self.creating;
        popover::dialog_card(&theme)
            .w(px(520.0))
            .child(popover::dialog_title(&theme, "Create pull request"))
            .child(
                div()
                    .mt(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child("Merge")
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_color(theme.text)
                            .child(SharedString::from(head)),
                    )
                    .child("into")
                    .child(base_trigger),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .child(popover::dialog_field(self.title.clone().into_any_element())),
            )
            .child(
                div().mt(px(8.0)).child(
                    popover::dialog_field(
                        div()
                            .h(px(200.0))
                            .overflow_hidden()
                            .child(self.body.clone())
                            .into_any_element(),
                    ),
                ),
            )
            .when_some(self.template.clone(), |el, template| {
                el.child(
                    div()
                        .mt(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("Follows {template}"))),
                )
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .mt(px(8.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(draft_toggle)
                    .child(div().flex_1())
                    .children(busy_label.map(|label| {
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_faint)
                            .child(label)
                    }))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "create-pr-cancel")
                            .id("create-pr-cancel")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(CreatePullRequestEvent::Close)
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Create pull request")
                            .id("create-pr-confirm")
                            .when(!can_create, |el| el.opacity(0.5))
                            .when(can_create, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.create(cx)))
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(pulled: usize, pushed: usize, published: bool) -> GitSyncResult {
        GitSyncResult {
            pulled,
            pushed,
            published_branch: published,
            status: GitBranchStatus {
                upstream: Some("origin/main".into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn sync_summaries_read_naturally() {
        assert_eq!(
            sync_summary(methods::GIT_PULL, &result(0, 0, false)),
            "Already up to date"
        );
        assert_eq!(
            sync_summary(methods::GIT_PULL, &result(1, 0, false)),
            "Pulled 1 commit"
        );
        assert_eq!(
            sync_summary(methods::GIT_PUSH, &result(0, 3, false)),
            "Pushed 3 commits to origin/main"
        );
        assert_eq!(
            sync_summary(methods::GIT_SYNC, &result(2, 1, false)),
            "Synced: pulled 2 commits, pushed 1 commit"
        );
        assert_eq!(
            sync_summary(methods::GIT_PUSH, &result(0, 1, true)),
            "Published branch to origin/main"
        );
    }

    #[test]
    fn targets_carry_cwd_and_device() {
        let target = GitTarget {
            chat_id: "c".into(),
            cwd: "/repo".into(),
            target_device_id: Some("dev-2".into()),
            branch: None,
        };
        assert_eq!(
            target.params("cwd", serde_json::json!({ "message": "m" })),
            serde_json::json!({ "cwd": "/repo", "targetDeviceId": "dev-2", "message": "m" })
        );
        assert_eq!(
            target.params("path", serde_json::json!({})),
            serde_json::json!({ "path": "/repo", "targetDeviceId": "dev-2" })
        );
    }
}
