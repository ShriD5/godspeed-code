//! Settings → Zoomies: always-on agents ("zoomies" in the UI) hosted by
//! this device's engine (`zeron_proto::agents`). Lists each agent with its
//! status and next fire, a small create/edit form (one schedule trigger per
//! agent here; the engine accepts more), the run inbox, whose rows open the
//! run's chat, and the Telegram bot that fronts them (`settings::telegram`).

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};
use gpui_tokio::Tokio;
use serde_json::json;

use zeron_proto::HarnessId;
use zeron_proto::agents::{AgentRun, AgentRunStatus, AgentSummary, AgentTrigger, AgentsSnapshot};
use zeron_rpc::methods;

use crate::composer::ComposerInput;
use crate::popover;
use crate::settings::telegram::TelegramSection;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const INBOX_LIMIT: u32 = 50;

pub enum AgentsPageEvent {
    OpenChat(String),
}

/// The schedule choices the form offers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TriggerChoice {
    Manual,
    Interval,
    Cron,
    Webhook,
}

impl TriggerChoice {
    const ALL: [TriggerChoice; 4] = [Self::Manual, Self::Interval, Self::Cron, Self::Webhook];

    fn label(self) -> &'static str {
        match self {
            Self::Manual => "Only when run by hand",
            Self::Interval => "Every N minutes",
            Self::Cron => "Cron schedule",
            Self::Webhook => "Webhook",
        }
    }

    fn owns(trigger: &AgentTrigger) -> bool {
        matches!(
            trigger,
            AgentTrigger::Interval { .. } | AgentTrigger::Cron { .. } | AgentTrigger::Webhook
        )
    }
}

struct Editor {
    /// `None` creates a new agent.
    id: Option<String>,
    name: Entity<ComposerInput>,
    instructions: Entity<ComposerInput>,
    /// Minutes for an interval, the expression for cron.
    schedule: Entity<ComposerInput>,
    timezone: Entity<ComposerInput>,
    choice: TriggerChoice,
    /// Triggers the form doesn't edit (file watch, one-shot) ride along.
    kept: Vec<AgentTrigger>,
    space_id: Option<String>,
    harness: Option<HarnessId>,
    enabled: bool,
    choice_select: widgets::SelectState,
    space_select: widgets::SelectState,
    harness_select: widgets::SelectState,
}

pub struct AgentsPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    snapshot: Option<AgentsSnapshot>,
    runs: Vec<AgentRun>,
    harnesses: Vec<(HarnessId, String)>,
    editor: Option<Editor>,
    telegram: Entity<TelegramSection>,
    error: Option<SharedString>,
    /// A zoomie to open in the editor once the first snapshot lands
    /// ([`Self::open_agent`] from the sidebar or a toast).
    pending_open: Option<String>,
    watch_task: Option<Task<()>>,
    harness_task: Option<Task<()>>,
    task: Option<Task<()>>,
    _observe: Subscription,
}

impl EventEmitter<AgentsPageEvent> for AgentsPage {}

use crate::zoomies::local_timezone;

fn next_fire_label(
    at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let minutes = (at - now).num_minutes();
    if minutes < 1 {
        "Next run in under a minute".into()
    } else if minutes < 60 {
        format!("Next run in {minutes}m")
    } else {
        format!(
            "Next run {}",
            at.with_timezone(&chrono::Local).format("%a %H:%M")
        )
    }
}

fn status_label(status: AgentRunStatus) -> &'static str {
    match status {
        AgentRunStatus::Queued => "Queued",
        AgentRunStatus::Running => "Running",
        AgentRunStatus::NeedsApproval => "Needs approval",
        AgentRunStatus::Succeeded => "Done",
        AgentRunStatus::Failed => "Failed",
        AgentRunStatus::Skipped => "Skipped",
        AgentRunStatus::Limited => "Limited",
    }
}

