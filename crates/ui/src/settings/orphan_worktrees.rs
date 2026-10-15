//! Settings → General → Worktrees: Godspeed worktrees no chat uses any more,
//! each with a delete action that refuses uncommitted work until confirmed.

use gpui::{AnyElement, Context, Entity, Render, SharedString, Task, Window, div, prelude::*, px};
use zeron_proto::OrphanWorktree;
use zeron_rpc::methods;

use super::widgets;
use crate::icons;
use crate::popover::Loadable;
use crate::state::AppState;
use crate::theme::Theme;

struct Refusal {
    path: String,
    message: String,
    dirty: bool,
}

pub(super) struct OrphanWorktreesList {
    state: Entity<AppState>,
    target: Option<String>,
    orphans: Loadable<Vec<OrphanWorktree>>,
    list_task: Option<Task<()>>,
    removing: Option<String>,
    refusal: Option<Refusal>,
    remove_task: Option<Task<()>>,
}

impl OrphanWorktreesList {
    pub fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            target: None,
            orphans: Loadable::Idle,
            list_task: None,
            removing: None,
            refusal: None,
            remove_task: None,
        }
    }

    pub fn load(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.target = target;
        self.removing = None;
        self.refusal = None;
        self.remove_task = None;
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.orphans = Loadable::Error("Device is not connected".into());
            cx.notify();
            return;
        };
        self.orphans = Loadable::Loading;
        let params = serde_json::json!({ "targetDeviceId": self.target });
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ORPHAN_WORKTREES, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<OrphanWorktree>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |list, cx| {
                list.orphans = match result {
                    Ok(orphans) => Loadable::Ready(orphans),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn remove(&mut self, orphan: OrphanWorktree, force: bool, cx: &mut Context<Self>) {
        if self.removing.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({
            "repoPath": orphan.repo_path,
            "worktreePath": orphan.path,
            "force": force,
            "targetDeviceId": self.target,
        });
        self.removing = Some(orphan.path.clone());
        self.refusal = None;
        self.remove_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::DELETE_WORKTREE, params).await;
            this.update(cx, |list, cx| {
                list.removing = None;
                match result {
                    Ok(_) => list.refresh(cx),
                    Err(error) => {
                        let message = error.to_string();
                        list.refusal = Some(Refusal {
                            dirty: message.starts_with(zeron_engine::repos::WORKTREE_DIRTY),
                            path: orphan.path,
                            message,
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let summary = match &self.orphans {
            Loadable::Ready(orphans) if orphans.is_empty() => "None left behind".to_string(),
            Loadable::Ready(orphans) => format!("{} left behind by deleted chats", orphans.len()),
            Loadable::Error(error) => error.clone(),
            _ => "Looking…".to_string(),
        };
        widgets::card_row(theme, false)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, "Unused worktrees"))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().truncate().child(summary).into_any_element()],
                    )),
            )
            .child(
                widgets::action_button(theme, widgets::ActionTone::Quiet)
                    .id("orphan-worktrees-refresh")
                    .w(px(32.0))
                    .px_0()
                    .justify_center()
                    .tab_index(0)
                    .role(gpui::Role::Button)
                    .aria_label("Refresh unused worktrees")
                    .tooltip(widgets::text_tooltip("Refresh"))
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .child(
                        icons::icon(icons::REFRESH)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                    .on_click(cx.listener(|list, _, _, cx| list.refresh(cx))),
            )
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        orphan: &OrphanWorktree,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = std::path::Path::new(&orphan.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| orphan.path.clone());
        let mut meta = vec![
            div()
                .child(orphan.branch.clone().unwrap_or_else(|| "Detached".into()))
                .into_any_element(),
        ];
        if orphan.dirty {
            meta.push(div().child("Uncommitted changes").into_any_element());
        }
        meta.push(
            div()
                .truncate()
                .child(orphan.path.clone())
                .into_any_element(),
        );
        let refusal = self.refusal.as_ref().filter(|r| r.path == orphan.path);
        let force = refusal.is_some_and(|r| r.dirty);
        let busy = self.removing.is_some();
        let removing = self.removing.as_deref() == Some(orphan.path.as_str());
        let target = orphan.clone();
        let action = if force {
            widgets::text_action(theme, widgets::ActionTone::Danger, "Delete anyway")
        } else {
            widgets::action_button(theme, widgets::ActionTone::Quiet)
                .w(px(32.0))
                .px_0()
                .justify_center()
                .child(
                    icons::icon(icons::TRASH_BIN_MINIMALISTIC)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
        }
        .id(("orphan-worktree-delete", index))
        .flex_none()
        .tab_index(0)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(format!("Delete worktree {name}")))
        .focus_visible(|s| s.border_2().border_color(theme.accent))
        .when(removing, |el| el.opacity(0.5))
        .when(!busy, |el| {
            el.on_click(cx.listener(move |list, _, _, cx| list.remove(target.clone(), force, cx)))
        });
        div()
            .child(
                widgets::card_row(theme, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(theme, name))
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .child(action),
            )
            .when_some(refusal, |el, refusal| {
                el.child(
                    div()
                        .px(px(16.0))
                        .pb(px(12.0))
                        .child(widgets::error_strip(theme, refusal.message.clone())),
                )
            })
            .into_any_element()
    }
}

impl Render for OrphanWorktreesList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let header = self.render_header(&theme, cx);
        let orphans = self.orphans.ready().cloned().unwrap_or_default();
        let rows: Vec<AnyElement> = orphans
            .iter()
            .enumerate()
            .map(|(index, orphan)| self.render_row(index, orphan, &theme, cx))
            .collect();
        div().flex().flex_col().child(header).children(rows)
    }
}
