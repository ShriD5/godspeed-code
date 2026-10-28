//! The composer's Zoomie mode on the new-chat canvas: a ⚡ chip beside the
//! model picker turns the prompt into a zoomie's instructions, and a When
//! row under the pill picks (or reads, in plain English) when it runs. Send
//! creates the zoomie on this device's engine and runs it once right away.

use super::*;

use chrono::Utc;
use gpui::AnyElement;
use zeron_proto::agents::{AgentRun, AgentSummary};

use crate::zoomies::schedule::{EMPTY_HINT, ParsedSchedule, Preset};
use crate::zoomies::{self, ComposerZoomie, ProjectRef, ZoomiePrefill};

/// The Zoomie mode's own state; `None` on [`Composer`] while it is off.
pub(crate) struct ZoomieMode {
    /// The When field (single line, plain English).
    pub(crate) when: Entity<ComposerInput>,
    pub(crate) parsed: Result<ParsedSchedule, String>,
    timezone: String,
    /// Create + first run in flight.
    pub(crate) creating: bool,
    /// Why the last Send was refused (no project, create failed, …).
    pub(crate) error: Option<SharedString>,
    when_focus_pending: bool,
    _when_events: Subscription,
}

/// What the shell hears after a zoomie was made from the composer.
#[derive(Debug, Clone)]
pub struct ZoomieCreated {
    pub agent: AgentSummary,
    /// The first run's chat (it starts immediately).
    pub run_chat_id: Option<String>,
    /// "every weekday at 18:00" — the toast reads "runs …".
    pub phrase: String,
}

impl Composer {
    /// Zoomie mode is on and the composer is on the new-chat canvas.
    pub(crate) fn zoomie_active(&self, cx: &App) -> bool {
        self.zoomie.is_some() && !self.side_chat && self.state.read(cx).selected_chat.is_none()
    }

    pub(crate) fn toggle_zoomie_mode(&mut self, cx: &mut Context<Self>) {
        let on = self.zoomie.is_none();
        self.set_zoomie_mode(on, cx);
    }

