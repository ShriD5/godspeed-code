//! Conductor on the shell ([`crate::conductor`] holds the view models): the
//! sidebar entries, the creation sheet, and the Answer pop-up that settles a
//! card's waiting request without leaving the main chat. The cards themselves
//! are transcript rows.
use super::*;

use zeron_proto::attention::AttentionItem;
use zeron_engine::registry::HarnessDescriptor;
use zeron_proto::{AccessMode, ChatConfig, Conductor, CreateConductorParams, HarnessId};

use crate::conductor;

/// How long a freshly created conductor's main chat gets to reach this
/// window's chat list before it is opened anyway.
const OPEN_WAIT: Duration = Duration::from_secs(8);
const OPEN_POLL: Duration = Duration::from_millis(100);

pub(super) struct ConductorSheet {
    folder: Option<String>,
    name: Entity<ComposerInput>,
    purpose: Entity<ComposerInput>,
    /// Agents this device can run; empty until the catalog loads.
    harnesses: Vec<HarnessDescriptor>,
    harness: Option<HarnessId>,
    access: AccessMode,
    error: Option<String>,
    busy: bool,
    focus_pending: bool,
    harness_task: Option<Task<()>>,
    pick_task: Option<Task<()>>,
    task: Option<Task<()>>,
    _events: Vec<Subscription>,
}

pub(super) struct AnswerPopup {
    item_id: String,
    error: Option<String>,
    busy: bool,
    task: Option<Task<()>>,
}

impl Shell {
    // ── sidebar ─────────────────────────────────────────────────────────────

