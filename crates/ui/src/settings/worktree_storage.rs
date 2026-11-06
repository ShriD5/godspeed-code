//! Settings → General → Worktrees: the cleanup policy (delete worktrees whose
//! branch is merged, or untouched for N days) and the storage view of every
//! worktree Godspeed created on the device: size on disk, branch, last
//! activity, merged / changed state, owning chat, with "Delete now" per row
//! and "Delete all eligible", each behind a confirm.
//!
//! Policy and listing live on the engine holding the worktrees
//! (`GetWorktreeCleanupSettings`, `ListWorktreeStorage`); sizes are measured
//! there, off this thread, and cached. Deletion goes through the engine's
//! usual path (`DeleteWorktree`, `CleanWorktrees`), which runs teardown and
//! never loses uncommitted work or unpushed commits without a second confirm.

use gpui::{AnyElement, Context, Entity, Render, SharedString, Task, Window, div, prelude::*, px};
use zeron_proto::worktree_cleanup::{WORKTREE_STALE_DAY_CHOICES, format_bytes};
use zeron_proto::{WorktreeCleanupReport, WorktreeCleanupSettings, WorktreeStorageEntry};
use zeron_rpc::methods;

use super::widgets;
use crate::icons;
use crate::popover::Loadable;
use crate::state::AppState;
use crate::theme::Theme;

/// Untouched-for span the switch turns on with.
const DEFAULT_STALE_DAYS: u32 = 14;

struct Refusal {
    path: String,
    message: String,
    dirty: bool,
}

/// What the next click on a confirm button deletes.
#[derive(Clone, PartialEq, Eq)]
enum Confirming {
    One(String),
    AllEligible,
}

pub(super) struct WorktreeStorage {
    state: Entity<AppState>,
    target: Option<String>,
    policy: Loadable<WorktreeCleanupSettings>,
    policy_task: Option<Task<()>>,
    entries: Loadable<Vec<WorktreeStorageEntry>>,
    /// Sizes are still being measured.
    measuring: bool,
    list_task: Option<Task<()>>,
    confirming: Option<Confirming>,
    /// A delete is running (a path, or every eligible one).
    deleting: Option<Confirming>,
    delete_task: Option<Task<()>>,
    refusal: Option<Refusal>,
    report: Option<String>,
    error: Option<String>,
}

fn unsupported(error: zeron_rpc::RpcError) -> String {
    match error {
        zeron_rpc::RpcError::UnknownMethod(_) => {
            "Update Godspeed on the selected device to see worktree storage and cleanup.".into()
        }
        other => other.to_string(),
    }
}

/// "Deleted 2 worktrees · kept 1 with changes".
fn report_summary(report: &WorktreeCleanupReport) -> String {
    let mut parts = vec![match report.deleted.len() {
        0 => "Nothing deleted".to_string(),
        1 => "Deleted 1 worktree".to_string(),
        n => format!("Deleted {n} worktrees"),
    }];
    if !report.kept.is_empty() {
        parts.push(format!(
            "kept {} with changes or a running chat",
            report.kept.len()
        ));
    }
    if !report.failed.is_empty() {
        parts.push(format!("{} couldn't be deleted", report.failed.len()));
    }
    let teardown = report
        .deleted
        .iter()
        .filter(|d| d.teardown_error.is_some())
        .count();
    if teardown > 0 {
        parts.push(format!("{teardown} teardown failed"));
    }
    parts.join(" · ")
}

