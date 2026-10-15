//! Settings → Import: bring chats over from T3 Code, Claude Code, Codex and
//! the Cursor Agent CLI.
//!
//! The engine scans every source on open (`ScanChatImportSources`); each
//! source lists its projects with a switch (on by default when it holds
//! anything new), and its Import button streams `RunChatImport` progress
//! into the page, then rescans so the states settle.

use std::collections::HashMap;

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use gpui_tokio::Tokio;

use zeron_proto::chat_import::{
    ChatImportPreview, ChatImportPreviewProject, ChatImportProgress, ChatImportRequest,
    ChatImportSource, ChatImportState, ChatImportSummary,
};
use zeron_rpc::methods;

use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// What a project would contribute: new chats plus ones with newer messages.
fn importable(project: &ChatImportPreviewProject) -> usize {
    project.count(ChatImportState::New) + project.count(ChatImportState::Update)
}

/// The import in flight.
struct Running {
    source: ChatImportSource,
    done: usize,
    total: usize,
    current: Option<String>,
}

pub struct ImportPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    /// `None` until the first scan answers.
    previews: Option<Vec<ChatImportPreview>>,
    /// Per-project switch, keyed by (source, project path). Absent = the
    /// default (on when the project has anything to import).
    selected: HashMap<(ChatImportSource, String), bool>,
    running: Option<Running>,
    summary: Option<(ChatImportSource, ChatImportSummary)>,
    error: Option<SharedString>,
    scan_task: Option<Task<()>>,
    import_task: Option<Task<()>>,
}