    /// A "Conductors" header with one row per live conductor and a `+` to make
    /// another. Rows open the conductor's main chat.
    pub(super) fn render_conductor_entries(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (entries, selected) = {
            let state = self.state.read(cx);
            (
                conductor::sidebar_entries(&state.conductors, &state.attention.items),
                state.selected_chat.clone(),
            )
        };
        let header = div()
            .id("sidebar-conductors-header")
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .h(px(26.0))
            .px(px(Theme::SPACE_SM))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(SharedString::from(zeron_proto::conductor::DISPLAY_NAME)),
            )
            .child(
                div()
                    .id("sidebar-conductors-new")
                    .role(gpui::Role::Button)
                    .aria_label("New conductor")
                    .size(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.glass_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.open_conductor_sheet(cx)))
                    .child(
                        icon(icons::PEN_NEW_SQUARE)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    ),
            );
        let rows = entries.into_iter().map(|entry| {
            let active = selected.as_deref() == Some(entry.main_chat_id.as_str());
            let chat_id = entry.main_chat_id.clone();
            div()
                .id(SharedString::from(format!("conductor-{}", entry.id)))
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(entry.name.clone()))
                .tab_index(0)
                .flex_none()
                .h(px(29.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM))
                .rounded(px(8.0))
                .px(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(if active {
                    theme.text
                } else {
                    theme.text.opacity(0.8)
                })
                .bg(if active {
                    theme.glass_hover()
                } else {
                    theme.glass_hover().opacity(0.0)
                })
                .hover(|s| s.bg(theme.glass_hover()))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_chat(chat_id.clone(), cx);
                }))
                .child(
                    icon(icons::WIDGET)
                        .size(px(15.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(entry.name.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(conductor::repo_summary(
                            entry.repo_count,
                        ))),
                )
                .when(entry.needs_you, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .size(px(7.0))
                            .rounded_full()
                            .bg(theme.warning),
                    )
                })
        });
        div()
            .id("sidebar-conductors")
            .flex_none()
            .flex()
            .flex_col()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .child(header)
            .children(rows)
            .into_any_element()
    }

    // ── creation sheet ──────────────────────────────────────────────────────

    pub(super) fn open_conductor_sheet(&mut self, cx: &mut Context<Self>) {
        self.command_palette = None;
        let name = cx.new(|cx| ComposerInput::new("Name (defaults to the folder)", cx));
        let purpose = cx.new(|cx| ComposerInput::new("What is it for? (optional)", cx));
        let events = [&name, &purpose]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Shell, _, event, cx| {
                    if matches!(event, ComposerInputEvent::Submitted) {
                        this.submit_conductor_sheet(cx);
                    } else {
                        cx.notify();
                    }
                })
            })
            .collect();
        self.conductor_sheet = Some(ConductorSheet {
            folder: None,
            name,
            purpose,
            harnesses: Vec::new(),
            harness: None,
            access: crate::settings::default_access_mode(cx),
            error: None,
            busy: false,
            focus_pending: true,
            harness_task: None,
            pick_task: None,
            task: None,
            _events: events,
        });
        self.load_conductor_harnesses(cx);
        cx.notify();
    }

    /// The sheet picks its own agent: the composer follows the selected chat,
    /// which may be on an agent this conductor should not inherit.
    fn load_conductor_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<Vec<HarnessDescriptor>>(
                    zeron_rpc::methods::LIST_HARNESSES,
                    serde_json::json!({}),
                )
                .await;
            this.update(cx, |shell, cx| {
                let Some(sheet) = shell.conductor_sheet.as_mut() else {
                    return;
                };
                match result {
                    Ok(list) => {
                        sheet.harnesses = crate::pickers::offered_harnesses(&list);
                        if sheet.harness.is_none() {
                            sheet.harness = sheet.harnesses.first().map(|d| d.id);
                        }
                    }
                    Err(error) => sheet.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(sheet) = self.conductor_sheet.as_mut() {
            sheet.harness_task = Some(task);
        }
    }

    fn close_conductor_sheet(&mut self, cx: &mut Context<Self>) {
        if self.conductor_sheet.as_ref().is_some_and(|s| s.busy) {
            return;
        }
        self.conductor_sheet = None;
        cx.notify();
    }

    fn choose_conductor_folder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Folder".into()),
        });
        let Some(sheet) = self.conductor_sheet.as_mut() else {
            return;
        };
        sheet.pick_task = Some(cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |shell, cx| {
                if let Some(sheet) = shell.conductor_sheet.as_mut() {
                    match result {
                        Ok(Ok(Some(paths))) => {
                            if let Some(path) = paths.into_iter().next() {
                                sheet.folder = Some(path.to_string_lossy().into_owned());
                                sheet.error = None;
                            }
                        }
                        Ok(Err(error)) => sheet.error = Some(error.to_string()),
                        Ok(Ok(None)) | Err(_) => {}
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn submit_conductor_sheet(&mut self, cx: &mut Context<Self>) {
        let (engine, local) = {
            let state = self.state.read(cx);
            (state.engine().cloned(), state.local_device_id.clone())
        };
        let Some(sheet) = self.conductor_sheet.as_mut() else {
            return;
        };
        if sheet.busy {
            return;
        }
        let Some(harness) = sheet.harness else {
            sheet.error = Some("No agent is available on this device.".into());
            cx.notify();
            return;
        };
        let config = Some(ChatConfig {
            harness,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: sheet.access.legacy().0,
            access_mode: Some(sheet.access),
        });
        let Some(folder) = sheet.folder.clone() else {
            sheet.error = Some("Choose a folder first.".into());
            cx.notify();
            return;
        };
        let Some(engine) = engine else {
            sheet.error = Some("The engine is not connected.".into());
            cx.notify();
            return;
        };
        let text = |input: &Entity<ComposerInput>| {
            let text = input.read(cx).text().trim().to_string();
            (!text.is_empty()).then_some(text)
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        let params = CreateConductorParams {
            chat_id: chat_id.clone(),
            id: uuid::Uuid::new_v4().to_string(),
            folder,
            name: text(&sheet.name),
            purpose: text(&sheet.purpose),
            device_id: local,
            config,
        };
        sheet.busy = true;
        sheet.error = None;
        sheet.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<Conductor>(
                    zeron_rpc::methods::CREATE_CONDUCTOR,
                    serde_json::to_value(&params).unwrap_or_default(),
                )
                .await
                .map_err(|error| error.to_string());
            match result {
                Ok(conductor) => {
                    // The main chat's row arrives over the chats watch; wait
                    // briefly so opening it does not land on a missing row.
                    let mut waited = Duration::ZERO;
                    while waited < OPEN_WAIT {
                        let known = this
                            .update(cx, |shell, cx| {
                                shell
                                    .state
                                    .read(cx)
                                    .chats
                                    .iter()
                                    .any(|c| c.id == conductor.main_chat_id)
                            })
                            .unwrap_or(true);
                        if known {
                            break;
                        }
                        cx.background_executor().timer(OPEN_POLL).await;
                        waited += OPEN_POLL;
                    }
                    this.update(cx, |shell, cx| {
                        shell.conductor_sheet = None;
                        shell.open_chat(conductor.main_chat_id.clone(), cx);
                    })
                    .ok();
                }
                Err(error) => {
                    this.update(cx, |shell, cx| {
                        if let Some(sheet) = shell.conductor_sheet.as_mut() {
                            sheet.busy = false;
                            sheet.error = Some(error);
                        }
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
        cx.notify();
    }

    // ── Answer pop-up ───────────────────────────────────────────────────────

    pub(super) fn open_answer_popup(&mut self, item_id: String, cx: &mut Context<Self>) {
        self.answer_popup = Some(AnswerPopup {
            item_id,
            error: None,
            busy: false,
            task: None,
        });
        cx.notify();
    }

    fn close_answer_popup(&mut self, cx: &mut Context<Self>) {
        self.answer_popup = None;
        cx.notify();
    }

    fn answer_popup_item(&self, cx: &App) -> Option<AttentionItem> {
        let id = &self.answer_popup.as_ref()?.item_id;
        self.state
            .read(cx)
            .attention
            .items
            .iter()
            .find(|item| &item.id == id)
            .cloned()
    }

    /// Approve or decline the popup's request with the decision Enter or Esc
    /// would pick on the approval card.
    fn answer_conductor_item(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Some(item) = self.answer_popup_item(cx) else {
            return;
        };
        let Some(approval) = &item.approval else {
            return;
        };
        let Some(decision) = crate::composer::approval_default_index(approval, approve)
            .and_then(|ix| approval.decisions.get(ix))
            .map(|option| option.decision)
        else {
            return;
        };
        let (engine, device) = {
            let state = self.state.read(cx);
            (state.engine().cloned(), state.attention.device_id.clone())
        };
        let Some(engine) = engine else {
            return;
        };
        let Some(popup) = self.answer_popup.as_mut() else {
            return;
        };
        if popup.busy {
            return;
        }
        popup.busy = true;
        popup.error = None;
        let mut params = serde_json::json!({ "itemId": item.id, "decision": decision });
        if !device.is_empty() {
            params["targetDeviceId"] = serde_json::json!(device);
        }
        popup.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(zeron_rpc::methods::RESPOND_ATTENTION, params)
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => shell.answer_popup = None,
                    Err(error) => {
                        if let Some(popup) = shell.answer_popup.as_mut() {
                            popup.busy = false;
                            popup.error = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ── overlays ────────────────────────────────────────────────────────────

    pub(super) fn render_conductor_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let mut overlays: Vec<AnyElement> = Vec::new();
        if let Some(overlay) = self.render_conductor_sheet(viewport, &theme, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_answer_popup(viewport, &theme, cx) {
            overlays.push(overlay);
        }
        overlays
    }

    fn render_conductor_sheet(
        &mut self,
        viewport: gpui::Size<Pixels>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let sheet = self.conductor_sheet.as_mut()?;
        if std::mem::take(&mut sheet.focus_pending) {
            window.focus(&sheet.name.focus_handle(cx), cx);
        }
        let name = sheet.name.clone();
        let purpose = sheet.purpose.clone();
        let busy = sheet.busy;
        let ready = sheet.folder.is_some() && !busy;
        let folder_label = sheet
            .folder
            .clone()
            .unwrap_or_else(|| "No folder chosen".into());
        let has_folder = sheet.folder.is_some();
        let error = sheet.error.clone();
        let harnesses: Vec<(HarnessId, bool)> = sheet
            .harnesses
            .iter()
            .map(|d| (d.id, sheet.harness == Some(d.id)))
            .collect();
        let access = sheet.access;
        let card = popover::dialog_card(theme)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close_conductor_sheet(cx);
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(
                theme,
                &format!("New {}", zeron_proto::conductor::DISPLAY_NAME.to_lowercase()),
            ))
            .child(
                div().mt(px(8.0)).child(popover::dialog_body(
                    theme,
                    "One long-lived chat for a folder. It starts and steers sub-agents in the repos inside.",
                )),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(if has_folder { theme.text } else { theme.text_faint })
                            .child(SharedString::from(folder_label)),
                    )
                    .child(
                        popover::btn_ghost(theme, "Choose folder…", "conductor-folder")
                            .id("conductor-folder")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_conductor_folder(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .child(popover::dialog_field(name.into_any_element())),
            )
            .child(
                div()
                    .mt(px(8.0))
                    .child(popover::dialog_field(purpose.into_any_element())),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child("Main agent"),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(6.0))
                    .when(harnesses.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_faint)
                                .child("Loading agents…"),
                        )
                    })
                    .children(harnesses.into_iter().map(|(id, selected)| {
                        let label = super::harness_updates::agent_name(id);
                        let key = format!("conductor-agent-{label}");
                        let chip = if selected {
                            popover::btn_primary(theme, label)
                        } else {
                            popover::btn_ghost(theme, label, key.clone())
                        };
                        chip.id(SharedString::from(key)).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if let Some(sheet) = this.conductor_sheet.as_mut() {
                                    sheet.harness = Some(id);
                                    if !zeron_proto::access_mode_supported(id, sheet.access) {
                                        sheet.access = AccessMode::default();
                                    }
                                }
                                cx.notify();
                            },
                        ))
                    })),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(6.0))
                    .children(AccessMode::ALL.into_iter().map(|mode| {
                        let key = format!("conductor-access-{}", mode.label());
                        let chip = if mode == access {
                            popover::btn_primary(theme, mode.label())
                        } else {
                            popover::btn_ghost(theme, mode.label(), key.clone())
                        };
                        chip.id(SharedString::from(key)).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if let Some(sheet) = this.conductor_sheet.as_mut() {
                                    sheet.access = mode;
                                }
                                cx.notify();
                            },
                        ))
                    })),
            )
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .mt(px(10.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "conductor-cancel")
                            .id("conductor-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.close_conductor_sheet(cx))),
                    )
                    .child(
                        popover::btn_primary(theme, if busy { "Creating…" } else { "Create" })
                            .id("conductor-create")
                            .opacity(if ready { 1.0 } else { 0.5 })
                            .on_click(cx.listener(|this, _, _, cx| this.submit_conductor_sheet(cx))),
                    ),
            );
        Some(popover::modal(
            "conductor-sheet",
            viewport,
            card.into_any_element(),
        ))
    }

    fn render_answer_popup(
        &mut self,
        viewport: gpui::Size<Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.answer_popup.as_ref()?;
        let Some(item) = self.answer_popup_item(cx) else {
            // Settled elsewhere while the pop-up was open.
            self.answer_popup = None;
            return None;
        };
        let popup = self.answer_popup.as_ref()?;
        let (busy, error) = (popup.busy, popup.error.clone());
        let chat_id = item.chat_id.clone();
        let is_approval = item.approval.is_some();
        let details: Vec<AnyElement> = item
            .approval
            .as_ref()
            .map(|approval| {
                crate::approval_view::approval_details(approval, theme, "conductor-answer")
            })
            .unwrap_or_default();
        let card = popover::dialog_card(theme)
            .w(px(460.0))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close_answer_popup(cx);
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(theme, &item.title))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                theme,
                if is_approval {
                    item.summary.clone()
                } else {
                    format!("{} Open the thread to answer it.", item.summary)
                },
            )))
            .children(details)
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .mt(px(10.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Close", "conductor-answer-close")
                            .id("conductor-answer-close")
                            .on_click(cx.listener(|this, _, _, cx| this.close_answer_popup(cx))),
                    )
                    .when_some(chat_id, |el, chat_id| {
                        el.child(
                            popover::btn_ghost(theme, "Open thread", "conductor-answer-open")
                                .id("conductor-answer-open")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.answer_popup = None;
                                    this.open_chat(chat_id.clone(), cx);
                                })),
                        )
                    })
                    .when(is_approval, |el| {
                        el.child(
                            popover::btn_ghost(theme, "Decline", "conductor-answer-decline")
                                .id("conductor-answer-decline")
                                .opacity(if busy { 0.5 } else { 1.0 })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.answer_conductor_item(false, cx);
                                })),
                        )
                        .child(
                            popover::btn_primary(theme, "Approve")
                                .id("conductor-answer-approve")
                                .opacity(if busy { 0.5 } else { 1.0 })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.answer_conductor_item(true, cx);
                                })),
                        )
                    }),
            );
        Some(popover::modal(
            "conductor-answer",
            viewport,
            card.into_any_element(),
        ))
    }
}
