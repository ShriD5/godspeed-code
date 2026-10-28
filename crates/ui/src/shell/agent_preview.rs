//! Agents drive the preview browser (`preview_*` MCP tools). The window
//! attaches to its engine as the preview driver (`WatchPreviewDriver`), runs
//! each request on the requesting chat's own agent tab, and answers
//! (`AnswerPreviewRequest`). The engine has already applied the URL and
//! network policy and the input rate limit; this side keeps the agent on its
//! chat's tab, out of the user's keyboard focus, and visible: the tab shows
//! "Agent is using the preview" with a Stop button while it acts.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{AppContext as _, AsyncWindowContext, Context, WeakEntity, Window};
use serde_json::{Value, json};
use zeron_proto::preview_agent::{PreviewAction, PreviewDriverAnswer, PreviewDriverRequest};
use zeron_rpc::methods;

use super::{RightSurface, Shell};
use crate::browser::{BrowserSurface, agent};

/// What an agent hears after the user pressed Stop on its tab.
pub(super) const STOPPED: &str = "The user stopped your control of this chat's preview tab. Don't use preview tools again unless the user asks; they work again after the user closes that tab.";
const NO_TAB: &str = "This chat has no preview tab open; call preview_open first.";
const CLOSED: &str = "The preview tab closed.";
/// Longest a page load may take before `preview_open` answers anyway.
const LOAD_TIMEOUT: Duration = Duration::from_secs(25);
/// The in-page action's own deadline (the engine has a longer one).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(12);
/// Between a click or keystroke and reading where the page went.
const SETTLE: Duration = Duration::from_millis(150);
const WAIT_POLL: Duration = Duration::from_millis(150);

#[derive(Default)]
pub(super) struct AgentPreviews {
    /// Each chat's agent tab (a browser id).
    tabs: HashMap<String, u64>,
    /// Tabs the user took back with Stop, by chat, until they close.
    stopped: HashMap<String, u64>,
    driver: Option<gpui::Task<()>>,
}

impl AgentPreviews {
    /// The chat whose agent tab is `browser`.
    fn chat_of(&self, browser: u64) -> Option<String> {
        self.tabs
            .iter()
            .find(|(_, id)| **id == browser)
            .map(|(chat, _)| chat.clone())
    }
}