impl ImportPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            previews: None,
            selected: HashMap::new(),
            running: None,
            summary: None,
            error: None,
            scan_task: None,
            import_task: None,
        };
        page.rescan(cx);
        page
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<Vec<ChatImportPreview>>(
                    methods::SCAN_CHAT_IMPORT_SOURCES,
                    serde_json::json!({}),
                )
                .await;
            this.update(cx, |page, cx| {
                page.scan_task = None;
                match result {
                    Ok(previews) => page.previews = Some(previews),
                    Err(err) => page.error = Some(format!("Couldn't scan for chats: {err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn is_selected(&self, source: ChatImportSource, project: &ChatImportPreviewProject) -> bool {
        importable(project) > 0
            && self
                .selected
                .get(&(source, project.path.clone()))
                .copied()
                .unwrap_or(true)
    }

    fn toggle(&mut self, source: ChatImportSource, path: String, cx: &mut Context<Self>) {
        let on = self
            .previews
            .iter()
            .flatten()
            .filter(|p| p.source == source)
            .flat_map(|p| &p.projects)
            .find(|p| p.path == path)
            .is_some_and(|p| self.is_selected(source, p));
        self.selected.insert((source, path), !on);
        cx.notify();
    }

    /// Selected projects of `source` and the chats they'd bring.
    fn selection(&self, source: ChatImportSource) -> (Vec<String>, usize) {
        let mut paths = Vec::new();
        let mut chats = 0;
        for preview in self
            .previews
            .iter()
            .flatten()
            .filter(|p| p.source == source)
        {
            for project in &preview.projects {
                if self.is_selected(source, project) {
                    paths.push(project.path.clone());
                    chats += importable(project);
                }
            }
        }
        (paths, chats)
    }

    fn start_import(&mut self, source: ChatImportSource, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let (projects, chats) = self.selection(source);
        if chats == 0 {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let request = ChatImportRequest {
            source,
            projects: Some(projects),
        };
        let Ok(params) = serde_json::to_value(&request) else {
            return;
        };
        self.summary = None;
        self.running = Some(Running {
            source,
            done: 0,
            total: chats,
            current: None,
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let stream = Tokio::spawn(cx, async move {
            let mut items = engine
                .client()
                .subscribe(methods::RUN_CHAT_IMPORT, params)
                .await
                .map_err(|error| error.to_string())?;
            while let Some(item) = items.recv().await {
                let _ = tx.send(item);
            }
            Ok::<(), String>(())
        });
        self.import_task = Some(cx.spawn(async move |this, cx| {
            while let Some(item) = rx.recv().await {
                let Ok(progress) = serde_json::from_value::<ChatImportProgress>(item) else {
                    continue;
                };
                this.update(cx, |page, cx| page.apply_progress(progress, cx))
                    .ok();
            }
            let failure = match stream.await {
                Ok(Err(error)) => Some(error),
                _ => None,
            };
            this.update(cx, |page, cx| {
                page.import_task = None;
                if page.running.take().is_some() {
                    page.error = Some(
                        failure
                            .unwrap_or_else(|| "The import ended before it finished.".into())
                            .into(),
                    );
                }
                page.selected.clear();
                page.rescan(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply_progress(&mut self, progress: ChatImportProgress, cx: &mut Context<Self>) {
        match progress {
            ChatImportProgress::Start { total } => {
                if let Some(running) = &mut self.running {
                    running.total = total;
                }
            }
            ChatImportProgress::Chat {
                index,
                total,
                title,
            } => {
                if let Some(running) = &mut self.running {
                    running.done = index;
                    running.total = total;
                    running.current = Some(title);
                }
            }
            ChatImportProgress::Summary { summary } => {
                if let Some(running) = self.running.take() {
                    self.summary = Some((running.source, summary));
                }
            }
        }
        cx.notify();
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn render_source(
        &self,
        preview: &ChatImportPreview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let source = preview.source;
        let label = source.label();
        if !preview.found {
            return widgets::section(
                theme,
                label,
                widgets::section_card(theme).mt_0().child(
                    widgets::card_row(theme, true).child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Not found on this device"))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    SharedString::from(preview.location.clone()).into_any_element(),
                                ],
                            )),
                    ),
                ),
            )
            .into_any_element();
        }
        let running_here = self.running.as_ref().is_some_and(|r| r.source == source);
        let busy = self.running.is_some();
        let (_, chats) = self.selection(source);
        let mut card = widgets::section_card(theme).mt_0();
        if preview.projects.is_empty() {
            card = card.child(
                widgets::card_row(theme, true).child(widgets::row_title(theme, "No chats found")),
            );
        }
        for (ix, project) in preview.projects.iter().enumerate() {
            let new = importable(project);
            let here = project.count(ChatImportState::Imported);
            let elsewhere = project.count(ChatImportState::Elsewhere);
            let mut meta: Vec<AnyElement> = vec![
                div()
                    .truncate()
                    .child(SharedString::from(project.path.clone()))
                    .into_any_element(),
                SharedString::from(format!("{} chats", project.chats.len())).into_any_element(),
            ];
            if let Some((first, last)) = project.date_range() {
                meta.push(
                    SharedString::from(format!(
                        "{} – {}",
                        first.format("%b %-d, %Y"),
                        last.format("%b %-d, %Y")
                    ))
                    .into_any_element(),
                );
            }
            if here > 0 {
                meta.push(SharedString::from(format!("{here} already here")).into_any_element());
            }
            let history_only = project
                .chats
                .iter()
                .filter(|c| !c.resumable && matches!(c.state, ChatImportState::New))
                .count();
            if history_only > 0 {
                meta.push(
                    SharedString::from(format!("{history_only} history only")).into_any_element(),
                );
            }
            if elsewhere > 0 {
                meta.push(
                    SharedString::from(format!("{elsewhere} via another source"))
                        .into_any_element(),
                );
            }
            let on = self.is_selected(source, project);
            let path = project.path.clone();
            let control: AnyElement = if new == 0 {
                widgets::badge(theme, "Imported").into_any_element()
            } else {
                div()
                    .id(("import-project", ix))
                    .flex_none()
                    .w(px(widgets::SWITCH_WIDTH))
                    .h(px(40.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .role(gpui::Role::Switch)
                    .aria_label(SharedString::from(format!("Import {}", project.title)))
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .tab_index(0)
                    .cursor_pointer()
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle(source, path.clone(), cx);
                        }))
                    })
                    .child(widgets::toggle_switch(
                        theme,
                        on,
                        format!("import-{}-{ix}", source.slug()),
                    ))
                    .into_any_element()
            };
            card = card.child(
                widgets::card_row(theme, ix == 0)
                    .child(widgets::row_tile(theme, crate::icons::FOLDER))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(widgets::row_title(theme, project.title.clone()))
                                    .when(new > 0, |el| {
                                        el.child(widgets::badge_active(theme, format!("{new} new")))
                                    }),
                            )
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .child(control),
            );
        }
        let progress: Option<AnyElement> = running_here.then(|| {
            let running = self.running.as_ref().expect("checked above");
            let line = match &running.current {
                Some(title) => format!(
                    "Importing {} of {} · {title}",
                    (running.done + 1).min(running.total.max(1)),
                    running.total
                ),
                None => "Preparing…".into(),
            };
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(line))
                .into_any_element()
        });
        let summary: Option<AnyElement> = self
            .summary
            .as_ref()
            .filter(|(s, _)| *s == source && !running_here)
            .map(|(_, summary)| {
                let mut line = format!(
                    "Imported {} · updated {} · skipped {}",
                    summary.imported, summary.updated, summary.skipped
                );
                if summary.spaces_created > 0 {
                    line.push_str(&format!(" · {} new projects", summary.spaces_created));
                }
                if !summary.errors.is_empty() {
                    line.push_str(&format!(" · {} failed", summary.errors.len()));
                }
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(line))
                    .into_any_element()
            });
        let import_label = if running_here {
            "Importing…".to_string()
        } else if chats == 0 {
            "Nothing to import".to_string()
        } else {
            format!("Import {chats} chat{}", if chats == 1 { "" } else { "s" })
        };
        let enabled = chats > 0 && !busy;
        let footer = div()
            .mt(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .children(progress)
            .children(summary)
            .child(div().flex_1())
            .child(
                widgets::action_button(theme, widgets::ActionTone::Solid)
                    .id(SharedString::from(format!("import-run-{}", source.slug())))
                    .flex_none()
                    .when(!enabled, |el| el.opacity(0.5).cursor_default())
                    .tab_index(0)
                    .role(gpui::Role::Button)
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .when(enabled, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.start_import(source, cx);
                        }))
                    })
                    .child(SharedString::from(import_label)),
            );
        div()
            .child(widgets::section(theme, label, card))
            .child(footer)
            .children(
                preview
                    .warnings
                    .first()
                    .map(|warning| widgets::warning_strip(theme, warning.clone())),
            )
            .into_any_element()
    }
}

impl popover::ScrollRailHost for ImportPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for ImportPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let scanning = self.scan_task.is_some();
        let body: Vec<AnyElement> = match &self.previews {
            None => vec![
                div()
                    .mt(px(32.0))
                    .px(px(8.0))
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Looking for chats…"))
                    .into_any_element(),
            ],
            Some(previews) => previews
                .iter()
                .map(|preview| self.render_source(preview, &theme, cx))
                .collect(),
        };
        let rescan = widgets::action_button(&theme, widgets::ActionTone::Quiet)
            .id("import-rescan")
            .flex_none()
            .when(scanning || self.running.is_some(), |el| {
                el.opacity(0.5).cursor_default()
            })
            .tab_index(0)
            .role(gpui::Role::Button)
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .when(!scanning && self.running.is_none(), |el| {
                el.on_click(cx.listener(|this, _, _, cx| this.rescan(cx)))
            })
            .child(
                crate::icons::icon(crate::icons::REFRESH)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from(if scanning {
                "Scanning…"
            } else {
                "Scan again"
            }));
        let scrollbar = popover::rail(self, "import-page-scrollbar", &theme, cx);
        div()
            .id("import-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("import-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(
                            widgets::page_column()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_end()
                                        .gap(px(16.0))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .child(widgets::page_header(&theme, "Import", None))
                                                .child(widgets::page_subtitle(
                                                    &theme,
                                                    "Bring chats over from T3 Code, Claude Code, Codex and \
                                                     Cursor. They keep their projects and history; sending a \
                                                     message in a Claude Code or Codex chat picks up the \
                                                     original agent session.",
                                                )),
                                        )
                                        .child(rescan),
                                )
                                .children(body),
                        ),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::chat_import::ChatImportPreviewChat;

    fn chat(state: ChatImportState) -> ChatImportPreviewChat {
        ChatImportPreviewChat {
            source_key: "t3:x".into(),
            title: "x".into(),
            created_at: None,
            updated_at: None,
            harness: None,
            resumable: false,
            state,
        }
    }

    #[test]
    fn importable_counts_new_and_updated_chats() {
        let project = ChatImportPreviewProject {
            path: "/p".into(),
            title: "p".into(),
            has_space: false,
            chats: vec![
                chat(ChatImportState::New),
                chat(ChatImportState::Update),
                chat(ChatImportState::Imported),
                chat(ChatImportState::Elsewhere),
            ],
        };
        assert_eq!(importable(&project), 2);
    }
}