impl AgentsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let telegram = cx.new(|cx| TelegramSection::new(state.clone(), cx));
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            snapshot: None,
            runs: Vec::new(),
            harnesses: Vec::new(),
            editor: None,
            telegram,
            error: None,
            pending_open: None,
            watch_task: None,
            harness_task: None,
            task: None,
            _observe: observe,
        };
        page.start_watch(cx);
        page
    }

    /// Follow `WatchAgents`; every snapshot also refreshes the inbox.
    fn start_watch(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<(AgentsSnapshot, Vec<AgentRun>)>();
        let feed = Tokio::spawn(cx, async move {
            let client = engine.client();
            let mut items = client
                .subscribe(methods::WATCH_AGENTS, json!({}))
                .await
                .map_err(|e| e.to_string())?;
            while let Some(item) = items.recv().await {
                let Ok(snapshot) = serde_json::from_value::<AgentsSnapshot>(item) else {
                    continue;
                };
                let runs = client
                    .call(methods::LIST_AGENT_RUNS, json!({ "limit": INBOX_LIMIT }))
                    .await
                    .ok()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                if tx.send((snapshot, runs)).is_err() {
                    break;
                }
            }
            Ok::<(), String>(())
        });
        self.load_harnesses(cx);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            while let Some((snapshot, runs)) = rx.recv().await {
                if this
                    .update(cx, |page, cx| {
                        page.snapshot = Some(snapshot);
                        page.runs = runs;
                        page.open_pending(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            if let Ok(Err(err)) = feed.await {
                this.update(cx, |page, cx| {
                    page.error = Some(format!("Zoomies unavailable: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Open zoomie `id` in the editor (now, or when the list arrives).
    pub fn open_agent(&mut self, id: String, cx: &mut Context<Self>) {
        self.pending_open = Some(id);
        self.open_pending(cx);
    }

    fn open_pending(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_open.as_ref() else {
            return;
        };
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let agent = snapshot
            .agents
            .iter()
            .find(|agent| &agent.id == id)
            .cloned();
        self.pending_open = None;
        if let Some(agent) = agent {
            self.open_editor(Some(&agent), cx);
        }
    }

    fn load_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.harness_task = Some(cx.spawn(async move |this, cx| {
            let Ok(value) = engine
                .client()
                .call(methods::LIST_HARNESSES, json!({}))
                .await
            else {
                return;
            };
            let harnesses: Vec<(HarnessId, String)> = value
                .as_array()
                .into_iter()
                .flatten()
                .filter(|h| h["installed"].as_bool().unwrap_or(true))
                .filter(|h| h["enabled"].as_bool() != Some(false))
                .filter_map(|h| {
                    let id: HarnessId = serde_json::from_value(h["id"].clone()).ok()?;
                    (id != HarnessId::Mock).then(|| (id, h["name"].as_str().unwrap_or("").into()))
                })
                .collect();
            this.update(cx, |page, cx| {
                page.harnesses = harnesses;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Fire one agent RPC; failures land in the page's error strip.
    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                if let Err(err) = result {
                    page.error = Some(err.to_string().into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_editor(&mut self, agent: Option<&AgentSummary>, cx: &mut Context<Self>) {
        let input = |placeholder: &str, text: String, single: bool, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            let entity = cx.new(|cx| {
                let input = ComposerInput::new(placeholder, cx);
                if single {
                    input.with_single_line()
                } else {
                    input
                }
            });
            entity.update(cx, |input, cx| input.set_text(text, cx));
            entity
        };
        let draft = agent.map(|a| &a.draft);
        let owned = draft
            .and_then(|d| d.triggers.iter().find(|t| TriggerChoice::owns(t)))
            .cloned();
        let (choice, schedule, timezone) = match &owned {
            Some(AgentTrigger::Interval { every_secs }) => (
                TriggerChoice::Interval,
                (every_secs / 60).to_string(),
                local_timezone(),
            ),
            Some(AgentTrigger::Cron { expr, timezone }) => {
                (TriggerChoice::Cron, expr.clone(), timezone.clone())
            }
            Some(AgentTrigger::Webhook) => {
                (TriggerChoice::Webhook, String::new(), local_timezone())
            }
            _ => (TriggerChoice::Manual, String::new(), local_timezone()),
        };
        let local = self.state.read(cx).local_device_id.clone();
        let default_space = self
            .state
            .read(cx)
            .spaces
            .iter()
            .find(|s| Some(&s.device_id) == local.as_ref())
            .map(|s| s.id.clone());
        self.editor = Some(Editor {
            id: agent.map(|a| a.id.clone()),
            name: input(
                "Name",
                draft.map(|d| d.name.clone()).unwrap_or_default(),
                true,
                cx,
            ),
            instructions: input(
                "What should this zoomie do each run?",
                draft.map(|d| d.instructions.clone()).unwrap_or_default(),
                false,
                cx,
            ),
            schedule: input("60 or 0 9 * * 1-5", schedule, true, cx),
            timezone: input("Europe/Berlin", timezone, true, cx),
            choice,
            kept: draft
                .map(|d| {
                    d.triggers
                        .iter()
                        .filter(|t| !TriggerChoice::owns(t))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            space_id: draft.map(|d| d.space_id.clone()).or(default_space),
            harness: draft
                .map(|d| d.harness)
                .or_else(|| self.harnesses.first().map(|(id, _)| *id)),
            enabled: draft.is_none_or(|d| d.enabled),
            choice_select: widgets::SelectState::default(),
            space_select: widgets::SelectState::default(),
            harness_select: widgets::SelectState::default(),
        });
        self.error = None;
        cx.notify();
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let text = |input: &Entity<ComposerInput>, cx: &Context<Self>| {
            input.read(cx).text().trim().to_string()
        };
        let schedule = text(&editor.schedule, cx);
        let mut triggers = editor.kept.clone();
        match editor.choice {
            TriggerChoice::Manual => {}
            TriggerChoice::Interval => match schedule.parse::<u64>() {
                Ok(minutes) if minutes > 0 => triggers.push(AgentTrigger::Interval {
                    every_secs: minutes * 60,
                }),
                _ => {
                    self.error = Some("Enter the interval in whole minutes".into());
                    cx.notify();
                    return;
                }
            },
            TriggerChoice::Cron => triggers.push(AgentTrigger::Cron {
                expr: schedule,
                timezone: text(&editor.timezone, cx),
            }),
            TriggerChoice::Webhook => triggers.push(AgentTrigger::Webhook),
        }
        let (Some(space_id), Some(harness)) = (editor.space_id.clone(), editor.harness) else {
            self.error = Some("Pick a workspace and a provider".into());
            cx.notify();
            return;
        };
        let draft = json!({
            "name": text(&editor.name, cx),
            "instructions": text(&editor.instructions, cx),
            "spaceId": space_id,
            "harness": harness,
            "enabled": editor.enabled,
            "triggers": triggers,
        });
        let (method, params) = match &editor.id {
            Some(id) => (methods::UPDATE_AGENT, json!({ "id": id, "draft": draft })),
            None => (methods::CREATE_AGENT, json!({ "draft": draft })),
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => page.editor = None,
                    Err(err) => page.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_run(&mut self, run: &AgentRun, cx: &mut Context<Self>) {
        if !run.read {
            self.call(
                methods::MARK_AGENT_RUNS,
                json!({ "ids": [run.id], "read": true }),
                cx,
            );
        }
        if let Some(chat_id) = &run.chat_id {
            cx.emit(AgentsPageEvent::OpenChat(chat_id.clone()));
        }
    }

    pub(crate) fn dismiss_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.editor.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn render_editor(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.editor.as_ref()?;
        let local = self.state.read(cx).local_device_id.clone();
        let spaces: Vec<(String, String)> = self
            .state
            .read(cx)
            .spaces
            .iter()
            .filter(|s| Some(&s.device_id) == local.as_ref())
            .map(|s| (s.id.clone(), s.display_name().to_string()))
            .collect();
        let space_ix = spaces
            .iter()
            .position(|(id, _)| Some(id) == editor.space_id.as_ref())
            .unwrap_or(0);
        let harness_ix = self
            .harnesses
            .iter()
            .position(|(id, _)| Some(*id) == editor.harness)
            .unwrap_or(0);
        let choice_ix = TriggerChoice::ALL
            .iter()
            .position(|c| *c == editor.choice)
            .unwrap_or(0);
        let field = |label: &str, control: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(widgets::field_label(theme, label.to_string()))
                .child(control)
        };
        let space_ids: Vec<String> = spaces.iter().map(|(id, _)| id.clone()).collect();
        let harness_ids: Vec<HarnessId> = self.harnesses.iter().map(|(id, _)| *id).collect();
        let space_select = widgets::select("agent-space", "Workspace", theme, |page: &mut Self| {
            &mut page.editor.as_mut().expect("editor open").space_select
        })
        .options(
            spaces
                .iter()
                .map(|(_, name)| widgets::SelectOption::new(name.clone())),
            space_ix,
        )
        .width(240.0)
        .open_rightward()
        .on_select(move |page, ix, _, cx| {
            if let Some(editor) = page.editor.as_mut() {
                editor.space_id = space_ids.get(ix).cloned();
            }
            cx.notify();
        })
        .render(&editor.space_select, cx);
        let harness_select =
            widgets::select("agent-harness", "Provider", theme, |page: &mut Self| {
                &mut page.editor.as_mut().expect("editor open").harness_select
            })
            .options(
                self.harnesses
                    .iter()
                    .map(|(_, name)| widgets::SelectOption::new(name.clone())),
                harness_ix,
            )
            .width(240.0)
            .open_rightward()
            .on_select(move |page, ix, _, cx| {
                if let Some(editor) = page.editor.as_mut() {
                    editor.harness = harness_ids.get(ix).copied();
                }
                cx.notify();
            })
            .render(&editor.harness_select, cx);
        let choice_select = widgets::select("agent-trigger", "Runs", theme, |page: &mut Self| {
            &mut page.editor.as_mut().expect("editor open").choice_select
        })
        .options(
            TriggerChoice::ALL
                .iter()
                .map(|c| widgets::SelectOption::new(c.label())),
            choice_ix,
        )
        .width(240.0)
        .open_rightward()
        .on_select(|page, ix, _, cx| {
            if let Some(editor) = page.editor.as_mut() {
                editor.choice = TriggerChoice::ALL[ix];
            }
            cx.notify();
        })
        .render(&editor.choice_select, cx);
        let schedule_field = match editor.choice {
            TriggerChoice::Interval => Some(field(
                "Minutes between runs",
                popover::dialog_field(editor.schedule.clone().into_any_element())
                    .into_any_element(),
            )),
            TriggerChoice::Cron => Some(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(12.0))
                    .child(
                        div().flex_1().child(field(
                            "Cron (min hour day month weekday)",
                            popover::dialog_field(editor.schedule.clone().into_any_element())
                                .into_any_element(),
                        )),
                    )
                    .child(
                        div().flex_1().child(field(
                            "Time zone",
                            popover::dialog_field(editor.timezone.clone().into_any_element())
                                .into_any_element(),
                        )),
                    ),
            ),
            TriggerChoice::Webhook => Some(widgets::page_subtitle(
                theme,
                "Saving shows a secret URL. POST JSON to it; {{body.field}} in the instructions is replaced with that field.",
            )),
            TriggerChoice::Manual => None,
        };
        let title = if editor.id.is_some() {
            "Edit zoomie"
        } else {
            "New zoomie"
        };
        Some(
            widgets::section(
                theme,
                title,
                widgets::section_card(theme)
                    .mt(px(0.0))
                    .p(px(16.0))
                    .gap(px(14.0))
                    .child(field(
                        "Name",
                        popover::dialog_field(editor.name.clone().into_any_element())
                            .into_any_element(),
                    ))
                    .child(field(
                        "Instructions",
                        popover::dialog_field(editor.instructions.clone().into_any_element())
                            .min_h(px(80.0))
                            .into_any_element(),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(12.0))
                            .child(field("Workspace", space_select.into_any_element()))
                            .child(field("Provider", harness_select.into_any_element()))
                            .child(field("Runs", choice_select.into_any_element())),
                    )
                    .children(schedule_field)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                widgets::text_action(theme, widgets::ActionTone::Quiet, "Cancel")
                                    .id("agent-editor-cancel")
                                    .on_click(cx.listener(|page, _, _, cx| {
                                        page.editor = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                widgets::text_action(theme, widgets::ActionTone::Solid, "Save")
                                    .id("agent-editor-save")
                                    .on_click(cx.listener(|page, _, _, cx| page.save_editor(cx))),
                            ),
                    ),
            )
            .into_any_element(),
        )
    }

    fn render_agents(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let now = chrono::Utc::now();
        let agents = self
            .snapshot
            .as_ref()
            .map(|s| s.agents.clone())
            .unwrap_or_default();
        if agents.is_empty() {
            return widgets::section(
                theme,
                "Zoomies",
                widgets::section_card(theme).mt(px(0.0)).child(
                    widgets::card_row(theme, true).child(widgets::page_subtitle(
                        theme,
                        "No zoomies yet. A zoomie is a prompt that runs on its own: on a schedule, when a webhook fires or when a file changes. It leaves its results in the inbox below.",
                    )),
                ),
            )
            .into_any_element();
        }
        let mut card = widgets::section_card(theme).mt(px(0.0));
        for (ix, agent) in agents.iter().enumerate() {
            let status: String = if let Some(reason) = &agent.paused_reason {
                reason.clone()
            } else if !agent.draft.enabled {
                "Off".into()
            } else if agent.active_run_id.is_some() {
                agent
                    .last_run
                    .as_ref()
                    .map(|r| status_label(r.status))
                    .unwrap_or("Running")
                    .into()
            } else if let Some(next) = agent.next_fire_at {
                next_fire_label(next, now)
            } else {
                "Runs on demand".into()
            };
            let workspace = std::path::Path::new(&agent.workspace_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| agent.workspace_path.clone());
            let fragments: Vec<AnyElement> = vec![
                SharedString::from(status).into_any_element(),
                SharedString::from(workspace).into_any_element(),
            ];
            let id = agent.id.clone();
            let enabled = agent.draft.enabled && agent.paused_reason.is_none();
            let webhook = agent
                .draft
                .triggers
                .iter()
                .any(|t| matches!(t, AgentTrigger::Webhook))
                .then(|| {
                    agent
                        .webhook_url
                        .clone()
                        .unwrap_or(agent.webhook_path.clone())
                });
            let edit_target = agent.clone();
            let (run_id, toggle_id, delete_id) = (id.clone(), id.clone(), id.clone());
            card = card.child(
                widgets::card_row(theme, ix == 0)
                    .child(widgets::row_tile(theme, crate::icons::CLOCK_CIRCLE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, agent.draft.name.clone()))
                            .child(widgets::meta_line(theme, fragments)),
                    )
                    .when_some(webhook, |row, url| {
                        row.child(
                            widgets::text_action(theme, widgets::ActionTone::Quiet, "Copy URL")
                                .id(("agent-webhook", ix))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                                })),
                        )
                    })
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Filled, "Run now")
                            .id(("agent-run", ix))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.call(methods::RUN_AGENT_NOW, json!({ "id": run_id }), cx);
                            })),
                    )
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Quiet, "Edit")
                            .id(("agent-edit", ix))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_editor(Some(&edit_target), cx);
                            })),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("agent-delete", ix))
                            .aria_label("Delete zoomie")
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.call(methods::DELETE_AGENT, json!({ "id": delete_id }), cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .id(("agent-enabled", ix))
                            .cursor_pointer()
                            .role(gpui::Role::Switch)
                            .aria_label("Enabled")
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.call(
                                    methods::SET_AGENT_ENABLED,
                                    json!({ "id": toggle_id, "enabled": !enabled }),
                                    cx,
                                );
                            }))
                            .child(widgets::toggle_switch(
                                theme,
                                enabled,
                                format!("agent-{id}"),
                            )),
                    ),
            );
        }
        widgets::section(theme, "Zoomies", card).into_any_element()
    }

    fn render_inbox(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let now = chrono::Utc::now();
        let runs = self.runs.clone();
        let label = match self.snapshot.as_ref().map(|s| s.unread_runs).unwrap_or(0) {
            0 => "Inbox".to_string(),
            unread => format!("Inbox · {unread} unread"),
        };
        if runs.is_empty() {
            return widgets::section(
                theme,
                label,
                widgets::section_card(theme).mt(px(0.0)).child(
                    widgets::card_row(theme, true)
                        .child(widgets::page_subtitle(theme, "Runs show up here.")),
                ),
            )
            .into_any_element();
        }
        let mut card = widgets::section_card(theme).mt(px(0.0));
        for (ix, run) in runs.into_iter().enumerate() {
            let when = run.finished_at.or(run.started_at).unwrap_or(run.created_at);
            let mut heading = format!("{} · {}", run.agent_name, run.trigger.label());
            if run.collapsed > 0 {
                heading.push_str(&format!(" (+{})", run.collapsed));
            }
            let badge = match run.status {
                AgentRunStatus::Succeeded => widgets::badge_active(theme, status_label(run.status)),
                _ => widgets::badge(theme, status_label(run.status)),
            };
            let summary = run.summary.clone().unwrap_or_default();
            let archive_id = run.id.clone();
            let opens = run.chat_id.is_some();
            let unread = !run.read;
            let target = run.clone();
            card = card.child(
                widgets::card_row(theme, ix == 0)
                    .id(("agent-run-row", ix))
                    .when(opens, |row| {
                        row.cursor_pointer()
                            .on_click(cx.listener(move |page, _, _, cx| page.open_run(&target, cx)))
                    })
                    .child(badge)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                widgets::row_title(theme, heading)
                                    .when(!unread, |el| el.font_weight(gpui::FontWeight::NORMAL)),
                            )
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    SharedString::from(crate::state::format_time_ago(when, now))
                                        .into_any_element(),
                                    div()
                                        .truncate()
                                        .child(SharedString::from(summary))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Quiet, "Archive")
                            .id(("agent-run-archive", ix))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                cx.stop_propagation();
                                page.call(
                                    methods::MARK_AGENT_RUNS,
                                    json!({ "ids": [archive_id], "read": true, "archived": true }),
                                    cx,
                                );
                            })),
                    ),
            );
        }
        widgets::section(theme, label, card).into_any_element()
    }
}