impl WorktreeStorage {
    pub fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            target: None,
            policy: Loadable::Idle,
            policy_task: None,
            entries: Loadable::Idle,
            measuring: false,
            list_task: None,
            confirming: None,
            deleting: None,
            delete_task: None,
            refusal: None,
            report: None,
            error: None,
        }
    }

    pub fn load(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.target = target;
        self.confirming = None;
        self.deleting = None;
        self.delete_task = None;
        self.refusal = None;
        self.report = None;
        self.error = None;
        self.policy = Loadable::Loading;
        self.call_policy(None, cx);
        self.refresh(cx);
    }

    fn call_policy(&mut self, save: Option<WorktreeCleanupSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.policy = Loadable::Error("Device is not connected".into());
            cx.notify();
            return;
        };
        let (method, mut params) = match save {
            Some(settings) => (
                methods::SET_WORKTREE_CLEANUP_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (
                methods::GET_WORKTREE_CLEANUP_SETTINGS,
                serde_json::json!({}),
            ),
        };
        params["targetDeviceId"] = serde_json::json!(self.target);
        self.policy_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(unsupported)
                .and_then(|value| {
                    serde_json::from_value::<WorktreeCleanupSettings>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |view, cx| {
                match result {
                    Ok(settings) => {
                        let changed = view.policy.ready() != Some(&settings);
                        view.policy = Loadable::Ready(settings);
                        // Eligibility follows the policy.
                        if changed && save.is_some() {
                            view.refresh(cx);
                        }
                    }
                    Err(error) if view.policy.ready().is_some() => view.error = Some(error),
                    Err(error) => view.policy = Loadable::Error(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn save_policy(&mut self, settings: WorktreeCleanupSettings, cx: &mut Context<Self>) {
        if self.policy.ready().is_some() {
            self.call_policy(Some(settings), cx);
        }
    }

    /// List at once with the sizes already known, then again once the engine
    /// has measured the rest.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.entries = Loadable::Error("Device is not connected".into());
            cx.notify();
            return;
        };
        if self.entries.ready().is_none() {
            self.entries = Loadable::Loading;
        }
        let target = self.target.clone();
        self.measuring = true;
        self.list_task = Some(cx.spawn(async move |this, cx| {
            for sizes in [false, true] {
                let params = serde_json::json!({ "sizes": sizes, "targetDeviceId": target });
                let result = engine
                    .client()
                    .call(methods::LIST_WORKTREE_STORAGE, params)
                    .await
                    .map_err(unsupported)
                    .and_then(|value| {
                        serde_json::from_value::<Vec<WorktreeStorageEntry>>(value)
                            .map_err(|error| error.to_string())
                    });
                let failed = result.is_err();
                let measured = sizes
                    || result
                        .as_ref()
                        .is_ok_and(|entries| entries.iter().all(|e| e.size_bytes.is_some()));
                let alive = this
                    .update(cx, |view, cx| {
                        view.entries = match result {
                            Ok(entries) => Loadable::Ready(entries),
                            Err(error) => Loadable::Error(error),
                        };
                        view.measuring = !(failed || measured);
                        cx.notify();
                    })
                    .is_ok();
                if !alive || failed || measured {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn delete_one(&mut self, entry: WorktreeStorageEntry, force: bool, cx: &mut Context<Self>) {
        if self.deleting.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({
            "repoPath": entry.repo_path,
            "worktreePath": entry.path,
            "force": force,
            "targetDeviceId": self.target,
        });
        self.confirming = None;
        self.refusal = None;
        self.report = None;
        self.deleting = Some(Confirming::One(entry.path.clone()));
        self.delete_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::DELETE_WORKTREE, params).await;
            this.update(cx, |view, cx| {
                view.deleting = None;
                match result {
                    Ok(reply) => {
                        if let Some(teardown) = reply["teardownError"].as_str() {
                            view.error = Some(format!("Deleted, but teardown failed: {teardown}"));
                        }
                        view.refresh(cx);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        view.refusal = Some(Refusal {
                            dirty: message.starts_with(zeron_engine::repos::WORKTREE_DIRTY),
                            path: entry.path,
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

    fn delete_eligible(&mut self, cx: &mut Context<Self>) {
        if self.deleting.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({ "targetDeviceId": self.target });
        self.confirming = None;
        self.refusal = None;
        self.report = None;
        self.deleting = Some(Confirming::AllEligible);
        self.delete_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::CLEAN_WORKTREES, params)
                .await
                .map_err(unsupported)
                .and_then(|value| {
                    serde_json::from_value::<WorktreeCleanupReport>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |view, cx| {
                view.deleting = None;
                match result {
                    Ok(report) => {
                        view.report = Some(report_summary(&report));
                        if let Some(failed) = report.failed.first() {
                            view.error = Some(failed.error.clone());
                        }
                    }
                    Err(error) => view.error = Some(error),
                }
                view.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn confirm(&mut self, what: Option<Confirming>, cx: &mut Context<Self>) {
        self.confirming = what;
        self.refusal = None;
        cx.notify();
    }

    fn render_policy(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let ready = self.policy.ready().copied();
        let interactive = ready.is_some();
        let settings = ready.unwrap_or_default();
        let switch = |key: &'static str, label: &'static str, on: bool| {
            widgets::toggle_switch(theme, on, key)
                .id(SharedString::from(format!("{key}-toggle")))
                .flex_none()
                .role(gpui::Role::Switch)
                .aria_label(label)
                .aria_toggled(if on {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
                .when(!interactive, |el| el.opacity(0.5))
        };
        let merged = switch(
            "worktrees-delete-merged",
            "Delete merged worktrees",
            settings.delete_merged,
        )
        .when(interactive, |el| {
            el.cursor_pointer()
                .tab_index(0)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.save_policy(
                        WorktreeCleanupSettings {
                            delete_merged: !settings.delete_merged,
                            ..settings
                        },
                        cx,
                    )
                }))
        });
        let stale_on = settings.delete_after_days.is_some();
        let stale = switch(
            "worktrees-delete-stale",
            "Delete untouched worktrees",
            stale_on,
        )
        .when(interactive, |el| {
            el.cursor_pointer()
                .tab_index(0)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.save_policy(
                        WorktreeCleanupSettings {
                            delete_after_days: (!stale_on).then_some(DEFAULT_STALE_DAYS),
                            ..settings
                        },
                        cx,
                    )
                }))
        });
        let row = |title: &'static str, meta: &'static str| {
            div()
                .flex_1()
                .min_w(px(160.0))
                .child(widgets::row_title(theme, title))
                .child(widgets::meta_line(
                    theme,
                    vec![div().child(meta).into_any_element()],
                ))
        };
        let mut rows = vec![
            widgets::card_row(theme, false)
                .child(row(
                    "Delete merged worktrees",
                    "When the branch is merged into the default branch. Worktrees with uncommitted changes, unpushed commits or a running chat are always kept.",
                ))
                .child(merged)
                .into_any_element(),
            widgets::card_row(theme, false)
                .child(row(
                    "Delete untouched worktrees",
                    "When nothing has happened in them for a while.",
                ))
                .child(stale)
                .into_any_element(),
        ];
        if let Some(days) = settings.delete_after_days {
            let chips = WORKTREE_STALE_DAY_CHOICES.into_iter().map(|span| {
                let label = format!("{span} days");
                let chip = if span == days {
                    widgets::badge_active(theme, label)
                } else {
                    widgets::badge(theme, label)
                        .cursor_pointer()
                        .hover(|style| style.text_color(theme.text))
                };
                chip.id(SharedString::from(format!("worktrees-stale-{span}d")))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.save_policy(
                            WorktreeCleanupSettings {
                                delete_after_days: Some(span),
                                ..settings
                            },
                            cx,
                        )
                    }))
            });
            rows.push(
                widgets::card_row(theme, false)
                    .child(
                        div()
                            .flex_1()
                            .child(widgets::row_title(theme, "After no activity for")),
                    )
                    .child(div().flex().flex_row().gap(px(6.0)).children(chips))
                    .into_any_element(),
            );
        }
        rows
    }

    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let entries = self.entries.ready();
        let eligible = entries.map_or(0, |e| e.iter().filter(|e| e.decision.deletes()).count());
        let summary = match &self.entries {
            Loadable::Ready(entries) if entries.is_empty() => {
                "No worktrees made by Godspeed".to_string()
            }
            Loadable::Ready(entries) => {
                let total: u64 = entries.iter().filter_map(|e| e.size_bytes).sum();
                let count = match entries.len() {
                    1 => "1 worktree".to_string(),
                    n => format!("{n} worktrees"),
                };
                if self.measuring {
                    format!("{count} · measuring…")
                } else {
                    format!("{count} · {}", format_bytes(total))
                }
            }
            Loadable::Error(error) => error.clone(),
            _ => "Looking…".to_string(),
        };
        let busy = self.deleting.is_some();
        let confirming_all = self.confirming == Some(Confirming::AllEligible);
        let mut actions = div().flex_none().flex().items_center().gap(px(8.0));
        if confirming_all {
            actions = actions
                .child(
                    widgets::text_action(theme, widgets::ActionTone::Quiet, "Cancel")
                        .id("worktrees-clean-cancel")
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(|view, _, _, cx| view.confirm(None, cx))),
                )
                .child(
                    widgets::text_action(
                        theme,
                        widgets::ActionTone::Danger,
                        format!("Delete {eligible}"),
                    )
                    .id("worktrees-clean-confirm")
                    .tab_index(0)
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!(
                        "Confirm deleting {eligible} worktrees"
                    )))
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(|view, _, _, cx| view.delete_eligible(cx)))
                    }),
                );
        } else {
            let label = if self.deleting == Some(Confirming::AllEligible) {
                "Deleting…".to_string()
            } else {
                format!("Delete all eligible ({eligible})")
            };
            let enabled = eligible > 0 && !busy;
            actions = actions.child(
                widgets::text_action(theme, widgets::ActionTone::Filled, label)
                    .id("worktrees-clean")
                    .role(gpui::Role::Button)
                    .when(enabled, |el| {
                        el.tab_index(0)
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.confirm(Some(Confirming::AllEligible), cx)
                            }))
                    })
                    .when(!enabled, |el| el.opacity(0.5)),
            );
        }
        actions = actions.child(
            widgets::action_button(theme, widgets::ActionTone::Quiet)
                .id("worktree-storage-refresh")
                .w(px(32.0))
                .px_0()
                .justify_center()
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label("Refresh worktree storage")
                .tooltip(widgets::text_tooltip("Refresh"))
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .child(
                    icons::icon(icons::REFRESH)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
        );
        let mut meta = vec![div().truncate().child(summary).into_any_element()];
        if let Some(report) = &self.report {
            meta.push(div().truncate().child(report.clone()).into_any_element());
        }
        widgets::card_row(theme, false)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, "Storage"))
                    .child(widgets::meta_line(theme, meta)),
            )
            .child(actions)
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        entry: &WorktreeStorageEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = std::path::Path::new(&entry.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.path.clone());
        let now = chrono::Utc::now();
        let mut meta: Vec<AnyElement> = vec![
            div()
                .child(entry.branch.clone().unwrap_or_else(|| "Detached".into()))
                .into_any_element(),
        ];
        meta.push(
            div()
                .child(match entry.size_bytes {
                    Some(bytes) => format_bytes(bytes),
                    None => "Measuring…".into(),
                })
                .into_any_element(),
        );
        if let Some(at) = entry.last_activity {
            meta.push(
                div()
                    .child(format!(
                        "Active {}",
                        zeron_proto::view::format_time_ago(at, now)
                    ))
                    .into_any_element(),
            );
        }
        meta.push(
            div()
                .truncate()
                .child(match (&entry.chat_title, &entry.chat_id) {
                    (Some(title), _) => title.clone(),
                    (None, Some(_)) => "Untitled chat".into(),
                    (None, None) => "No chat".into(),
                })
                .into_any_element(),
        );
        let mut badges: Vec<AnyElement> = Vec::new();
        if entry.merged {
            badges.push(widgets::badge(theme, "Merged").into_any_element());
        }
        if entry.dirty {
            badges.push(widgets::badge(theme, "Uncommitted changes").into_any_element());
        }
        if entry.unpushed {
            badges.push(widgets::badge(theme, "Unpushed").into_any_element());
        }
        if entry.running {
            badges.push(widgets::badge(theme, "Running").into_any_element());
        }
        match (entry.decision.reason, entry.decision.kept) {
            (Some(_), Some(kept)) => {
                badges.push(widgets::badge(theme, kept.label()).into_any_element())
            }
            (Some(reason), None) => badges.push(
                widgets::badge_active(theme, format!("Eligible: {}", reason.label()))
                    .into_any_element(),
            ),
            _ => {}
        }

        let refusal = self.refusal.as_ref().filter(|r| r.path == entry.path);
        let force = refusal.is_some_and(|r| r.dirty);
        let busy = self.deleting.is_some();
        let deleting = self.deleting == Some(Confirming::One(entry.path.clone()));
        let confirming = self.confirming == Some(Confirming::One(entry.path.clone()));
        let target = entry.clone();
        let path = entry.path.clone();
        let mut actions = div().flex_none().flex().items_center().gap(px(8.0));
        if confirming || force {
            actions = actions
                .child(
                    widgets::text_action(theme, widgets::ActionTone::Quiet, "Cancel")
                        .id(("worktree-delete-cancel", index))
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.refusal = None;
                            view.confirm(None, cx)
                        })),
                )
                .child(
                    widgets::text_action(
                        theme,
                        widgets::ActionTone::Danger,
                        if force { "Delete anyway" } else { "Delete now" },
                    )
                    .id(("worktree-delete-confirm", index))
                    .tab_index(0)
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!(
                        "Confirm deleting worktree {name}"
                    )))
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .when(deleting, |el| el.opacity(0.5))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |view, _, _, cx| {
                            view.delete_one(target.clone(), force, cx)
                        }))
                    }),
                );
        } else {
            // A running chat's checkout can't be pulled out from under it.
            let enabled = !busy && !entry.running;
            actions = actions.child(
                widgets::action_button(theme, widgets::ActionTone::Quiet)
                    .w(px(32.0))
                    .px_0()
                    .justify_center()
                    .child(
                        icons::icon(icons::TRASH_BIN_MINIMALISTIC)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                    .id(("worktree-delete", index))
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!("Delete worktree {name}")))
                    .tooltip(widgets::text_tooltip(if entry.running {
                        "A chat is running in it"
                    } else {
                        "Delete now"
                    }))
                    .when(enabled, |el| {
                        el.tab_index(0)
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.confirm(Some(Confirming::One(path.clone())), cx)
                            }))
                    })
                    .when(!enabled, |el| el.opacity(0.5)),
            );
        }
        div()
            .child(
                widgets::card_row(theme, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(theme, name.clone()))
                            .child(widgets::meta_line(theme, meta))
                            .when(!badges.is_empty(), |el| {
                                el.child(
                                    div()
                                        .mt(px(6.0))
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap(px(6.0))
                                        .children(badges),
                                )
                            }),
                    )
                    .child(actions),
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

