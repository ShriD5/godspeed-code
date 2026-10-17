//! "Continue on another account…" on a Limited chat's banner: pick another
//! signed-in account of the same provider, or another provider, and the
//! chat's host forks it into a new chat there (`ContinueChatOnAccount`,
//! engine `account_handoff`). Nothing is automatic and nothing switches in
//! place: the original chat stays as it was, prompt cache and all, and the
//! new one opens with the continuation staged for the user to send.

use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, Chat, ContinueOnAccountReply, ContinueOnAccountRequest,
    HarnessId,
};
use zeron_rpc::methods;

use crate::account_usage::{device_params, fetch_accounts};
use crate::popover;
use crate::settings::accounts::{self, AccountsSnapshotCache, PROVIDERS, switches_accounts};
use crate::settings::widgets::{self, ActionTone};
use crate::state::AppState;
use crate::theme::Theme;

/// What the new chat's composer is staged with.
pub(crate) const CONTINUE_PROMPT: &str = "Continue where you left off.";
/// How long to wait for the new chat's row to sync before opening it anyway.
const ROW_WAIT: Duration = Duration::from_secs(2);

/// An account the chat can continue on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub account: AgentAccount,
    /// The chat's own provider (another account of it).
    pub same_provider: bool,
}

/// Whether `account` is the login `chat` already runs on: the one its limit
/// hit, else the live one (the engine refuses it the same way). Pure.
pub(crate) fn is_same_login(
    chat: &Chat,
    snapshot: &AgentAccountsSnapshot,
    account: &AgentAccount,
) -> bool {
    if chat.config.as_ref().map(|c| c.harness) != Some(account.harness) {
        return false;
    }
    let recorded = chat
        .limit
        .as_ref()
        .and_then(|limit| limit.login.as_ref())
        .filter(|login| login.harness == account.harness)
        .and_then(|login| login.fingerprint.as_deref());
    match recorded {
        Some(fingerprint) => snapshot.fingerprint_with(account).as_deref() == Some(fingerprint),
        None => account.active,
    }
}

/// The accounts `chat` can continue on: the other accounts of its own
/// provider first, then every other provider's, in Settings order. Its own
/// login and accounts Godspeed can't switch to are left out. Pure.
pub(crate) fn candidates(snapshot: &AgentAccountsSnapshot, chat: &Chat) -> Vec<Candidate> {
    let own = chat.config.as_ref().map(|c| c.harness);
    let usable = |account: &&AgentAccount| {
        !is_same_login(chat, snapshot, account)
            && (account.active || (account.switchable && switches_accounts(account.harness)))
    };
    let order = |harness: HarnessId| {
        if Some(harness) == own {
            0
        } else {
            1 + PROVIDERS
                .iter()
                .position(|(h, _, _)| *h == harness)
                .unwrap_or(PROVIDERS.len())
        }
    };
    let mut list: Vec<Candidate> = snapshot
        .accounts
        .iter()
        .filter(usable)
        .map(|account| Candidate {
            account: account.clone(),
            same_provider: Some(account.harness) == own,
        })
        .collect();
    // Stable: accounts keep the engine's order within a provider.
    list.sort_by_key(|candidate| order(candidate.account.harness));
    list
}

pub(crate) struct ContinueMenu {
    state: Entity<AppState>,
    popup: popover::Popup<()>,
    /// The account a fork is running for.
    busy: Option<String>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    task: Option<Task<()>>,
}