impl popover::ScrollRailHost for AgentsPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for AgentsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.watch_task.is_none() {
            self.start_watch(cx);
        }
        let theme = Theme::of(cx).for_settings_surface();
        let count = self.snapshot.as_ref().map(|s| s.agents.len()).unwrap_or(0);
        let new_button = self.editor.is_none().then(|| {
            widgets::action_button(&theme, widgets::ActionTone::Outlined)
                .id("agents-new")
                .flex_none()
                .on_click(cx.listener(|page, _, _, cx| page.open_editor(None, cx)))
                .child(
                    crate::icons::icon(crate::icons::PLUS)
                        .size(px(14.0))
                        .text_color(theme.text),
                )
                .child(SharedString::from("New zoomie"))
        });
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message));
        let editor = self.render_editor(&theme, cx);
        let agents = self.render_agents(&theme, cx);
        let inbox = self.render_inbox(&theme, cx);
        let scrollbar = popover::rail(self, "agents-page-scrollbar", &theme, cx);
        div()
            .id("agents-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("agents-page")
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
                                                .child(widgets::page_header(
                                                    &theme,
                                                    "Zoomies",
                                                    (count > 0).then_some(count),
                                                ))
                                                .child(widgets::page_subtitle(
                                                    &theme,
                                                    "Zoomies run on this device's engine, even headless.",
                                                )),
                                        )
                                        .children(new_button),
                                )
                                .children(error)
                                .children(editor)
                                .child(agents)
                                .child(inbox)
                                .child(self.telegram.clone()),
                        ),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
    }
}