    pub(crate) fn set_zoomie_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.zoomie.is_some() {
            return;
        }
        if !on {
            self.zoomie = None;
            cx.notify();
            return;
        }
        let when = cx.new(|cx| {
            ComposerInput::new("or type it: every 30 min, mondays 10am, daily 21:00", cx)
                .with_single_line()
        });
        let events = cx.subscribe(&when, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => this.reparse_zoomie(cx),
            ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted => {
                this.submit_zoomie(cx)
            }
            _ => {}
        });
        self.zoomie = Some(ZoomieMode {
            when,
            parsed: Err(EMPTY_HINT.into()),
            timezone: zoomies::local_timezone(),
            creating: false,
            error: None,
            when_focus_pending: false,
            _when_events: events,
        });
        self.focus_pending = true;
        cx.notify();
    }

    /// Turn Zoomie mode on with `prefill` ("Run this on a schedule…"): the
    /// prompt goes into the editor, the run config into the pickers. The
    /// caller has already put the canvas on the chat's project.
    pub(crate) fn enter_zoomie_mode(
        &mut self,
        prefill: Option<ZoomiePrefill>,
        cx: &mut Context<Self>,
    ) {
        self.set_zoomie_mode(true, cx);
        if let Some(prefill) = prefill {
            if !prefill.prompt.is_empty() {
                let prompt = prefill.prompt.clone();
                self.input
                    .update(cx, |input, cx| input.set_text(prompt, cx));
            }
            self.pickers.update(cx, |pickers, cx| {
                pickers.prefill_draft(
                    prefill.harness,
                    prefill.model.clone(),
                    prefill.reasoning,
                    prefill.access_mode,
                    cx,
                )
            });
            // The prompt is already written: the next thing to say is when.
            if let Some(mode) = self.zoomie.as_mut() {
                mode.when_focus_pending = true;
            }
        }
        cx.notify();
    }

    pub(crate) fn pick_zoomie_preset(&mut self, preset: Preset, cx: &mut Context<Self>) {
        let Some(mode) = self.zoomie.as_mut() else {
            return;
        };
        let when = mode.when.clone();
        // "Once at…" leaves the time to type; the field takes focus.
        mode.when_focus_pending = preset == Preset::Once;
        when.update(cx, |input, cx| input.set_text(preset.text(), cx));
        self.reparse_zoomie(cx);
    }

    fn reparse_zoomie(&mut self, cx: &mut Context<Self>) {
        let Some(mode) = self.zoomie.as_mut() else {
            return;
        };
        let text = mode.when.read(cx).text().to_string();
        mode.parsed = zoomies::parse_schedule(&text, &mode.timezone, Utc::now());
        mode.error = None;
        cx.notify();
    }

    /// Send is inert until there is a prompt and a schedule it can read.
    pub(crate) fn zoomie_send_blocked(&self, cx: &App) -> bool {
        let Some(mode) = &self.zoomie else {
            return false;
        };
        mode.creating || mode.parsed.is_err() || self.input.read(cx).text().trim().is_empty()
    }

    /// The draft Send would save, or why not.
    pub(crate) fn zoomie_draft(&self, cx: &App) -> Result<zeron_proto::agents::AgentDraft, String> {
        let Some(mode) = &self.zoomie else {
            return Err("Zoomie mode is off.".into());
        };
        let state = self.state.read(cx);
        let space = state.selected_space_row();
        let resolved = self.pickers.read(cx).resolved(cx);
        let prompt = self.input.read(cx).text().to_string();
        zoomies::assemble_draft(&ComposerZoomie {
            prompt: &prompt,
            name: None,
            schedule: mode.parsed.as_ref().map_err(String::as_str),
            project: space.map(|space| ProjectRef {
                space_id: &space.id,
                device_id: &space.device_id,
            }),
            local_device_id: state.local_device_id.as_deref(),
            harness: resolved.harness,
            model: resolved.model.clone(),
            reasoning: resolved.reasoning,
            access_mode: resolved.access_mode,
        })
    }

    /// Create the zoomie, run it once, and hand the result to the shell.
    pub(crate) fn submit_zoomie(&mut self, cx: &mut Context<Self>) {
        if self.zoomie.as_ref().is_none_or(|mode| mode.creating) {
            return;
        }
        let draft = match self.zoomie_draft(cx) {
            Ok(draft) => draft,
            Err(reason) => {
                if let Some(mode) = self.zoomie.as_mut() {
                    mode.error = Some(reason.into());
                }
                cx.notify();
                return;
            }
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(mode) = self.zoomie.as_mut() {
                mode.error = Some("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        let phrase = self
            .zoomie
            .as_ref()
            .and_then(|mode| mode.parsed.as_ref().ok())
            .map(|parsed| parsed.phrase.clone())
            .unwrap_or_default();
        if let Some(mode) = self.zoomie.as_mut() {
            mode.creating = true;
            mode.error = None;
        }
        cx.notify();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let client = engine.client();
            let created = client
                .call(methods::CREATE_AGENT, serde_json::json!({ "draft": draft }))
                .await
                .map_err(|err| err.to_string())
                .and_then(|value| {
                    serde_json::from_value::<AgentSummary>(value).map_err(|err| err.to_string())
                });
            let created = match created {
                Ok(agent) => agent,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        if let Some(mode) = this.zoomie.as_mut() {
                            mode.creating = false;
                            mode.error = Some(format!("Couldn't create the zoomie: {err}").into());
                        }
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            // Run once now so the user sees it work.
            let run = client
                .call(
                    methods::RUN_AGENT_NOW,
                    serde_json::json!({ "id": created.id }),
                )
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<AgentRun>(value).ok());
            this.update(cx, |this, cx| {
                this.finish_zoomie_create(
                    ZoomieCreated {
                        agent: created,
                        run_chat_id: run.and_then(|run| run.chat_id),
                        phrase,
                    },
                    cx,
                );
            })
            .ok();
        }));
    }

    fn finish_zoomie_create(&mut self, created: ZoomieCreated, cx: &mut Context<Self>) {
        self.zoomie = None;
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.drafts.remove(&self.current_key);
        cx.emit(ComposerEvent::ZoomieCreated(created));
        cx.notify();
    }

    /// The ⚡ chip beside the model picker (new-chat canvas only).
    pub(crate) fn render_zoomie_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.side_chat || self.state.read(cx).selected_chat.is_some() {
            return None;
        }
        let on = self.zoomie.is_some();
        let hover_key: SharedString = format!("composer-zoomie-{}", cx.entity_id()).into();
        let label = if on { "Zoomie mode: on" } else { "Zoomie mode" };
        Some(
            div()
                .id("composer-zoomie-chip")
                .debug_selector(|| "composer-zoomie-chip".into())
                .role(Role::Switch)
                .aria_label(label)
                .tab_index(0)
                .h(px(28.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(if on { 8.0 } else { 6.0 }))
                .rounded_full()
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .when(on, |el| el.bg(theme.accent_wash).text_color(theme.accent))
                .when(!on, |el| {
                    el.text_color(theme.text_muted).bg(motion::hover_blend(
                        &hover_key,
                        gpui::transparent_black(),
                        crate::theme::ink(0.10),
                    ))
                })
                .on_hover(motion::hover_listener(hover_key))
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .tooltip(crate::settings::widgets::text_tooltip(if on {
                    "Back to a normal chat"
                } else {
                    "Zoomie: run this prompt on a schedule"
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_zoomie_mode(cx);
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.toggle_zoomie_mode(cx);
                    }
                }))
                .child(
                    crate::icons::icon(if on {
                        crate::icons::FAST_TIER_BOLD
                    } else {
                        crate::icons::FAST_TIER
                    })
                    .size(px(14.0))
                    .text_color(if on {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
                )
                .when(on, |el| el.child(SharedString::from("Zoomie")))
                .into_any_element(),
        )
    }

    /// The When row under the pill: presets, the plain-English field, and
    /// the live reading of it.
    pub(crate) fn render_zoomie_panel(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.zoomie_active(cx) {
            return None;
        }
        let draft_issue = self.zoomie_draft(cx).err();
        let prompt_empty = self.input.read(cx).text().trim().is_empty();
        let mode = self.zoomie.as_mut()?;
        if std::mem::take(&mut mode.when_focus_pending) {
            let focus = mode.when.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        }
        let when_text = mode.when.read(cx).text().to_string();
        let selected = Preset::matching(&when_text);
        let now = Utc::now();
        let creating = mode.creating;

        let mut presets = div().flex().flex_row().flex_wrap().gap(px(6.0));
        for (ix, preset) in Preset::ALL.into_iter().enumerate() {
            let active = selected == Some(preset)
                || (preset == Preset::Once && when_text.trim_start().starts_with("once"));
            presets = presets.child(
                div()
                    .id(("zoomie-preset", ix))
                    .debug_selector(move || format!("zoomie-preset-{ix}"))
                    .role(Role::Button)
                    .aria_label(preset.label())
                    .h(px(24.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .border_1()
                    .text_size(crate::typography::ui_rems(12.0))
                    .cursor_pointer()
                    .when(active, |el| {
                        el.border_color(theme.accent.opacity(0.35))
                            .bg(theme.accent_wash)
                            .text_color(theme.accent)
                    })
                    .when(!active, |el| {
                        el.border_color(theme.border)
                            .text_color(theme.text_muted)
                            .hover(|style| style.bg(theme.element_hover).text_color(theme.text))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.pick_zoomie_preset(preset, cx);
                    }))
                    .child(SharedString::from(preset.label())),
            );
        }

        // One status line: what it read, why it can't, or a nudge.
        let (status_icon, status_color, status): (&'static str, gpui::Hsla, SharedString) =
            if creating {
                (
                    crate::icons::FAST_TIER_BOLD,
                    theme.accent,
                    "Creating your zoomie…".into(),
                )
            } else if let Some(error) = mode.error.clone() {
                (crate::icons::DANGER_TRIANGLE, theme.danger, error)
            } else {
                match &mode.parsed {
                    Ok(parsed) => (
                        crate::icons::CLOCK_CIRCLE,
                        theme.text_muted,
                        parsed.summary(now).into(),
                    ),
                    Err(_) if when_text.trim().is_empty() => (
                        crate::icons::CLOCK_CIRCLE,
                        theme.text_faint,
                        "Pick one, or say it in plain English.".into(),
                    ),
                    Err(hint) => (
                        crate::icons::DANGER_TRIANGLE,
                        theme.warning,
                        hint.clone().into(),
                    ),
                }
            };
        // A blocking issue the schedule line doesn't already cover.
        let issue = (!creating && mode.error.is_none() && mode.parsed.is_ok() && !prompt_empty)
            .then_some(draft_issue)
            .flatten();

        let field = div()
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(8.0))
            .border_1()
            .border_color(match &mode.parsed {
                Err(_) if !when_text.trim().is_empty() => theme.warning.opacity(0.5),
                _ => theme.border,
            })
            .bg(crate::theme::ink(0.03))
            .child(div().flex_1().min_w_0().child(mode.when.clone()));

        let status_row = |icon_path: &'static str, color: gpui::Hsla, text: SharedString| {
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(6.0))
                .text_size(crate::typography::ui_rems(12.0))
                .line_height(px(16.0))
                .text_color(color)
                .child(
                    crate::icons::icon(icon_path)
                        .flex_none()
                        .mt(px(1.0))
                        .size(px(13.0))
                        .text_color(color),
                )
                .child(div().flex_1().min_w_0().child(text))
        };

        Some(
            div()
                .id("zoomie-when")
                .debug_selector(|| "zoomie-when".into())
                .mx(px(4.0))
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.composer_surface_border())
                .bg(theme.composer_surface_bg())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(
                            crate::icons::icon(crate::icons::FAST_TIER_BOLD)
                                .size(px(13.0))
                                .text_color(theme.accent),
                        )
                        .child(SharedString::from("When")),
                )
                .child(presets)
                .child(field)
                .child(status_row(status_icon, status_color, status))
                .children(issue.map(|issue| {
                    status_row(crate::icons::INFO_CIRCLE, theme.text_muted, issue.into())
                }))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AccessMode;
    use zeron_proto::agents::AgentTrigger;

    fn space(id: &str, device: &str) -> zeron_proto::Space {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": device, "path": format!("/{id}"),
            "createdAt": "2026-10-01T00:00:00Z",
        }))
        .unwrap()
    }

    #[gpui::test]
    fn draft_comes_from_composer_state(cx: &mut gpui::TestAppContext) {
        let (_dir, window) = super::super::tests::composer_focus_window(cx);
        window
            .update(cx, |composer, _, cx| {
                composer.state.update(cx, |state, cx| {
                    state.local_device_id = Some("this-mac".into());
                    state.spaces = vec![space("s1", "this-mac"), space("s2", "vps")];
                    state.select_space(Some("s1".into()), cx);
                });
                // Off by default; the chip turns it on, only on the canvas.
                assert!(!composer.zoomie_active(cx));
                composer.toggle_zoomie_mode(cx);
                assert!(composer.zoomie_active(cx));

                composer.input.update(cx, |input, cx| {
                    input.set_text("Summarize merged PRs and post to #eng", cx)
                });
                // No schedule yet: Send is inert.
                assert!(composer.zoomie_send_blocked(cx));
                composer.pick_zoomie_preset(Preset::WeekdayEvenings, cx);
                composer.pickers.update(cx, |pickers, cx| {
                    pickers.prefill_draft(
                        Some(HarnessId::Codex),
                        Some("gpt-5.4".into()),
                        None,
                        Some(AccessMode::Supervised),
                        cx,
                    )
                });
                assert!(!composer.zoomie_send_blocked(cx));
                let draft = composer.zoomie_draft(cx).unwrap();
                assert_eq!(draft.name, "Summarize merged PRs");
                assert_eq!(draft.instructions, "Summarize merged PRs and post to #eng");
                assert_eq!(draft.space_id, "s1");
                assert_eq!(draft.harness, HarnessId::Codex);
                assert_eq!(draft.model.as_deref(), Some("gpt-5.4"));
                assert_eq!(draft.access_mode, Some(AccessMode::Supervised));
                assert_eq!(
                    draft.triggers,
                    vec![AgentTrigger::Cron {
                        expr: "0 18 * * 1-5".into(),
                        timezone: crate::zoomies::local_timezone(),
                    }]
                );

                // Free text that doesn't parse blocks Send with a hint.
                let when = composer.zoomie.as_ref().unwrap().when.clone();
                when.update(cx, |input, cx| input.set_text("every blue moon", cx));
                composer.reparse_zoomie(cx);
                assert!(composer.zoomie_send_blocked(cx));
                assert!(
                    composer
                        .zoomie_draft(cx)
                        .unwrap_err()
                        .contains("Couldn't read")
                );
                when.update(cx, |input, cx| input.set_text("every 30 min", cx));
                composer.reparse_zoomie(cx);
                assert_eq!(
                    composer.zoomie_draft(cx).unwrap().triggers,
                    vec![AgentTrigger::Interval { every_secs: 1800 }]
                );

                // A project on another device can't host it.
                composer
                    .state
                    .update(cx, |state, cx| state.select_space(Some("s2".into()), cx));
                assert!(
                    composer
                        .zoomie_draft(cx)
                        .unwrap_err()
                        .contains("this device")
                );
                // Send surfaces that reason inline instead of sending.
                composer.submit_zoomie(cx);
                let mode = composer.zoomie.as_ref().unwrap();
                assert!(!mode.creating);
                assert!(mode.error.as_ref().unwrap().contains("this device"));

                composer.toggle_zoomie_mode(cx);
                assert!(!composer.zoomie_active(cx));
            })
            .unwrap();
    }

    #[gpui::test]
    fn once_preset_waits_for_a_time(cx: &mut gpui::TestAppContext) {
        let (_dir, window) = super::super::tests::composer_focus_window(cx);
        window
            .update(cx, |composer, _, cx| {
                composer.set_zoomie_mode(true, cx);
                composer.pick_zoomie_preset(Preset::Once, cx);
                let mode = composer.zoomie.as_ref().unwrap();
                assert_eq!(mode.when.read(cx).text(), "once at ");
                assert!(mode.parsed.is_err());
                assert!(mode.when_focus_pending);
            })
            .unwrap();
    }
}