impl ContinueMenu {
    pub(crate) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        Self {
            state,
            popup: popover::Popup::default(),
            busy: None,
            error: None,
            load_task: None,
            task: None,
        }
    }

    /// The Limited chat and the device hosting it (`None` = this one).
    fn chat(&self, cx: &gpui::App) -> Option<(Chat, Option<String>)> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?.clone();
        let host = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        Some((chat, host))
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.popup.take_press_was_open() || self.popup.is_open() {
            self.dismiss(cx);
            return;
        }
        // The host's accounts as cached (usage served from its probe cache;
        // no forced probe).
        if let (Some((_, host)), Some(engine)) =
            (self.chat(cx), self.state.read(cx).engine().cloned())
        {
            let cached = cx
                .try_global::<AccountsSnapshotCache>()
                .is_some_and(|cache| cache.0.contains_key(&host));
            self.load_task = Some(fetch_accounts(engine, host, false, !cached, cx));
        }
        self.error = None;
        self.popup.open(());
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.popup.begin_close() {
            popover::reap_popup(cx, |menu: &mut Self| &mut menu.popup);
        }
        cx.notify();
    }

    fn continue_on(&mut self, account: AgentAccount, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some((chat, host)) = self.chat(cx) else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = ContinueOnAccountRequest {
            chat_id: uuid::Uuid::new_v4().to_string(),
            source_chat_id: chat.id.clone(),
            harness: account.harness,
            account_id: account.id.clone(),
            model: None,
        };
        let params = device_params(&host, serde_json::to_value(&request).unwrap_or_default());
        let handle = window.window_handle();
        let state = self.state.clone();
        self.busy = Some(account.id.clone());
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ContinueOnAccountReply>(methods::CONTINUE_CHAT_ON_ACCOUNT, params)
                .await;
            let reply = match result {
                Ok(reply) => reply,
                Err(error) => {
                    this.update(cx, |menu, cx| {
                        menu.busy = None;
                        menu.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            // The handoff may have switched the account: refresh the list.
            cx.update(|cx| fetch_accounts(engine, host, false, false, cx).detach());
            // Open once the row has synced, so the composer has its config.
            let id = reply.chat.id.clone();
            let started = std::time::Instant::now();
            while started.elapsed() < ROW_WAIT
                && !cx.update(|cx| state.read(cx).chats.iter().any(|c| c.id == id))
            {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
            this.update(cx, |menu, cx| {
                menu.busy = None;
                menu.dismiss(cx);
            })
            .ok();
            cx.update_window(handle, |_, window, cx| {
                window.dispatch_action(
                    Box::new(crate::usage_cockpit::OpenContinuedChat {
                        chat_id: id,
                        prompt: CONTINUE_PROMPT.into(),
                    }),
                    cx,
                );
            })
            .ok();
        }));
        cx.notify();
    }

    fn card(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let chat = self.chat(cx);
        let snapshot = chat.as_ref().and_then(|(_, host)| {
            cx.try_global::<AccountsSnapshotCache>()
                .and_then(|cache| cache.0.get(host))
                .cloned()
        });
        let list = match (&chat, &snapshot) {
            (Some((chat, _)), Some(snapshot)) => candidates(snapshot, chat),
            _ => Vec::new(),
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut last_group: Option<bool> = None;
        for (ix, candidate) in list.iter().enumerate() {
            if last_group != Some(candidate.same_provider) {
                last_group = Some(candidate.same_provider);
                rows.push(
                    popover::menu_heading(
                        theme,
                        if candidate.same_provider {
                            "Another account"
                        } else {
                            "Another provider"
                        },
                    )
                    .into_any_element(),
                );
            }
            rows.push(self.row(candidate, ix, theme, cx));
        }
        if rows.is_empty() {
            let note = if snapshot.is_none() {
                "Reading accounts…"
            } else {
                "No other signed-in account. Add one in Settings → Providers."
            };
            rows.push(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(note))
                    .into_any_element(),
            );
        }
        popover::popover_card(theme)
            .w(px(360.0))
            .flex()
            .flex_col()
            .children(rows)
            .children(self.error.clone().map(|error| {
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger)
                    .child(error)
            }))
            .child(
                div()
                    .mt(px(4.0))
                    .px(px(8.0))
                    .pb(px(6.0))
                    .text_size(px(11.5))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "Starts a new chat with this one's context; this chat stays as it is. \
                         Picking an account that isn't in use switches its provider to it.",
                    )),
            )
    }

    fn row(
        &self,
        candidate: &Candidate,
        ix: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let account = &candidate.account;
        let name: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let mut meta: Vec<AnyElement> =
            vec![SharedString::from(accounts::provider_name(account.harness)).into_any_element()];
        if let Some(plan) = &account.plan_label {
            meta.push(SharedString::from(plan.clone()).into_any_element());
        }
        if let Some(reading) = crate::usage_cockpit::five_hour(account) {
            meta.push(
                SharedString::from(format!(
                    "5-hour {}%",
                    (reading.used_fraction.clamp(0.0, 1.0) * 100.0).round() as u32
                ))
                .into_any_element(),
            );
        }
        if self.busy.as_deref() == Some(account.id.as_str()) {
            meta.push(
                div()
                    .text_color(theme.accent)
                    .child("Starting…")
                    .into_any_element(),
            );
        } else if account.active {
            meta.push(
                div()
                    .text_color(theme.accent)
                    .child("In use")
                    .into_any_element(),
            );
        }
        let target = account.clone();
        let busy = self.busy.is_some();
        popover::menu_row(theme, false, format!("continue-on-{ix}"))
            .id(("continue-on-account", ix))
            .when(busy, |row| row.opacity(0.6).cursor_default())
            .when(!busy, |row| {
                row.on_click(cx.listener(move |menu, _, window, cx| {
                    menu.continue_on(target.clone(), window, cx);
                }))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(name),
                    )
                    .child(widgets::meta_line(theme, meta)),
            )
            .into_any_element()
    }
}

