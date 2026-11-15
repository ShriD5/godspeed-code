use super::*;

/// The root view for a session opened in its own desktop window.
///
/// It deliberately contains only the transcript and composer. The main
/// window remains the owner of navigation, sidebar state, and auxiliary
/// surfaces.
pub(super) struct DetachedSessionWindow {
    pub(super) state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    title: String,
    _state_observation: Subscription,
    _transcript_events: Subscription,
    _composer_events: Subscription,
}

impl DetachedSessionWindow {
    pub(super) fn new(
        parent: Entity<AppState>,
        chat: zeron_proto::Chat,
        links: crate::markdown::render::LinkUi,
        main_shell: gpui::WeakEntity<Shell>,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = chat
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Session".into());
        let state = cx.new(|cx| AppState::independent_chat_state(&parent, chat, false, cx));
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links);
        });
        let composer = cx.new(|cx| {
            let mut composer = Composer::new(state.clone(), cx);
            composer.set_side_chat(cx);
            composer.focus_pending = true;
            composer
        });

        let transcript_events = cx.subscribe(&transcript, {
            let main_shell = main_shell.clone();
            move |_: &mut DetachedSessionWindow, _, event: &TranscriptEvent, cx| {
                if let TranscriptEvent::OpenChat { chat_id } = event {
                    let _ = main_shell.update(cx, |shell, cx| {
                        shell.open_detached_chat(chat_id.clone(), cx);
                    });
                }
            }
        });
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            let main_shell = main_shell.clone();
            move |_: &mut DetachedSessionWindow, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent {
                    chat_id,
                    message_id,
                } => transcript.update(cx, |transcript, cx| {
                    transcript.on_own_send(chat_id.clone(), message_id.clone(), cx)
                }),
                ComposerEvent::Queued {
                    chat_id,
                    message_id,
                } => transcript.update(cx, |transcript, cx| {
                    transcript.on_own_queued_send(chat_id.clone(), message_id.clone(), cx)
                }),
                ComposerEvent::WorkspaceCommand(command) => {
                    let _ = main_shell.update(cx, |shell, cx| {
                        shell.pending_workspace_command = Some(*command);
                        cx.notify();
                    });
                }
                _ => {}
            }
        });
        let state_observation = cx.observe(&state, |_, _, cx| cx.notify());

        Self {
            state,
            transcript,
            composer,
            title,
            _state_observation: state_observation,
            _transcript_events: transcript_events,
            _composer_events: composer_events,
        }
    }

    pub(super) fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |composer, cx| {
            composer.focus_pending = true;
            cx.notify();
        });
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
    }
}

impl Render for DetachedSessionWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let title = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.title.clone())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| self.title.clone());
        window.set_window_title(&format!("Godspeed — {title}"));

        let width = f32::from(window.viewport_size().width);
        self.composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer.set_available_width((width - 32.0).max(320.0), cx);
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(
                div()
                    .id("detached-session-title")
                    .h(px(Theme::TITLEBAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .px(px(18.0))
                    .bg(theme.surface)
                    .text_color(theme.text)
                    .text_size(crate::typography::ui_rems(13.0))
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(SharedString::from(title)),
            )
            .child(div().flex_1().min_h_0().child(crate::edge_fade::edge_faded(
                Theme::TRANSCRIPT_FADE_BAND,
                true,
                true,
                div().size_full().child(self.transcript.clone()),
            )))
            .child(div().flex_none().child(self.composer.clone()))
            .child(Shell::attachment_drop_overlay(&theme))
            .child(Shell::mention_drop_overlay(&theme))
    }
}

impl Shell {
    pub(super) fn open_detached_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let Some(chat) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .cloned()
            .or_else(|| {
                self.side_chats.values().find_map(|tab| {
                    tab.state
                        .read(cx)
                        .chats
                        .iter()
                        .find(|chat| chat.id == chat_id)
                        .cloned()
                })
            })
        else {
            return;
        };

        if let Some(handle) = self.detached_session_windows.get(&chat_id).copied() {
            if handle
                .update(cx, |window, native_window, cx| {
                    native_window.activate_window();
                    window.focus_composer(native_window, cx);
                })
                .is_ok()
            {
                return;
            }
            self.detached_session_windows.remove(&chat_id);
        }

        let title = chat
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Session".into());
        let links = Self::session_links(Some(chat_id.clone()), cx);
        let close_chat_id = chat_id.clone();
        let parent = self.state.clone();
        let main_shell = cx.entity().downgrade();
        let close_shell = main_shell.clone();
        let window_background = Theme::of(cx).window_background_appearance();
        let window = cx
            .open_window(
                gpui::WindowOptions {
                    window_min_size: Some(gpui::size(px(640.0), px(480.0))),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: cfg!(target_os = "windows")
                            .then(|| format!("Godspeed — {title}").into()),
                        appears_transparent: true,
                        traffic_light_position: Some(gpui::point(px(14.0), px(14.0))),
                        ..Default::default()
                    }),
                    app_owns_titlebar_drag: true,
                    window_decorations: cfg!(target_os = "linux")
                        .then_some(gpui::WindowDecorations::Client),
                    window_background,
                    app_id: Some("godspeed".into()),
                    ..Default::default()
                },
                move |window, cx| {
                    window.set_rem_size(px(crate::typography::font_size(cx).pixels()));
                    crate::appearance::observe_window(window, cx).detach();
                    let root = cx
                        .new(|cx| DetachedSessionWindow::new(parent, chat, links, main_shell, cx));
                    let chat_id = close_chat_id.clone();
                    window.on_window_should_close(cx, move |_, cx| {
                        let _ = close_shell.update(cx, |shell, _| {
                            shell.detached_session_windows.remove(&chat_id);
                        });
                        true
                    });
                    root
                },
            )
            .expect("failed to open detached session window");
        self.detached_session_windows.insert(chat_id, window);
        cx.notify();
    }

    pub(super) fn detached_chat_open(&self, chat_id: &str, cx: &App) -> bool {
        self.detached_session_windows
            .get(chat_id)
            .and_then(|window| window.entity(cx).ok())
            .is_some()
    }

    pub(super) fn detached_chat_state(&self, chat_id: &str, cx: &App) -> Option<Entity<AppState>> {
        self.detached_session_windows
            .get(chat_id)
            .and_then(|window| window.entity(cx).ok())
            .map(|window| window.read(cx).state.clone())
    }

    pub(super) fn close_deleted_detached_sessions(
        &mut self,
        chat_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        let open_ids: Vec<_> = self
            .detached_session_windows
            .keys()
            .filter(|chat_id| !chat_ids.iter().any(|id| id == *chat_id))
            .cloned()
            .collect();
        for chat_id in open_ids {
            if let Some(window) = self.detached_session_windows.remove(&chat_id) {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            }
        }
    }
}
