//! "Add to <device>…": clone a project onto a device that doesn't have it,
//! bringing the ignored files the user ticks (env files by default).
//!
//! The dialog lists the source's ignored files, reads the ticked ones when
//! Clone is pressed (so the copy reflects them as they were then), asks the
//! target to clone and register the project, then opens the new project on
//! the target. The files follow through [`crate::add_to_device::start_relay`]
//! while the composer holds sends. Progress stays in the dialog; nothing
//! toasts on success.
use super::*;

use std::sync::Arc;

use zeron_proto::project_transfer::{CloneProgress, CloneProtocol, IgnoredFileListing};

use crate::add_to_device::{
    DialogModel, Listing, RelayFile, TransferPhase, device_params, format_bytes, read_project_file,
    start_relay,
};

/// How long a finished clone waits for its project row to sync here before
/// the dialog gives up on opening it.
const OPEN_WAIT: Duration = Duration::from_secs(30);
const OPEN_POLL: Duration = Duration::from_millis(200);

pub(super) struct AddToDeviceDialog {
    model: DialogModel,
    /// The source project's own name override, carried to the new project.
    name: Option<String>,
    destination: Entity<ComposerInput>,
    /// The default destination was filled in (or the user typed first).
    destination_filled: bool,
    focus_pending: bool,
    clone_id: Option<String>,
    listing_task: Option<Task<()>>,
    task: Option<Task<()>>,
    _events: Subscription,
}

