//! Worktree environments in the session card: the chat's worktree ports,
//! setup outcome and sleep state with Sleep/Wake, waking a sleeping worktree
//! when its chat is opened, and the project's Environment editor.
use super::*;

use zeron_proto::{
    WakeWorktreeOutcome, WorktreeEnvironment, WorktreeEnvironmentSnapshot,
    WorktreeEnvironmentStatus, WorktreeSetupState,
};

use super::actions_ui::project_action_params;
use crate::worktree_env::{
    EnvironmentDraft, EnvironmentEditor, EnvironmentFields, WorktreeKey, WorktreeStatusEntry,
};

const SETUP_RECHECK: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct WorktreeTarget {
    key: WorktreeKey,
    chat_id: String,
    target_device_id: Option<String>,
}

fn row_icon(path: &'static str, color: gpui::Hsla) -> AnyElement {
    icon(path)
        .size(px(15.0))
        .text_color(color)
        .into_any_element()
}

impl Shell {
    /// The selected chat's linked worktree (a cwd other than its project's
    /// folder, on the project's device).
    fn worktree_target(&self, cx: &App) -> Option<WorktreeTarget> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let space = state.space_row(chat.space_id.as_deref()?)?;
        if space.device_id != chat.device_id {
            return None;
        }
        let cwd = chat.cwd.clone().filter(|cwd| *cwd != space.path)?;
        let target_device_id = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        Some(WorktreeTarget {
            key: WorktreeKey {
                device_id: chat.device_id.clone(),
                path: cwd,
            },
            chat_id: chat.id.clone(),
            target_device_id,
        })
    }

    /// Read the selected chat's worktree status when the chat changes; a
    /// sleeping worktree wakes as its chat opens.
    pub(super) fn ensure_worktree_status(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| chat.id.clone());
        let changed = self.worktree_env.select(selected.as_deref());
        let Some(target) = self.worktree_target(cx) else {
            return;
        };
        if changed || !self.worktree_env.statuses.contains_key(&target.key) {
            self.refresh_worktree_status(target, cx);
        }
    }

    fn refresh_worktree_status(&mut self, target: WorktreeTarget, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.worktree_env
            .statuses
            .entry(target.key.clone())
            .or_insert(WorktreeStatusEntry::Loading);
        let params = project_action_params(
            serde_json::json!({ "path": target.key.path }),
            &target.target_device_id,
        );
        self.worktree_env.status_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<Option<WorktreeEnvironmentStatus>>(methods::GET_WORKTREE_STATUS, params)
                .await
                .map_err(|error| error.to_string());
            let setup_running = matches!(
                &result,
                Ok(Some(status)) if status.setup == Some(WorktreeSetupState::Running)
            );
            let key = target.key.clone();
            let woke = this.update(cx, |shell, cx| {
                let wake = shell.worktree_env.accept_status(target.key.clone(), result);
                if wake {
                    shell.wake_worktree(cx);
                }
                cx.notify();
            });
            if woke.is_err() || !setup_running {
                return;
            }
            cx.background_executor().timer(SETUP_RECHECK).await;
            this.update(cx, |shell, cx| {
                if let Some(target) = shell.worktree_target(cx)
                    && target.key == key
                {
                    shell.refresh_worktree_status(target, cx);
                }
            })
            .ok();
        }));
    }

    fn sleep_worktree(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(engine)) = (
            self.worktree_target(cx),
            self.state.read(cx).engine().cloned(),
        ) else {
            return;
        };
        if self.worktree_env.busy {
            return;
        }
        self.worktree_env.busy = true;
        self.worktree_env.wake_on_open = false;
        let params = project_action_params(
            serde_json::json!({ "path": target.key.path }),
            &target.target_device_id,
        );
        self.worktree_env.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<WorktreeEnvironmentStatus>(methods::SLEEP_WORKTREE, params)
                .await;
            this.update(cx, |shell, cx| {
                shell.worktree_env.busy = false;
                match result {
                    Ok(status) => {
                        shell
                            .worktree_env
                            .statuses
                            .insert(target.key, WorktreeStatusEntry::Ready(Some(status)));
                    }
                    Err(error) => shell.show_notice(
                        crate::toast::ToastKind::Error,
                        format!("Couldn't put the worktree to sleep: {error}"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn wake_worktree(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(engine)) = (
            self.worktree_target(cx),
            self.state.read(cx).engine().cloned(),
        ) else {
            return;
        };
        if self.worktree_env.busy {
            return;
        }
        self.worktree_env.busy = true;
        let params = project_action_params(
            serde_json::json!({ "path": target.key.path, "cols": 120, "rows": 32 }),
            &target.target_device_id,
        );
        self.worktree_env.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<Option<WakeWorktreeOutcome>>(methods::WAKE_WORKTREE, params)
                .await;
            this.update(cx, |shell, cx| {
                shell.worktree_env.busy = false;
                match result {
                    Ok(Some(outcome)) => {
                        shell.worktree_env.statuses.insert(
                            target.key.clone(),
                            WorktreeStatusEntry::Ready(Some(outcome.status)),
                        );
                        if let Some(run) = outcome.wake_action {
                            shell.attach_host_terminal(
                                target.chat_id,
                                "Wake".into(),
                                run,
                                target.target_device_id,
                                cx,
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => shell.show_notice(
                        crate::toast::ToastKind::Error,
                        format!("Couldn't wake the worktree: {error}"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The card's worktree rows: ports, a failed setup, the sleep state and
    /// Sleep/Wake. Empty outside a worktree.
    pub(super) fn render_session_info_worktree(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.ensure_worktree_status(cx);
        let Some(target) = self.worktree_target(cx) else {
            return Vec::new();
        };
        if matches!(
            self.worktree_env.statuses.get(&target.key),
            None | Some(WorktreeStatusEntry::Unavailable)
        ) {
            return Vec::new();
        }
        let status = self.worktree_env.status(&target.key).cloned();
        let mut rows = Vec::new();
        let label_row = |glyph: AnyElement, label: SharedString| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .min_h(px(popover::PICKER_ROW_HEIGHT))
                .px(px(9.0))
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text)
                .child(popover::picker_icon_slot(glyph))
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        if let Some(ports) = status.as_ref().and_then(|status| status.ports.as_ref()) {
            let label: SharedString = format!("Ports {}", ports.label()).into();
            rows.push(
                label_row(row_icon(icons::GLOBE, theme.text_muted), label)
                    .id("session-info-ports")
                    .tooltip(crate::settings::widgets::text_tooltip(ports.detail()))
                    .into_any_element(),
            );
        }
        if let Some(WorktreeSetupState::Failed {
            step,
            exit_code,
            command,
        }) = status.as_ref().and_then(|status| status.setup.clone())
        {
            rows.push(
                label_row(
                    row_icon(icons::DANGER_TRIANGLE, theme.warning),
                    format!("Setup failed at step {step} (exit {exit_code})").into(),
                )
                .id("session-info-setup-failed")
                .tooltip(crate::settings::widgets::text_tooltip(command))
                .into_any_element(),
            );
        }
        let sleeping = status.as_ref().is_some_and(|status| status.sleeping);
        if sleeping {
            rows.push(
                label_row(row_icon(icons::MOON, theme.text_muted), "Sleeping".into())
                    .into_any_element(),
            );
        }
        let busy = self.worktree_env.busy;
        let (glyph, label) = if sleeping {
            (icons::SUN, "Wake worktree")
        } else {
            (icons::MOON, "Sleep worktree")
        };
        rows.push(
            popover::picker_row(theme, false, false, "session-info-worktree-sleep")
                .id("session-info-worktree-sleep")
                .when(busy, |row| row.opacity(0.5))
                .when(!sleeping, |row| {
                    row.tooltip(crate::settings::widgets::text_tooltip(
                        "Stop this worktree's terminals and dev servers",
                    ))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if sleeping {
                        this.wake_worktree(cx);
                    } else {
                        this.sleep_worktree(cx);
                    }
                }))
                .child(popover::picker_icon_slot(row_icon(glyph, theme.text_muted)))
                .child(div().flex_1().child(label))
                .into_any_element(),
        );
        rows
    }

    /// The Actions section's entry to the project's environment editor.
    pub(super) fn render_environment_entry(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::picker_row(theme, false, false, "session-info-environment")
            .id("session-info-environment")
            .tooltip(crate::settings::widgets::text_tooltip(
                "Files, ports and commands for this project's worktrees",
            ))
            .on_click(cx.listener(|this, _, _, cx| this.open_environment_editor(cx)))
            .child(popover::picker_icon_slot(row_icon(
                icons::TUNING,
                theme.text_muted,
            )))
            .child(div().flex_1().child("Environment…"))
            .into_any_element()
    }

    fn open_environment_editor(&mut self, cx: &mut Context<Self>) {
        let (Some(context), Some(engine)) = (
            self.project_action_context(cx),
            self.state.read(cx).engine().cloned(),
        ) else {
            return;
        };
        if self.session_info_overlay {
            self.session_info_overlay = false;
        }
        let field = |placeholder: &'static str, single: bool, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let input = ComposerInput::new(placeholder, cx);
                if single {
                    input.with_single_line()
                } else {
                    input
                }
            })
        };
        let fields = EnvironmentFields {
            copy: field(".env*", false, cx),
            symlink: field("node_modules", false, cx),
            ports: field("None", true, cx),
            port_names: field("web=0, api=1", true, cx),
            setup: field("createdb app_{{slug}} -T app_dev", false, cx),
            teardown: field("dropdb --if-exists app_{{slug}}", false, cx),
            wake: field("pnpm dev --port {{port.web}}", false, cx),
            branch: field("{{prefix}}{{slug}}", true, cx),
        };
        let events = [
            &fields.copy,
            &fields.symlink,
            &fields.ports,
            &fields.port_names,
            &fields.setup,
            &fields.teardown,
            &fields.wake,
            &fields.branch,
        ]
        .into_iter()
        .map(|input| {
            cx.subscribe(
                input,
                |_: &mut Shell, _, _: &crate::composer::ComposerInputEvent, cx| cx.notify(),
            )
        })
        .collect();
        self.worktree_env.editor = Some(EnvironmentEditor {
            key: context.key.clone(),
            snapshot: None,
            load_error: None,
            fields,
            error: None,
            saving: false,
            focus_pending: true,
            _events: events,
        });
        let params = project_action_params(
            serde_json::json!({ "spaceId": context.key.space_id }),
            &context.target_device_id,
        );
        self.worktree_env.editor_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<WorktreeEnvironmentSnapshot>(methods::GET_WORKTREE_ENVIRONMENT, params)
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(snapshot) => {
                        let environment = snapshot.environment.clone();
                        if let Some(editor) = shell.worktree_env.editor.as_mut() {
                            editor.snapshot = Some(snapshot);
                        }
                        shell.fill_environment_editor(&environment, cx);
                    }
                    Err(error) => {
                        if let Some(editor) = shell.worktree_env.editor.as_mut() {
                            editor.load_error = Some(match error {
                                zeron_rpc::RpcError::UnknownMethod(_) => {
                                    "Update Godspeed on this project's device to edit its environment."
                                        .into()
                                }
                                error => error.to_string(),
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn fill_environment_editor(
        &mut self,
        environment: &WorktreeEnvironment,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.worktree_env.editor.as_ref() else {
            return;
        };
        let draft = EnvironmentDraft::from_environment(environment);
        let fields = &editor.fields;
        for (input, text) in [
            (&fields.copy, draft.copy),
            (&fields.symlink, draft.symlink),
            (&fields.ports, draft.ports),
            (&fields.port_names, draft.port_names),
            (&fields.setup, draft.setup),
            (&fields.teardown, draft.teardown),
            (&fields.wake, draft.wake),
            (&fields.branch, draft.branch),
        ] {
            input.update(cx, |input, cx| input.set_text(text, cx));
        }
    }

    fn environment_draft(&self, cx: &App) -> Option<EnvironmentDraft> {
        let fields = &self.worktree_env.editor.as_ref()?.fields;
        let text = |input: &Entity<ComposerInput>| input.read(cx).text().to_string();
        Some(EnvironmentDraft {
            copy: text(&fields.copy),
            symlink: text(&fields.symlink),
            ports: text(&fields.ports),
            port_names: text(&fields.port_names),
            setup: text(&fields.setup),
            teardown: text(&fields.teardown),
            wake: text(&fields.wake),
            branch: text(&fields.branch),
        })
    }

    fn save_environment(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.environment_draft(cx) else {
            return;
        };
        let context = self.project_action_context(cx);
        let engine = self.state.read(cx).engine().cloned();
        let Some(editor) = self.worktree_env.editor.as_mut() else {
            return;
        };
        if editor.saving || editor.snapshot.is_none() {
            return;
        }
        let environment = match draft.to_environment() {
            Ok(environment) => environment,
            Err(error) => {
                editor.error = Some(error);
                cx.notify();
                return;
            }
        };
        let (Some(context), Some(engine)) = (context, engine) else {
            editor.error = Some("The project is no longer available".into());
            cx.notify();
            return;
        };
        if context.key != editor.key {
            editor.error = Some("The selected project changed".into());
            cx.notify();
            return;
        }
        editor.saving = true;
        editor.error = None;
        let params = project_action_params(
            serde_json::json!({ "spaceId": context.key.space_id, "environment": environment }),
            &context.target_device_id,
        );
        self.worktree_env.editor_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<WorktreeEnvironmentSnapshot>(methods::SET_WORKTREE_ENVIRONMENT, params)
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => shell.worktree_env.editor = None,
                    Err(error) => {
                        if let Some(editor) = shell.worktree_env.editor.as_mut() {
                            editor.saving = false;
                            editor.error = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn import_environment(&mut self, cx: &mut Context<Self>) {
        let Some(importable) = self
            .worktree_env
            .editor
            .as_ref()
            .and_then(|editor| editor.snapshot.as_ref())
            .and_then(|snapshot| snapshot.importable.clone())
        else {
            return;
        };
        self.fill_environment_editor(&importable, cx);
        if let Some(editor) = self.worktree_env.editor.as_mut() {
            editor.error = None;
        }
        cx.notify();
    }

    pub(super) fn render_environment_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.worktree_env.editor.as_mut()?;
        if std::mem::take(&mut editor.focus_pending) {
            window.focus(&editor.fields.copy.focus_handle(cx), cx);
        }
        let theme = Theme::of(cx).clone();
        let loaded = editor.snapshot.is_some();
        let saving = editor.saving;
        let issue = editor
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.project_file_issue.clone());
        let importable = editor
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.importable.is_some());
        let message = editor.load_error.clone().or_else(|| editor.error.clone());
        let fields = &editor.fields;
        let multiline = |input: &Entity<ComposerInput>, height: f32| {
            popover::dialog_field(
                div()
                    .h(px(height))
                    .overflow_hidden()
                    .child(input.clone())
                    .into_any_element(),
            )
            .font_family(theme.font_mono.clone())
        };
        let single = |input: &Entity<ComposerInput>| {
            popover::dialog_field(input.clone().into_any_element())
                .font_family(theme.font_mono.clone())
        };
        let body = div()
            .id("environment-editor-body")
            .max_h(px((f32::from(viewport.height) - 220.0).max(240.0)))
            .overflow_y_scroll()
            .child(field_label(&theme, "Copy into new worktrees", "Untracked files from the main checkout, one pattern per line"))
            .child(multiline(&fields.copy, 44.0))
            .child(field_label(&theme, "Symlink into new worktrees", "Shared instead of copied, like dependency folders"))
            .child(multiline(&fields.symlink, 44.0))
            .child(
                div()
                    .flex()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(120.0))
                            .child(field_label(&theme, "Ports", "Per worktree"))
                            .child(single(&fields.ports)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(field_label(&theme, "Port names", "name=offset"))
                            .child(single(&fields.port_names)),
                    ),
            )
            .child(field_label(&theme, "Setup", "Run in each new worktree, one command per line"))
            .child(multiline(&fields.setup, 66.0))
            .child(field_label(&theme, "Teardown", "Run before a worktree is deleted"))
            .child(multiline(&fields.teardown, 44.0))
            .child(field_label(&theme, "Wake", "Run when a sleeping worktree wakes"))
            .child(multiline(&fields.wake, 44.0))
            .child(field_label(&theme, "Branch name", "Uses {{prefix}}, {{slug}}, {{project}}; empty keeps the prefix setting"))
            .child(single(&fields.branch))
            .child(
                div()
                    .mt(px(10.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "Commands can use {{worktree}}, {{root}}, {{project}}, {{branch}}, {{slug}}, {{port}} and {{port.<name>}}.",
                    )),
            );
        let card = popover::dialog_card(&theme)
            .w(px(520.0))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.worktree_env.editor = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, "Worktree environment"))
            .child(body)
            .when_some(issue, |card, issue| {
                card.child(
                    div()
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(issue)),
                )
            })
            .when_some(message, |card, message| {
                card.child(
                    div()
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(message)),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().when(importable, |el| {
                        el.child(
                            popover::btn_ghost(
                                &theme,
                                "Import from godspeed.json",
                                "environment-import",
                            )
                            .id("environment-import")
                            .on_click(cx.listener(|this, _, _, cx| this.import_environment(cx))),
                        )
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "environment-cancel")
                                    .id("environment-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.worktree_env.editor = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if saving { "Saving…" } else { "Save" },
                                )
                                .id("environment-save")
                                .when(!loaded, |button| button.opacity(0.5))
                                .when(
                                    loaded && !saving,
                                    |button| {
                                        button.on_click(
                                            cx.listener(|this, _, _, cx| this.save_environment(cx)),
                                        )
                                    },
                                ),
                            ),
                    ),
            );
        Some(popover::modal(
            "worktree-environment-editor",
            viewport,
            card.into_any_element(),
        ))
    }
}

fn field_label(theme: &Theme, label: &str, hint: &str) -> gpui::Div {
    div()
        .mt(px(12.0))
        .mb(px(5.0))
        .flex()
        .items_baseline()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .truncate()
                .child(SharedString::from(hint.to_string())),
        )
}

/// The demo recording shows a worktree environment without an engine.
#[cfg(feature = "demo-fixture")]
impl Shell {
    pub fn fixture_demo_worktree(&mut self, sleeping: bool, cx: &mut Context<Self>) {
        if let Some(target) = self.worktree_target(cx) {
            let mut names = std::collections::BTreeMap::new();
            names.insert("web".to_string(), 0);
            names.insert("api".to_string(), 1);
            names.insert("db".to_string(), 2);
            let _ = self.worktree_env.accept_status(
                target.key.clone(),
                Ok(Some(WorktreeEnvironmentStatus {
                    path: target.key.path.clone(),
                    ports: Some(zeron_proto::WorktreePorts {
                        base: 4100,
                        count: 3,
                        names,
                    }),
                    sleeping,
                    setup: None,
                })),
            );
        }
        self.show_session_info(cx);
        cx.notify();
    }
}
