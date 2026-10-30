//! Zoomies in the main window: the sidebar section (speedster rows with
//! next run, last result and a switch), the composer hand-offs (+, "New
//! zoomie…", "Run this on a schedule…") and the toast after one is made.

use super::*;
use crate::composer::ZoomieCreated;
use crate::zoomies::mascot::{self, Pose};
use crate::zoomies::rows::{RowStatus, ZoomieRow};
use spaces::SIDEBAR_DISCLOSURE_BODY_INSET;

const KEY: &str = "zoomies";
const ROW_HEIGHT: f32 = 29.0;
const EMPTY_HEIGHT: f32 = 58.0;
/// The toast stays long enough to reach Undo.
const CREATED_TOAST: Duration = Duration::from_secs(8);

/// The dot's colour for a row's last result.
fn status_color(status: RowStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        RowStatus::Fresh => theme.text_faint.opacity(0.6),
        RowStatus::Running => theme.accent,
        RowStatus::NeedsYou => theme.warning,
        RowStatus::Succeeded => theme.success,
        RowStatus::Failed | RowStatus::Paused => theme.danger,
        RowStatus::Held => theme.limited,
    }
}

/// Height of the section body when open.
pub(super) fn zoomies_body_height(rows: usize) -> f32 {
    SIDEBAR_DISCLOSURE_BODY_INSET
        + if rows == 0 {
            EMPTY_HEIGHT
        } else {
            rows as f32 * ROW_HEIGHT + rows.saturating_sub(1) as f32 * SIDEBAR_LIST_GAP
        }
}

impl Shell {
    /// The Home composer, in Zoomie mode, focused.
    pub(super) fn open_zoomie_composer(&mut self, cx: &mut Context<Self>) {
        self.open_new_session(None, cx);
        self.composer
            .update(cx, |composer, cx| composer.set_zoomie_mode(true, cx));
    }