impl Shell {
    /// Open the dialog for adding `space_id`'s project to `device_id`.
    pub(super) fn open_add_to_device(
        &mut self,
        space_id: String,
        device_id: String,
        cx: &mut Context<Self>,
    ) {
        self.close_space_menu_for_transfer(cx);
        let now = Utc::now();
        let prepared = {
            let state = self.state.read(cx);
            let Some(space) = state.space_row(&space_id).cloned() else {
                return;
            };
            let target = state
                .add_targets(&space, now)
                .into_iter()
                .find(|target| target.device_id == device_id);
            let source_name = state
                .device_name(&space.device_id)
                .unwrap_or("this device")
                .to_string();
            let project_name = state
                .representative_space(&space)
                .display_name()
                .to_string();
            (space, target, source_name, project_name)
        };
        let (space, target, source_name, project_name) = prepared;
        let Some(target) = target else {
            return;
        };
        if let Some(reason) = target.disabled.clone() {
            self.show_notice(crate::toast::ToastKind::Error, reason, cx);
            return;
        }
        let model = DialogModel::new(&space, &source_name, &project_name, &target);
        let destination = cx.new(|cx| ComposerInput::new("~/code/project", cx));
        let events = cx.subscribe(&destination, |this: &mut Shell, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.submit_add_to_device(cx),
            ComposerInputEvent::Edited => {
                if let Some(dialog) = this.add_to_device.as_mut() {
                    dialog.destination_filled = true;
                }
            }
            _ => {}
        });
        self.add_to_device = Some(AddToDeviceDialog {
            model,
            name: space.name.clone(),
            destination,
            destination_filled: false,
            focus_pending: true,
            clone_id: None,
            listing_task: None,
            task: None,
            _events: events,
        });
        self.load_transfer_listing(cx);
        cx.notify();
    }

    fn close_space_menu_for_transfer(&mut self, cx: &mut Context<Self>) {
        if self.space_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.space_menu);
        }
    }

    /// Pick up an "Add to <device>…" request from the composer's device picker.
    pub(super) fn take_add_to_device_request(
        &mut self,
        state: &Entity<AppState>,
        cx: &mut Context<Self>,
    ) {
        let Some((space_id, device_id)) = state.read(cx).add_to_device_request.clone() else {
            return;
        };
        state.update(cx, |state, _| state.add_to_device_request = None);
        if self
            .add_to_device
            .as_ref()
            .is_some_and(|dialog| dialog.model.phase.busy())
        {
            return;
        }
        self.open_add_to_device(space_id, device_id, cx);
    }

    fn load_transfer_listing(&mut self, cx: &mut Context<Self>) {
        let (engine, local) = {
            let state = self.state.read(cx);
            (state.engine().cloned(), state.local_device_id.clone())
        };
        let Some(dialog) = self.add_to_device.as_mut() else {
            return;
        };
        let Some(engine) = engine else {
            dialog
                .model
                .accept_listing(Err("The engine is not connected".into()));
            return;
        };
        let source = dialog.model.source_device_id.clone();
        let source_name = dialog.model.source_name.clone();
        let params = device_params(
            serde_json::json!({ "spaceId": dialog.model.source_space_id }),
            &source,
            local.as_deref(),
        );
        dialog.listing_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<IgnoredFileListing>(methods::LIST_IGNORED_FILES, params)
                .await
                .map_err(|error| match error {
                    zeron_rpc::RpcError::UnknownMethod(_) => {
                        format!("Update Godspeed on {source_name}")
                    }
                    error => error.to_string(),
                });
            this.update(cx, |shell, cx| {
                let Some(dialog) = shell.add_to_device.as_mut() else {
                    return;
                };
                dialog.model.accept_listing(result);
                if !dialog.destination_filled
                    && dialog.destination.read(cx).text().trim().is_empty()
                {
                    let default = dialog.model.default_destination();
                    dialog
                        .destination
                        .update(cx, |input, cx| input.set_text(default, cx));
                    dialog.destination_filled = true;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn close_add_to_device(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.add_to_device.as_mut() else {
            return;
        };
        if matches!(
            dialog.model.phase,
            TransferPhase::Cloning { .. } | TransferPhase::Reading { .. }
        ) {
            self.cancel_add_to_device(cx);
            return;
        }
        if dialog.model.phase.busy() {
            return; // registering/opening: almost done, nothing to cancel
        }
        self.add_to_device = None;
        cx.notify();
    }

    fn cancel_add_to_device(&mut self, cx: &mut Context<Self>) {
        let (engine, local) = {
            let state = self.state.read(cx);
            (state.engine().cloned(), state.local_device_id.clone())
        };
        let Some(dialog) = self.add_to_device.as_mut() else {
            return;
        };
        match (&dialog.model.phase, dialog.clone_id.clone(), engine) {
            (TransferPhase::Reading { .. }, ..) => {
                dialog.task = None;
                dialog.model.phase = TransferPhase::Editing;
            }
            (TransferPhase::Cloning { .. }, Some(clone_id), Some(engine)) => {
                dialog.model.phase = TransferPhase::Cancelling;
                let params = device_params(
                    serde_json::json!({ "cloneId": clone_id }),
                    &dialog.model.target_device_id,
                    local.as_deref(),
                );
                cx.spawn(async move |_, _| {
                    if let Err(error) = engine
                        .client()
                        .call(methods::CANCEL_PROJECT_CLONE, params)
                        .await
                    {
                        tracing::warn!(%error, "cancelling the project clone failed");
                    }
                })
                .detach();
            }
            _ => {}
        }
        cx.notify();
    }

    fn pick_transfer_protocol(&mut self, protocol: CloneProtocol, cx: &mut Context<Self>) {
        if let Some(dialog) = self.add_to_device.as_mut()
            && dialog.model.pick_protocol(protocol)
        {
            cx.notify();
        }
    }

    fn toggle_transfer_file(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(dialog) = self.add_to_device.as_mut()
            && !dialog.model.phase.busy()
            && dialog.model.selection.toggle(&path)
        {
            cx.notify();
        }
    }

    fn submit_add_to_device(&mut self, cx: &mut Context<Self>) {
        let (engine, local) = {
            let state = self.state.read(cx);
            (state.engine().cloned(), state.local_device_id.clone())
        };
        let Some(dialog) = self.add_to_device.as_mut() else {
            return;
        };
        let destination = dialog.destination.read(cx).text().trim().to_string();
        if dialog.model.submit_blocked(&destination).is_some() {
            return;
        }
        let (Some(engine), Some(url)) = (engine, dialog.model.clone_url()) else {
            return;
        };
        let model = dialog.model.clone();
        let files = model.selection.chosen();
        let name = dialog.name.clone();
        let clone_id = uuid::Uuid::new_v4().to_string();
        let space_id = uuid::Uuid::new_v4().to_string();
        dialog.clone_id = Some(clone_id.clone());
        dialog.model.phase = TransferPhase::Reading {
            done: 0,
            total: files.len(),
        };
        let state = self.state.clone();
        dialog.task = Some(cx.spawn(async move |this, cx| {
            let set_phase = |phase: TransferPhase, cx: &mut gpui::AsyncApp| {
                this.update(cx, |shell, cx| {
                    if let Some(dialog) = shell.add_to_device.as_mut() {
                        dialog.model.phase = phase;
                    }
                    cx.notify();
                })
                .is_ok()
            };
            // Read first, so the copy reflects the files as they were when
            // the user asked, and nothing clones if one can't be read.
            let mut relay_files = Vec::with_capacity(files.len());
            for (ix, file) in files.iter().enumerate() {
                match read_project_file(
                    engine.client(),
                    &model.source_device_id,
                    local.as_deref(),
                    &model.source_space_id,
                    &file.path,
                )
                .await
                {
                    Ok(bytes) => relay_files.push(RelayFile {
                        path: file.path.clone(),
                        bytes: Arc::new(bytes),
                    }),
                    Err(error) => {
                        set_phase(
                            TransferPhase::Failed(format!(
                                "Couldn't read {} on {}: {error}",
                                file.path, model.source_name
                            )),
                            cx,
                        );
                        return;
                    }
                }
                if !set_phase(
                    TransferPhase::Reading {
                        done: ix + 1,
                        total: files.len(),
                    },
                    cx,
                ) {
                    return;
                }
            }

            set_phase(
                TransferPhase::Cloning {
                    stage: String::new(),
                    percent: None,
                },
                cx,
            );
            let params = device_params(
                serde_json::json!({
                    "cloneId": clone_id,
                    "url": url,
                    "destination": destination,
                    "subdirectory": model.subdirectory(),
                    "spaceId": space_id,
                    "name": name,
                }),
                &model.target_device_id,
                local.as_deref(),
            );
            let mut stream = match engine
                .client()
                .subscribe_checked(methods::CLONE_PROJECT, params)
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    let message = match error {
                        zeron_rpc::RpcError::UnknownMethod(_) => {
                            format!("Update Godspeed on {}", model.target_name)
                        }
                        error => error.to_string(),
                    };
                    set_phase(TransferPhase::Failed(message), cx);
                    return;
                }
            };
            let mut registered: Option<String> = None;
            while let Some(item) = stream.recv().await {
                let Ok(progress) = serde_json::from_value::<CloneProgress>(item) else {
                    continue;
                };
                match progress {
                    CloneProgress::Starting => {}
                    CloneProgress::Cloning { stage, percent } => {
                        set_phase(TransferPhase::Cloning { stage, percent }, cx);
                    }
                    CloneProgress::Registering => {
                        set_phase(TransferPhase::Registering, cx);
                    }
                    CloneProgress::Done { space_id, .. } => {
                        registered = Some(space_id);
                        break;
                    }
                    CloneProgress::Failed { message } => {
                        set_phase(TransferPhase::Failed(message), cx);
                        return;
                    }
                    CloneProgress::Cancelled => {
                        set_phase(TransferPhase::Editing, cx);
                        return;
                    }
                }
            }
            let Some(new_space) = registered else {
                set_phase(
                    TransferPhase::Failed(format!(
                        "Lost contact with {} during the clone",
                        model.target_name
                    )),
                    cx,
                );
                return;
            };

            // The checkout exists: the files start for it right away, and the
            // composer holds sends until they land.
            set_phase(TransferPhase::Opening, cx);
            let relay_space = new_space.clone();
            let target_id = model.target_device_id.clone();
            let target_name = model.target_name.clone();
            cx.update(|cx| {
                start_relay(&state, relay_space, target_id, target_name, relay_files, cx);
            });
            let deadline = std::time::Instant::now() + OPEN_WAIT;
            loop {
                let synced = cx.update(|cx| state.read(cx).space_row(&new_space).is_some());
                if synced || std::time::Instant::now() >= deadline {
                    this.update(cx, |shell, cx| {
                        shell.add_to_device = None;
                        if synced {
                            shell.open_new_session(Some(new_space.clone()), cx);
                        } else {
                            shell.show_notice(
                                crate::toast::ToastKind::Info,
                                format!(
                                    "Added to {}. It appears here once it syncs.",
                                    model.target_name
                                ),
                                cx,
                            );
                        }
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                cx.background_executor().timer(OPEN_POLL).await;
            }
        }));
        cx.notify();
    }

    pub(super) fn render_add_to_device_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.add_to_device.as_mut()?;
        if std::mem::take(&mut dialog.focus_pending) {
            window.focus(&dialog.destination.focus_handle(cx), cx);
        }
        let theme = Theme::of(cx).for_popup();
        let model = dialog.model.clone();
        let destination_input = dialog.destination.clone();
        let destination = destination_input.read(cx).text().trim().to_string();
        let busy = model.phase.busy();
        let blocked = model.submit_blocked(&destination);
        let mono = theme.font_mono.clone();

        let route = div()
            .mt(px(14.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(route_end(
                &theme,
                &model.source_name,
                &model.source_path,
                mono.clone(),
            ))
            .child(
                icon(icons::ARROW_RIGHT)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_faint),
            )
            .child(route_end(
                &theme,
                &model.target_name,
                if destination.is_empty() {
                    "…"
                } else {
                    destination.as_str()
                },
                mono.clone(),
            ));

        let protocol_toggle = model.shows_protocol().then(|| {
            let tab = |protocol: CloneProtocol, label: &'static str, cx: &mut Context<Self>| {
                let selected = model.protocol == Some(protocol);
                let available = model.protocol_available(protocol);
                crate::settings::widgets::section_tab(
                    &theme,
                    selected,
                    if selected { 1.0 } else { 0.0 },
                    format!("transfer-protocol-{label}"),
                    format!("transfer-protocol-hover-{label}"),
                )
                .min_h(px(24.0))
                .py(px(2.0))
                .text_size(crate::typography::ui_rems(12.0))
                .when(!available || busy, |el| el.opacity(0.4).cursor_default())
                .when(!available, |el| {
                    el.tooltip(crate::settings::widgets::text_tooltip(
                        "This remote can't be rewritten (it uses a custom port)",
                    ))
                })
                .when(available && !busy, |el| {
                    el.cursor_pointer().on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.pick_transfer_protocol(protocol, cx)
                        }),
                    )
                })
                .child(SharedString::from(label))
            };
            div()
                .flex()
                .flex_row()
                .gap(px(2.0))
                .child(tab(CloneProtocol::Ssh, "SSH", cx))
                .child(tab(CloneProtocol::Https, "HTTPS", cx))
        });
        let url_line: SharedString = model
            .clone_url()
            .or_else(|| model.remote_url().map(str::to_string))
            .unwrap_or_else(|| match &model.listing {
                Listing::Loading => "…".into(),
                _ => "No remote".into(),
            })
            .into();

        let files_body: AnyElement = match &model.listing {
            Listing::Loading => {
                popover::skeleton_rows("transfer-files-loading", &theme, 3, cx.entity_id(), cx)
                    .into_any_element()
            }
            Listing::Failed(error) => div()
                .text_size(px(12.0))
                .text_color(theme.danger)
                .child(SharedString::from(error.clone()))
                .into_any_element(),
            Listing::Ready(listing) if listing.files.is_empty() => div()
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(
                    "Git ignores no files here worth copying.",
                ))
                .into_any_element(),
            Listing::Ready(listing) => {
                let rows: Vec<AnyElement> = model
                    .selection
                    .files
                    .iter()
                    .enumerate()
                    .map(|(ix, file)| {
                        let checked = model.selection.selected.contains(&file.path);
                        let block = model.selection.block(file);
                        let path = file.path.clone();
                        let row = div()
                            .id(("transfer-file", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(6.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .when(block.is_none() && !busy, |el| {
                                el.cursor_pointer()
                                    .hover(|s| s.bg(theme.glass_hover()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_transfer_file(path.clone(), cx)
                                    }))
                            })
                            .when(block.is_some(), |el| el.opacity(0.45))
                            .child(checkbox(&theme, checked))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono.clone())
                                    .text_size(px(12.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(file.path.clone())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(match block {
                                        Some(block) => block.label().to_string(),
                                        None => format_bytes(file.size),
                                    })),
                            );
                        row.into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("transfer-file-list")
                            .max_h(px(176.0))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .children(rows),
                    )
                    .when(listing.truncated, |el| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(
                                    "Only the first 200 ignored files are listed.",
                                )),
                        )
                    })
                    .into_any_element()
            }
        };
        let selected_hint = match &model.listing {
            Listing::Ready(listing) if !listing.files.is_empty() => format!(
                "{} selected · {}",
                model.selection.selected_count(),
                format_bytes(model.selection.selected_bytes())
            ),
            _ => String::new(),
        };

        let progress =
            model.phase.label(&model.target_name).map(|label| {
                let percent = match &model.phase {
                    TransferPhase::Cloning {
                        percent: Some(percent),
                        ..
                    } => Some(*percent),
                    _ => None,
                };
                div()
                    .mt(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .h(px(3.0))
                            .w_full()
                            .rounded_full()
                            .bg(theme.wash(0.08))
                            .child(div().h_full().rounded_full().bg(theme.accent).w(
                                gpui::relative(
                                    percent.map(|p| f32::from(p) / 100.0).unwrap_or(0.15),
                                ),
                            )),
                    )
            });
        let error = match &model.phase {
            TransferPhase::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let hint = (!busy && error.is_none())
            .then(|| blocked.clone())
            .flatten()
            .filter(|_| !matches!(model.listing, Listing::Loading));
        let can_submit = blocked.is_none();
        let cancel_label = if matches!(
            model.phase,
            TransferPhase::Cloning { .. } | TransferPhase::Reading { .. }
        ) {
            "Stop"
        } else {
            "Cancel"
        };

        let card = popover::dialog_card(&theme)
            .w(px(520.0))
            .id("add-to-device-card")
            .role(gpui::Role::Dialog)
            .aria_label("Add project to another device")
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close_add_to_device(cx);
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(
                &theme,
                &format!("Add to {}", model.target_name),
            ))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                format!(
                    "Clone {} on {} and bring along the files git doesn't track.",
                    model.project_name, model.target_name
                ),
            )))
            .child(route)
            .child(transfer_label(
                &theme,
                &format!("Folder on {}", model.target_name),
                "Missing or empty",
            ))
            .child(
                popover::dialog_field(destination_input.into_any_element())
                    .font_family(mono.clone())
                    .when(busy, |el| el.opacity(0.6)),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .mb(px(5.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        transfer_label(&theme, "Clone URL", "")
                            .mt(px(0.0))
                            .mb(px(0.0)),
                    )
                    .children(protocol_toggle),
            )
            .child(
                div()
                    .truncate()
                    .font_family(mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(url_line),
            )
            .child(transfer_label(&theme, "Files to copy", &selected_hint))
            .child(files_body)
            .children(progress)
            .when_some(error, |card, message| {
                card.child(
                    div()
                        .mt(px(12.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .whitespace_normal()
                        .child(SharedString::from(message)),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .children(hint.map(SharedString::from)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, cancel_label, "add-to-device-cancel")
                                    .id("add-to-device-cancel")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.close_add_to_device(cx)),
                                    ),
                            )
                            .child(
                                popover::btn_primary(&theme, model.submit_label())
                                    .id("add-to-device-submit")
                                    .when(!can_submit, |el| el.opacity(0.5).cursor_default())
                                    .when(can_submit, |el| {
                                        el.on_click(cx.listener(|this, _, _, cx| {
                                            this.submit_add_to_device(cx)
                                        }))
                                    }),
                            ),
                    ),
            );
        Some(popover::modal(
            "add-to-device-dialog",
            viewport,
            card.into_any_element(),
        ))
    }
}

/// One end of the source → target route: device name over a muted path.
fn route_end(theme: &Theme, device: &str, path: &str, mono: SharedString) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .truncate()
                .text_size(px(13.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(device.to_string())),
        )
        .child(
            div()
                .truncate()
                .font_family(mono)
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(path.to_string())),
        )
}

fn transfer_label(theme: &Theme, label: &str, hint: &str) -> gpui::Div {
    div()
        .mt(px(14.0))
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

/// A 14px square check box in theme tokens.
fn checkbox(theme: &Theme, checked: bool) -> gpui::Div {
    div()
        .size(px(14.0))
        .flex_none()
        .rounded(px(4.0))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .map(|el| {
            if checked {
                el.bg(theme.accent)
                    .border_color(theme.accent)
                    .child(icon(icons::CHECK).size(px(10.0)).text_color(theme.on_solid))
            } else {
                el.border_color(theme.border_strong)
            }
        })
}