impl Render for ContinueMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let open = self.popup.get().is_some();
        let trigger =
            widgets::text_action(&theme, ActionTone::Quiet, "Continue on another account…")
                .id("limit-banner-continue-elsewhere")
                .relative()
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label("Continue this chat on another account or provider")
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|menu, _, _, _| menu.popup.note_trigger_press()),
                )
                .on_click(cx.listener(|menu, _, _, cx| menu.toggle(cx)));
        if !open {
            return trigger.into_any_element();
        }
        let popup_theme = theme.for_popup();
        let card = self
            .card(&popup_theme, cx)
            .on_mouse_down_out(cx.listener(|menu, _, _, cx| menu.dismiss(cx)))
            .into_any_element();
        trigger
            .child(popover::anchored_menu_above(
                "continue-on-account-menu",
                card,
                self.popup.closing_since(),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::{ChatLimit, LimitLogin, LimitRecovery, login_fingerprint};

    fn account(id: &str, harness: HarnessId, active: bool, switchable: bool) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": harness,
            "email": null,
            "planLabel": null,
            "active": active,
            "switchable": switchable,
        }))
        .unwrap()
    }

    fn limited_chat(harness: HarnessId, fingerprint: Option<String>) -> Chat {
        let mut chat: Chat = serde_json::from_value(serde_json::json!({
            "id": "c",
            "deviceId": "d",
            "title": "t",
            "archived": false,
            "cwd": null,
            "branch": null,
            "checkoutId": null,
            "config": { "harness": harness, "model": null, "reasoning": null, "sandbox": "workspace-write" },
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": chrono::Utc::now(),
        }))
        .unwrap();
        chat.limit = Some(ChatLimit {
            limited_at: chrono::Utc::now(),
            reset_at: None,
            reset_source: None,
            window: None,
            recovery: LimitRecovery::Ask,
            login: Some(LimitLogin {
                harness,
                fingerprint,
            }),
        });
        chat
    }

    #[test]
    fn candidates_skip_the_limited_login_and_put_the_same_provider_first() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("codex", HarnessId::Codex, true, true),
                account("claude-a", HarnessId::ClaudeCode, true, true),
                account("claude-b", HarnessId::ClaudeCode, false, true),
                account("claude-locked", HarnessId::ClaudeCode, false, false),
                account("hermes", HarnessId::Hermes, false, false),
            ],
            warnings: Vec::new(),
        };
        let chat = limited_chat(
            HarnessId::ClaudeCode,
            login_fingerprint(["claude-a"].into_iter()),
        );
        let ids: Vec<(String, bool)> = candidates(&snapshot, &chat)
            .into_iter()
            .map(|c| (c.account.id, c.same_provider))
            .collect();
        assert_eq!(
            ids,
            [("claude-b".to_owned(), true), ("codex".to_owned(), false)]
        );

        // The user switched to b after the limit hit a: b, now live, is
        // still offered and a still is not.
        let mut switched = snapshot.clone();
        accounts::mark_switched(&mut switched, &snapshot.accounts[2]);
        let ids: Vec<String> = candidates(&switched, &chat)
            .into_iter()
            .map(|c| c.account.id)
            .collect();
        assert_eq!(ids, ["claude-b", "codex"]);
        // No recorded login: the live one is the chat's own.
        let unknown = limited_chat(HarnessId::ClaudeCode, None);
        let ids: Vec<String> = candidates(&snapshot, &unknown)
            .into_iter()
            .map(|c| c.account.id)
            .collect();
        assert_eq!(ids, ["claude-b", "codex"]);
    }
}