    /// "Run this on a schedule…": the Home composer in Zoomie mode, on the
    /// chat's project, prefilled with its first prompt and run config.
    pub(super) fn convert_chat_to_zoomie(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let (chat, transcript) = {
            let state = self.state.read(cx);
            let Some(chat) = state.chats.iter().find(|chat| chat.id == chat_id).cloned() else {
                return;
            };
            let transcript = if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                state.transcript.clone()
            } else {
                Vec::new()
            };
            (chat, transcript)
        };
        let target = chat
            .space_id
            .clone()
            .filter(|id| self.state.read(cx).space_row(id).is_some())
            .map(NewSessionTarget::Project);
        let prefill = crate::zoomies::prefill_from_chat(&chat, &transcript);
        let needs_transcript = crate::zoomies::draft::first_user_prompt(&transcript).is_none();
        self.open_new_session(target, cx);
        self.composer.update(cx, |composer, cx| {
            composer.enter_zoomie_mode(Some(prefill), cx)
        });
        if needs_transcript {
            self.fetch_first_prompt_for_zoomie(chat_id, cx);
        }
    }

    /// The chat wasn't open: read its transcript once and put its first
    /// prompt in, unless the user already started typing over the title.
    fn fetch_first_prompt_for_zoomie(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let placeholder = self.composer.read(cx).input.read(cx).text().to_string();
        cx.spawn(async move |this, cx| {
            let timeout = cx.background_executor().timer(Duration::from_secs(10));
            let read = async {
                let mut items = engine
                    .client()
                    .subscribe(
                        methods::WATCH_DOC_MESSAGES,
                        serde_json::json!({ "chatId": chat_id }),
                    )
                    .await
                    .ok()?;
                // The opening frame is the whole transcript.
                let first = items.recv().await?;
                let reset: Vec<zeron_doc::SessionMessageEntry> =
                    serde_json::from_value(first.get("reset")?.clone()).ok()?;
                crate::zoomies::draft::first_user_prompt(&reset)
            };
            let prompt = match futures::future::select(Box::pin(read), timeout).await {
                futures::future::Either::Left((prompt, _)) => prompt,
                futures::future::Either::Right(_) => None,
            };
            let Some(prompt) = prompt else {
                return;
            };
            this.update(cx, |this, cx| {
                this.composer.update(cx, |composer, cx| {
                    let untouched = composer.input.read(cx).text() == placeholder;
                    if composer.zoomie.is_some() && untouched {
                        composer
                            .input
                            .update(cx, |input, cx| input.set_text(prompt, cx));
                    }
                });
            })
            .ok();
        })
        .detach();
    }

    /// Settings → Zoomies with this zoomie open in the editor.
    pub(super) fn open_zoomie_detail(&mut self, id: String, cx: &mut Context<Self>) {
        self.open_settings(SettingsSection::AlwaysOn, cx);
        let page = self.agents_page(cx);
        page.update(cx, |page, cx| page.open_agent(id, cx));
    }

    /// The Settings → Zoomies page, created on first use.
    pub(super) fn agents_page(&mut self, cx: &mut Context<Self>) -> Entity<AgentsPage> {
        if let Some(page) = &self.agents_page {
            return page.clone();
        }
        let state = self.state.clone();
        let page = cx.new(|cx| AgentsPage::new(state, cx));
        self.agents_page_sub = Some(cx.subscribe(
            &page,
            |this: &mut Shell, _, event: &AgentsPageEvent, cx| match event {
                AgentsPageEvent::OpenChat(chat_id) => {
                    this.open_chat(chat_id.clone(), cx);
                }
            },
        ));
        self.agents_page = Some(page.clone());
        page
    }

    /// After the composer made a zoomie: show its first run, and a toast
    /// with Open (the zoomie's settings) and Undo (delete it).
    pub(super) fn zoomie_created(&mut self, created: &ZoomieCreated, cx: &mut Context<Self>) {
        if let Some(chat_id) = created.run_chat_id.clone() {
            self.open_chat(chat_id, cx);
        }
        if !self.settings.sidebar_zoomies_open || !self.settings.sidebar_show_zoomies {
            self.settings.sidebar_zoomies_open = true;
            self.settings.sidebar_show_zoomies = true;
            self.schedule_save(cx);
        }
        let shell = cx.entity().downgrade();
        let undo_shell = shell.clone();
        let id = created.agent.id.clone();
        let undo_id = id.clone();
        let message = if created.phrase.is_empty() {
            "Zoomie created".to_string()
        } else {
            format!("Zoomie created · runs {}", created.phrase)
        };
        crate::toast::notice(
            cx,
            message,
            vec![
                crate::toast::ToastAction::new("Open", move |cx| {
                    shell
                        .update(cx, |this, cx| this.open_zoomie_detail(id.clone(), cx))
                        .ok();
                }),
                crate::toast::ToastAction::new("Undo", move |cx| {
                    undo_shell
                        .update(cx, |this, cx| {
                            this.zoomies_feed.update(cx, |feed, cx| {
                                feed.call(
                                    methods::DELETE_AGENT,
                                    serde_json::json!({ "id": undo_id }),
                                    cx,
                                )
                            });
                            crate::toast::info(cx, "Zoomie removed");
                        })
                        .ok();
                }),
            ],
            CREATED_TOAST,
        );
    }

    fn zoomie_rows(&self, cx: &App) -> Option<Vec<ZoomieRow>> {
        let snapshot = self.zoomies_feed.read(cx).snapshot()?;
        Some(crate::zoomies::rows::rows(snapshot, Utc::now()))
    }

    /// The sidebar's Zoomies section: collapsible, shown (with its empty
    /// state) even before the first zoomie so people find it.
    pub(super) fn render_zoomies_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.settings.sidebar_show_zoomies {
            return None;
        }
        self.zoomies_feed
            .update(cx, |feed, cx| feed.ensure_watching(cx));
        // An engine without zoomies (or none yet) shows nothing at all.
        let rows = self.zoomie_rows(cx)?;
        let open = self.settings.sidebar_zoomies_open;
        let body_height = zoomies_body_height(rows.len());
        self.begin_queued_sidebar_reveal(KEY, open, body_height);
        let chevron = self.sidebar_disclosure_chevron(KEY, open, theme);
        let plus = div()
            .id("sidebar-zoomies-new")
            .debug_selector(|| "sidebar-zoomies-new".into())
            .role(gpui::Role::Button)
            .aria_label("New zoomie")
            .size(px(18.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.glass_hover()))
            .tooltip(crate::settings::widgets::text_tooltip("New zoomie"))
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.open_zoomie_composer(cx);
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(12.0))
                    .text_color(spaces::sidebar_header_glyph(theme)),
            )
            .into_any_element();
        let running = rows.iter().filter(|row| row.running()).count();
        let header = spaces::sidebar_disclosure_header(
            theme,
            None,
            "Zoomies".into(),
            (!open && !rows.is_empty()).then_some(rows.len()),
            Some(plus),
            chevron,
        )
        .id("zoomies-toggle")
        .debug_selector(|| "zoomies-toggle".into())
        .when(running > 0 && !open, |el| {
            el.child(div().size(px(6.0)).rounded_full().bg(theme.accent))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            let was_open = this.settings.sidebar_zoomies_open;
            this.begin_sidebar_disclosure_motion(
                KEY,
                if was_open { body_height } else { 0.0 },
                if was_open { 0.0 } else { body_height },
            );
            this.settings.sidebar_zoomies_open = !was_open;
            this.schedule_save(cx);
            cx.notify();
        }));
        let body = if rows.is_empty() {
            self.render_zoomies_empty(theme, cx)
        } else {
            let mut list = div()
                .flex()
                .flex_col()
                .pt(px(SIDEBAR_DISCLOSURE_BODY_INSET))
                .gap(px(SIDEBAR_LIST_GAP));
            for (ix, row) in rows.into_iter().enumerate() {
                list = list.child(self.render_zoomie_row(ix, row, theme, cx));
            }
            list.into_any_element()
        };
        let body = self.render_sidebar_disclosure_body(KEY, open, body_height, body);
        Some(
            div()
                .flex()
                .flex_col()
                .pb(px(8.0))
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }

    fn render_zoomies_empty(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("zoomies-empty")
            .debug_selector(|| "zoomies-empty".into())
            .mt(px(SIDEBAR_DISCLOSURE_BODY_INSET))
            .h(px(EMPTY_HEIGHT))
            .px(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.glass_hover()))
            .on_click(cx.listener(|this, _, _, cx| this.open_zoomie_composer(cx)))
            .child(mascot::sleeping(36.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(px(16.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "No zoomies yet. Tell one what to do and when.",
                    )),
            )
            .into_any_element()
    }

    fn render_zoomie_row(
        &self,
        ix: usize,
        row: ZoomieRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover_key: SharedString = format!("sidebar-zoomie-{}", row.id).into();
        let open_id = row.id.clone();
        let toggle_id = row.id.clone();
        let enabled = row.enabled;
        let dot = status_color(row.status, theme);
        // A running row jogs in its own small view on the pulse clock; the
        // shell re-renders only when the zoomies themselves change.
        let glyph = if row.status == RowStatus::Running {
            mascot::jogging(format!("sidebar-zoomie-jog-{}", row.id), 18.0).into_any_element()
        } else {
            mascot::mascot(Pose::for_row(row.status, 0, true), 18.0).into_any_element()
        };
        let switch = div()
            .id(("sidebar-zoomie-switch", ix))
            .debug_selector(move || format!("sidebar-zoomie-switch-{ix}"))
            .role(gpui::Role::Switch)
            .aria_label(if enabled { "Turn off" } else { "Turn on" })
            .flex_none()
            .w(px(22.0))
            .h(px(13.0))
            .p(px(2.0))
            .rounded_full()
            .flex()
            .items_center()
            .when(enabled, |el| el.justify_end().bg(theme.accent))
            .when(!enabled, |el| {
                el.justify_start().bg(crate::theme::ink(0.16))
            })
            .cursor_pointer()
            .tooltip(crate::settings::widgets::text_tooltip(if enabled {
                "On — click to pause"
            } else {
                "Off — click to turn on"
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let id = toggle_id.clone();
                this.zoomies_feed.update(cx, |feed, cx| {
                    feed.call(
                        methods::SET_AGENT_ENABLED,
                        serde_json::json!({ "id": id, "enabled": !enabled }),
                        cx,
                    )
                });
            }))
            .child(div().size(px(9.0)).rounded_full().bg(gpui::white()));
        div()
            .id(("sidebar-zoomie", ix))
            .debug_selector(move || format!("sidebar-zoomie-{ix}"))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(row.name.clone()))
            .h(px(ROW_HEIGHT))
            .px(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(motion::hover_blend(
                &hover_key,
                theme.text.opacity(if enabled { 0.85 } else { 0.55 }),
                theme.text,
            ))
            .bg(motion::hover_blend(
                &hover_key,
                theme.glass_hover().opacity(0.0),
                theme.glass_hover(),
            ))
            .on_hover(motion::hover_listener(hover_key))
            .tooltip(crate::settings::widgets::text_tooltip(row.status.label()))
            .on_click(
                cx.listener(move |this, _, _, cx| this.open_zoomie_detail(open_id.clone(), cx)),
            )
            .child(
                div()
                    .flex_none()
                    .size(px(18.0))
                    .when(!enabled, |el| el.opacity(0.5))
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(row.name)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(SharedString::from(row.detail)),
            )
            .child(div().flex_none().size(px(6.0)).rounded_full().bg(dot))
            .child(switch)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::agents::{
        AgentRun, AgentRunStatus, AgentSummary, AgentTriggerKind, AgentsSnapshot,
    };

    struct SidebarHost(Entity<Shell>);
    impl Render for SidebarHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.0.update(cx, |shell, cx| {
                div()
                    .w(px(280.0))
                    .h(px(800.0))
                    .child(shell.render_chat_sidebar(&Theme::default(), cx))
            })
        }
    }

    fn init(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
        });
    }

    fn new_shell(dir: &std::path::Path, cx: &mut Context<Shell>) -> Shell {
        let state = cx.new(|_| AppState::new());
        let mut shell = Shell::new(
            state,
            EngineBootConfig {
                data_dir: dir.into(),
                ipc_port: 0,
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zeron_proto::HarnessId::Mock,
            },
            cx,
        );
        shell.settings.sidebar_organization = SidebarOrganization::InOneList;
        shell.state.update(cx, |state, _| {
            state.workspace_scope = Some(WorkspaceScope::Local);
            state.local_device_id = Some("local".into());
            state.chats_synced = true;
            state.spaces_synced = true;
            state.spaces = vec![
                serde_json::from_value(serde_json::json!({
                    "id": "space-1", "deviceId": "local", "path": "/repo",
                    "createdAt": "2026-10-01T00:00:00Z",
                }))
                .unwrap(),
            ];
        });
        shell
    }

    fn agent(id: &str, name: &str) -> AgentSummary {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "harness": "codex",
            "spaceId": "space-1",
            "instructions": "go",
            "triggers": [{ "kind": "interval", "everySecs": 3600 }],
            "workspacePath": "/repo",
            "memoryPath": "/m",
            "webhookPath": "/w",
            "consecutiveFailures": 0,
            "nextFireAt": Utc::now() + chrono::Duration::minutes(90),
            "createdAt": "2026-10-01T00:00:00Z",
            "updatedAt": "2026-10-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn set_agents(shell: &mut Shell, agents: Vec<AgentSummary>, cx: &mut Context<Shell>) {
        shell.zoomies_feed.update(cx, |feed, cx| {
            feed.set_snapshot_for_test(
                AgentsSnapshot {
                    agents,
                    unread_runs: 0,
                    needs_approval_runs: 0,
                },
                cx,
            )
        });
    }

    fn redraw(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
    }

    #[gpui::test]
    fn sidebar_section_shows_empty_state_rows_and_respects_settings(cx: &mut gpui::TestAppContext) {
        init(cx);
        let dir = tempfile::tempdir().unwrap();
        let (host, cx) =
            cx.add_window_view(|_, cx| SidebarHost(cx.new(|cx| new_shell(dir.path(), cx))));
        let shell = host.read_with(cx, |host, _| host.0.clone());

        // No snapshot yet (or an engine without zoomies): nothing at all.
        redraw(cx);
        assert!(cx.debug_bounds("zoomies-toggle").is_none());

        // None yet: the section shows, with the sleeping speedster's nudge.
        shell.update(cx, |shell, cx| set_agents(shell, vec![], cx));
        redraw(cx);
        assert!(cx.debug_bounds("zoomies-toggle").is_some());
        assert!(cx.debug_bounds("zoomies-empty").is_some());
        assert!(cx.debug_bounds("sidebar-zoomies-new").is_some());

        // Rows, in engine order, each with its switch.
        shell.update(cx, |shell, cx| {
            set_agents(
                shell,
                vec![agent("a", "Nightly"), agent("b", "CI watch")],
                cx,
            )
        });
        redraw(cx);
        assert!(cx.debug_bounds("zoomies-empty").is_none());
        let first = cx.debug_bounds("sidebar-zoomie-0").unwrap();
        let second = cx.debug_bounds("sidebar-zoomie-1").unwrap();
        assert!(first.top() < second.top());
        assert!(cx.debug_bounds("sidebar-zoomie-switch-1").is_some());

        // Collapsing persists in settings and hides the rows.
        shell.update(cx, |shell, cx| {
            assert!(shell.settings.sidebar_zoomies_open);
            shell.settings.sidebar_zoomies_open = false;
            cx.notify();
        });
        redraw(cx);
        assert!(cx.debug_bounds("zoomies-toggle").is_some());
        let collapsed = cx.debug_bounds("sidebar-zoomie-0");
        assert!(collapsed.is_none_or(
            |bounds| f32::from(bounds.size.height) < 1.0 || bounds.top() >= first.top()
        ));

        // The sidebar's View → Show → Zoomies toggle removes the section.
        shell.update(cx, |shell, cx| {
            shell.settings.sidebar_show_zoomies = false;
            cx.notify();
        });
        redraw(cx);
        assert!(cx.debug_bounds("zoomies-toggle").is_none());
    }

    /// A running zoomie jogs in its own row view: the feed (which the shell
    /// observes, cascading to the transcript and sidebar) notifies only when
    /// the zoomies change, never per frame.
    #[gpui::test]
    fn a_running_zoomie_never_re_renders_the_shell_per_frame(cx: &mut gpui::TestAppContext) {
        init(cx);
        let dir = tempfile::tempdir().unwrap();
        let (host, cx) =
            cx.add_window_view(|_, cx| SidebarHost(cx.new(|cx| new_shell(dir.path(), cx))));
        let shell = host.read_with(cx, |host, _| host.0.clone());
        let feed = shell.read_with(cx, |shell, _| shell.zoomies_feed.clone());
        let notified = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let count = notified.clone();
        let _observe = cx.update(|_, cx| cx.observe(&feed, move |_, _| count.set(count.get() + 1)));

        let mut running = agent("a1", "Nightly build");
        running.active_run_id = Some("run-1".into());
        shell.update(cx, |shell, cx| set_agents(shell, vec![running.clone()], cx));
        redraw(cx);
        assert_eq!(notified.get(), 1, "a new snapshot notifies once");
        assert!(cx.debug_bounds("sidebar-zoomie-0").is_some());

        // The engine re-sending the same list is not a change.
        shell.update(cx, |shell, cx| set_agents(shell, vec![running.clone()], cx));
        assert_eq!(notified.get(), 1);

        // Seconds of jogging: no feed notifications at all.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        assert_eq!(notified.get(), 1, "the jog must not tick the shell");

        // The run ending is a change.
        running.active_run_id = None;
        shell.update(cx, |shell, cx| set_agents(shell, vec![running], cx));
        assert_eq!(notified.get(), 2);
    }

    #[gpui::test]
    fn plus_and_palette_open_the_home_composer_in_zoomie_mode(cx: &mut gpui::TestAppContext) {
        init(cx);
        let dir = tempfile::tempdir().unwrap();
        let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |state, cx| {
                    state.chats = vec![
                        serde_json::from_value(serde_json::json!({
                            "id": "c1", "title": "Old", "deviceId": "local", "archived": false,
                            "createdAt": "2026-09-20T00:00:00Z", "spaceId": "space-1",
                        }))
                        .unwrap(),
                    ];
                    state.select_chat(Some("c1".into()), cx);
                });
                shell.open_zoomie_composer(cx);
                assert_eq!(shell.state.read(cx).selected_chat, None);
                assert!(shell.composer.read(cx).zoomie_active(cx));
            })
            .unwrap();
    }

    #[gpui::test]
    fn convert_chat_prefills_the_zoomie_composer(cx: &mut gpui::TestAppContext) {
        init(cx);
        let dir = tempfile::tempdir().unwrap();
        let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |state, cx| {
                    state.chats = vec![
                        serde_json::from_value(serde_json::json!({
                            "id": "c1", "title": "Nightly report", "deviceId": "local",
                            "archived": false, "createdAt": "2026-09-20T00:00:00Z",
                            "spaceId": "space-1",
                            "config": {
                                "harness": "codex", "model": "gpt-5.4", "sandbox": "read-only",
                            },
                        }))
                        .unwrap(),
                    ];
                    state.select_chat(Some("c1".into()), cx);
                    state.transcript = vec![serde_json::from_value(serde_json::json!({
                        "id": "u1", "role": "user", "createdAt": 0, "deviceId": "local",
                        "parts": [{ "kind": "text", "id": "p", "text": "Check the nightly build" }],
                    }))
                    .unwrap()];
                });
                shell.convert_chat_to_zoomie("c1".into(), cx);
                let state = shell.state.read(cx);
                assert_eq!(state.selected_chat, None);
                assert_eq!(
                    state.selected_space_row().map(|s| s.id.as_str()),
                    Some("space-1")
                );
                let composer = shell.composer.read(cx);
                assert!(composer.zoomie_active(cx));
                assert_eq!(composer.input.read(cx).text(), "Check the nightly build");
                let resolved = composer.pickers().read(cx).resolved(cx);
                assert_eq!(resolved.harness, Some(zeron_proto::HarnessId::Codex));
                assert_eq!(resolved.model.as_deref(), Some("gpt-5.4"));
                assert_eq!(resolved.access_mode, zeron_proto::AccessMode::Supervised);
            })
            .unwrap();
    }

    #[test]
    fn body_height_tracks_rows() {
        assert_eq!(
            zoomies_body_height(0),
            SIDEBAR_DISCLOSURE_BODY_INSET + EMPTY_HEIGHT
        );
        assert_eq!(
            zoomies_body_height(3),
            SIDEBAR_DISCLOSURE_BODY_INSET + 3.0 * ROW_HEIGHT + 2.0 * SIDEBAR_LIST_GAP
        );
    }

    #[allow(dead_code)]
    fn _run(status: AgentRunStatus) -> AgentRun {
        AgentRun {
            id: "r".into(),
            agent_id: "a".into(),
            agent_name: "a".into(),
            trigger: AgentTriggerKind::Manual,
            scheduled_at: Utc::now(),
            started_at: None,
            finished_at: None,
            status,
            summary: None,
            chat_id: None,
            collapsed: 0,
            read: false,
            archived: false,
            created_at: Utc::now(),
        }
    }
}