impl Shell {
    /// Attach this window as the engine's preview driver, re-attaching
    /// whenever the engine connection changes or drops.
    pub(super) fn ensure_preview_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(test) || self.agent_previews.driver.is_some() {
            return;
        }
        self.agent_previews.driver = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(handle) = this.update(cx, |this, cx| this.state.read(cx).engine().cloned())
                else {
                    return;
                };
                if let Some(handle) = handle
                    && let Ok(mut requests) = handle
                        .client()
                        .subscribe(methods::WATCH_PREVIEW_DRIVER, json!({}))
                        .await
                {
                    while let Some(value) = requests.recv().await {
                        let Ok(request) = serde_json::from_value::<PreviewDriverRequest>(value)
                        else {
                            continue;
                        };
                        let engine = handle.clone();
                        if this
                            .update_in(cx, |_, window, cx| {
                                cx.spawn_in(window, async move |this, cx| {
                                    let id = request.id.clone();
                                    let result = perform(this, request, cx).await;
                                    let answer = match result {
                                        Ok(ok) => PreviewDriverAnswer {
                                            id,
                                            ok: Some(ok),
                                            err: None,
                                        },
                                        Err(err) => PreviewDriverAnswer {
                                            id,
                                            ok: None,
                                            err: Some(err),
                                        },
                                    };
                                    if let Ok(params) = serde_json::to_value(&answer) {
                                        let _ = engine
                                            .client()
                                            .call(methods::ANSWER_PREVIEW_REQUEST, params)
                                            .await;
                                    }
                                })
                                .detach();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_secs(2))
                    .await;
            }
        }));
    }

    /// The user pressed Stop on `browser`.
    pub(super) fn agent_preview_stopped(&mut self, browser: u64, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_previews.chat_of(browser) {
            self.agent_previews.tabs.remove(&chat);
            self.agent_previews.stopped.insert(chat, browser);
        }
        cx.notify();
    }

    /// Forget tabs that closed: a closed stopped tab hands control back.
    fn prune_agent_previews(&mut self) {
        let browsers = &self.browsers;
        self.agent_previews
            .tabs
            .retain(|_, id| browsers.contains_key(id));
        self.agent_previews
            .stopped
            .retain(|_, id| browsers.contains_key(id));
    }

    /// The chat's agent tab, for an action on the page it has open.
    pub(super) fn agent_preview_tab(
        &mut self,
        chat_id: &str,
    ) -> Result<WeakEntity<BrowserSurface>, String> {
        self.prune_agent_previews();
        if self.agent_previews.stopped.contains_key(chat_id) {
            return Err(STOPPED.into());
        }
        self.agent_previews
            .tabs
            .get(chat_id)
            .and_then(|id| self.browsers.get(id))
            .map(|browser| browser.downgrade())
            .ok_or_else(|| NO_TAB.into())
    }

    /// Where a load for `chat_id` goes: its agent tab, the chat's own
    /// browser tab to adopt (`open` only: the selected one, else the newest),
    /// or a new tab (`open` only).
    pub(super) fn agent_preview_target(
        &mut self,
        chat_id: &str,
        open: bool,
        cx: &gpui::App,
    ) -> Result<LoadTarget, String> {
        match self.agent_preview_tab(chat_id) {
            Ok(_) => {
                return Ok(LoadTarget::Tab(self.agent_previews.tabs[chat_id]));
            }
            Err(error) if error == STOPPED || !open => return Err(error),
            Err(_) => {}
        }
        let selected = self.panels.get(chat_id).right_active;
        let candidates: Vec<u64> = self
            .right_tabs
            .get(chat_id)
            .into_iter()
            .flatten()
            .filter_map(|surface| match surface {
                RightSurface::Browser(id) => Some(*id),
                _ => None,
            })
            .filter(|id| {
                self.browsers
                    .get(id)
                    .is_some_and(|browser| !browser.read(cx).agent_stopped())
            })
            .collect();
        Ok(match selected {
            RightSurface::Browser(id) if candidates.contains(&id) => LoadTarget::Adopt(id),
            _ => candidates
                .last()
                .copied()
                .map_or(LoadTarget::New, LoadTarget::Adopt),
        })
    }

    /// Load `url` in the chat's agent tab ([`Self::agent_preview_target`]).
    /// Never moves keyboard focus; in the chat on screen a new tab becomes
    /// the visible one.
    pub(super) fn agent_preview_load(
        &mut self,
        chat_id: &str,
        url: &str,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<WeakEntity<BrowserSurface>, String> {
        let id = match self.agent_preview_target(chat_id, open, cx)? {
            LoadTarget::Tab(id) | LoadTarget::Adopt(id) => {
                if let Some(browser) = self.browsers.get(&id).cloned() {
                    browser.update(cx, |browser, cx| {
                        browser.mark_agent_tab(cx);
                        browser.navigate_quietly(url, window, cx);
                    });
                }
                id
            }
            LoadTarget::New => {
                let on_screen = chat_id == self.active_chat;
                if on_screen {
                    self.set_surfaces_open(true, cx);
                }
                self.add_browser_surface_for(
                    chat_id,
                    Some(url.to_owned()),
                    on_screen,
                    true,
                    window,
                    cx,
                )
            }
        };
        self.agent_previews.tabs.insert(chat_id.to_owned(), id);
        cx.notify();
        self.browsers
            .get(&id)
            .map(|browser| browser.downgrade())
            .ok_or_else(|| CLOSED.into())
    }

    /// Record `browser` as the chat's agent tab (tests, fixtures).
    #[cfg(any(test, feature = "browser-fixture"))]
    pub(super) fn adopt_agent_preview(&mut self, chat_id: &str, browser: u64, cx: &mut Context<Self>) {
        if let Some(entity) = self.browsers.get(&browser).cloned() {
            entity.update(cx, |browser, cx| browser.mark_agent_tab(cx));
            self.agent_previews.tabs.insert(chat_id.to_owned(), browser);
        }
    }
}