impl Render for WorktreeStorage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let policy = self.render_policy(&theme, cx);
        let header = self.render_header(&theme, cx);
        let entries = self.entries.ready().cloned().unwrap_or_default();
        let rows: Vec<AnyElement> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| self.render_row(index, entry, &theme, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .children(policy)
            .child(header)
            .children(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::worktree_cleanup::{DeletedWorktree, KeptWorktree};
    use zeron_proto::{WorktreeCleanupReason, WorktreeKeepReason};

    #[test]
    fn the_report_reads_as_one_line() {
        assert_eq!(
            report_summary(&WorktreeCleanupReport::default()),
            "Nothing deleted"
        );
        let report = WorktreeCleanupReport {
            deleted: vec![
                DeletedWorktree {
                    path: "/w/a".into(),
                    branch: None,
                    reason: WorktreeCleanupReason::Merged,
                    teardown_error: Some("exit 1".into()),
                },
                DeletedWorktree {
                    path: "/w/b".into(),
                    branch: None,
                    reason: WorktreeCleanupReason::Unused,
                    teardown_error: None,
                },
            ],
            kept: vec![KeptWorktree {
                path: "/w/c".into(),
                reason: WorktreeKeepReason::Uncommitted,
            }],
            failed: vec![],
        };
        assert_eq!(
            report_summary(&report),
            "Deleted 2 worktrees · kept 1 with changes or a running chat · 1 teardown failed"
        );
    }

    #[test]
    fn the_default_span_is_offered() {
        assert!(WORKTREE_STALE_DAY_CHOICES.contains(&DEFAULT_STALE_DAYS));
        assert!(
            !WorktreeCleanupSettings::default().enabled(),
            "off until chosen"
        );
    }
}