/// Fixtures: run one driver request through the window's real path.
#[cfg(feature = "browser-fixture")]
impl Shell {
    pub fn fixture_agent_preview(
        &mut self,
        request: PreviewDriverRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Result<Value, String>> {
        cx.spawn_in(window, async move |this, cx| perform(this, request, cx).await)
    }

    pub fn fixture_agent_tab(&self, chat_id: &str) -> Option<gpui::Entity<BrowserSurface>> {
        self.agent_previews
            .tabs
            .get(chat_id)
            .and_then(|id| self.browsers.get(id))
            .cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoadTarget {
    Tab(u64),
    Adopt(u64),
    New,
}

async fn timer(cx: &mut AsyncWindowContext, duration: Duration) {
    cx.background_executor().timer(duration).await;
}

/// Run one request; the error is the message the agent reads.
async fn perform(
    shell: WeakEntity<Shell>,
    request: PreviewDriverRequest,
    cx: &mut AsyncWindowContext,
) -> Result<Value, String> {
    let gone = |_| "The Godspeed window closed.".to_owned();
    let chat = request.chat_id.clone();
    let browser = match &request.action {
        PreviewAction::Open { url } | PreviewAction::Navigate { url } => {
            let open = matches!(request.action, PreviewAction::Open { .. });
            let url = url.clone();
            shell
                .update_in(cx, |shell, window, cx| {
                    shell.agent_preview_load(&chat, &url, open, window, cx)
                })
                .map_err(gone)??
        }
        _ => shell
            .update(cx, |shell, _| shell.agent_preview_tab(&chat))
            .map_err(gone)??,
    };
    browser
        .update(cx, |browser, cx| browser.agent_activity(true, cx))
        .map_err(|_| CLOSED.to_owned())?;
    let result = act(&browser, &request.action, cx).await;
    let _ = browser.update(cx, |browser, cx| browser.agent_activity(false, cx));
    // Stop pressed while the action ran: don't hand the agent the result.
    if browser
        .read_with(cx, |browser, _| browser.agent_stopped())
        .unwrap_or(true)
    {
        return Err(STOPPED.into());
    }
    result
}

/// The tab's page state as the agent sees it.
fn page_summary(browser: &BrowserSurface) -> Value {
    json!({
        "url": browser.page.url,
        "title": browser.page.title,
        "loading": browser.page.loading,
    })
}

async fn act(
    browser: &WeakEntity<BrowserSurface>,
    action: &PreviewAction,
    cx: &mut AsyncWindowContext,
) -> Result<Value, String> {
    match action {
        PreviewAction::Open { .. } | PreviewAction::Navigate { .. } => wait_for_load(browser, cx).await,
        PreviewAction::Screenshot => {
            let bytes = capture(browser, cx).await?;
            let (png, width, height) = cx
                .background_spawn(async move { agent::encode_screenshot(&bytes) })
                .await?;
            let page = browser
                .read_with(cx, |browser, _| page_summary(browser))
                .map_err(|_| CLOSED.to_owned())?;
            Ok(json!({
                "png": png,
                "width": width,
                "height": height,
                "url": page["url"],
                "title": page["title"],
            }))
        }
        PreviewAction::Wait { .. } => {
            let script = agent::action_script(action).ok_or("unsupported action")?;
            let timeout = Duration::from_millis(action.wait_timeout_ms());
            let started = std::time::Instant::now();
            loop {
                // A page mid-navigation can't answer; keep polling.
                let present = evaluate(browser, &script, cx)
                    .await
                    .ok()
                    .and_then(|value| value.get("present").and_then(Value::as_bool))
                    .unwrap_or(false);
                let waited = started.elapsed();
                if present || waited >= timeout {
                    let page = browser
                        .read_with(cx, |browser, _| page_summary(browser))
                        .map_err(|_| CLOSED.to_owned())?;
                    return Ok(json!({
                        "found": present,
                        "waitedMs": waited.as_millis() as u64,
                        "url": page["url"],
                    }));
                }
                timer(cx, WAIT_POLL).await;
            }
        }
        other => {
            let script = agent::action_script(other).ok_or("unsupported action")?;
            let value = evaluate(browser, &script, cx).await?;
            match other {
                PreviewAction::Snapshot => Ok(agent::finish_snapshot(value)),
                PreviewAction::Click { .. } | PreviewAction::Type { .. } => {
                    // Let a click's navigation or re-render start, then say
                    // where the page is.
                    timer(cx, SETTLE).await;
                    let page = browser
                        .read_with(cx, |browser, _| page_summary(browser))
                        .map_err(|_| CLOSED.to_owned())?;
                    let mut value = value;
                    if let Some(map) = value.as_object_mut() {
                        map.insert("page".into(), page);
                    }
                    Ok(value)
                }
                _ => Ok(value),
            }
        }
    }
}

/// Wait for the tab's load to finish (or fail).
async fn wait_for_load(
    browser: &WeakEntity<BrowserSurface>,
    cx: &mut AsyncWindowContext,
) -> Result<Value, String> {
    let started = std::time::Instant::now();
    loop {
        let (loading, error, summary) = browser
            .read_with(cx, |browser, _| {
                (
                    browser.page.loading,
                    browser.page.error.clone(),
                    page_summary(browser),
                )
            })
            .map_err(|_| CLOSED.to_owned())?;
        if let Some(error) = error {
            return Err(format!(
                "The page failed to load: {error} ({})",
                summary["url"].as_str().unwrap_or_default()
            ));
        }
        if !loading {
            browser
                .read_with(cx, |browser, _| browser.agent_ready())
                .map_err(|_| CLOSED.to_owned())??;
            return Ok(summary);
        }
        if started.elapsed() >= LOAD_TIMEOUT {
            let mut summary = summary;
            summary["note"] = json!("Still loading; try preview_wait or preview_snapshot.");
            return Ok(summary);
        }
        timer(cx, Duration::from_millis(100)).await;
    }
}

async fn evaluate(
    browser: &WeakEntity<BrowserSurface>,
    script: &str,
    cx: &mut AsyncWindowContext,
) -> Result<Value, String> {
    let (tx, rx) = futures::channel::oneshot::channel();
    browser
        .update(cx, |browser, _| {
            browser.agent_evaluate(
                script,
                Box::new(move |result| {
                    let _ = tx.send(result);
                }),
            )
        })
        .map_err(|_| CLOSED.to_owned())?;
    let deadline = cx.background_executor().timer(SCRIPT_TIMEOUT);
    match futures::future::select(rx, deadline).await {
        futures::future::Either::Left((Ok(raw), _)) => agent::parse_result(raw),
        futures::future::Either::Left((Err(_), _)) => Err(CLOSED.into()),
        futures::future::Either::Right(_) => Err("The page did not answer in time.".into()),
    }
}

async fn capture(
    browser: &WeakEntity<BrowserSurface>,
    cx: &mut AsyncWindowContext,
) -> Result<Vec<u8>, String> {
    let (tx, rx) = futures::channel::oneshot::channel();
    browser
        .update(cx, |browser, _| {
            browser.agent_capture(Box::new(move |result| {
                let _ = tx.send(result);
            }))
        })
        .map_err(|_| CLOSED.to_owned())?;
    let deadline = cx.background_executor().timer(SCRIPT_TIMEOUT);
    match futures::future::select(rx, deadline).await {
        futures::future::Either::Left((Ok(result), _)) => {
            result.map_err(|error| format!("Could not capture the preview: {error}"))
        }
        futures::future::Either::Left((Err(_), _)) => Err(CLOSED.into()),
        futures::future::Either::Right(_) => Err(
            "The preview did not render a screenshot in time (is the chat's preview hidden?)."
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, EngineBootConfig};
    use crate::theme::Theme;
    use gpui::TestAppContext;

    #[gpui::test]
    fn agents_get_their_chats_tab_and_stop_takes_it_back(cx: &mut TestAppContext) {
        let _platform = crate::platform_turn();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        let user_tab = window
            .update(cx, |shell, window, cx| {
                shell.active_chat = "chat-a".into();
                // No tab yet: navigate refuses, open makes one.
                assert_eq!(shell.agent_preview_tab("chat-a").unwrap_err(), NO_TAB);
                assert_eq!(
                    shell.agent_preview_target("chat-a", false, cx).unwrap_err(),
                    NO_TAB
                );
                assert_eq!(
                    shell.agent_preview_target("chat-a", true, cx).unwrap(),
                    LoadTarget::New
                );
                // The chat's own browser tab is adopted; another chat's never is.
                shell.add_browser_surface(None, window, cx);
                let user_tab = shell.browser_seq;
                assert_eq!(
                    shell.agent_preview_target("chat-a", true, cx).unwrap(),
                    LoadTarget::Adopt(user_tab)
                );
                assert_eq!(
                    shell.agent_preview_target("chat-b", true, cx).unwrap(),
                    LoadTarget::New
                );
                shell.adopt_agent_preview("chat-a", user_tab, cx);
                assert_eq!(
                    shell.agent_preview_target("chat-a", false, cx).unwrap(),
                    LoadTarget::Tab(user_tab)
                );
                assert_eq!(shell.agent_preview_tab("chat-b").unwrap_err(), NO_TAB);
                // The strip shows while the agent acts and lingers after.
                let browser = shell.browsers[&user_tab].clone();
                browser.update(cx, |browser, cx| {
                    assert!(browser.agent_owned() && !browser.agent_using());
                    browser.agent_activity(true, cx);
                    assert!(browser.agent_using());
                    browser.agent_activity(false, cx);
                    assert!(browser.agent_using(), "lingers after the action");
                    browser.stop_agent(cx);
                    assert!(!browser.agent_using() && browser.agent_stopped());
                });
                user_tab
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, window, cx| {
                // Stopped: every agent action refuses until the tab closes.
                assert_eq!(shell.agent_preview_tab("chat-a").unwrap_err(), STOPPED);
                assert_eq!(
                    shell.agent_preview_target("chat-a", true, cx).unwrap_err(),
                    STOPPED
                );
                shell.close_right_surface(RightSurface::Browser(user_tab), window, cx);
                assert_eq!(
                    shell.agent_preview_target("chat-a", true, cx).unwrap(),
                    LoadTarget::New
                );
            })
            .unwrap();
    }
}
