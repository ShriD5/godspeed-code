//! The composer footer's gauge cluster. The tachometer
//! ([`crate::tachometer`]) shows how much of the active account's 5-hour
//! limit the session's harness has used, beside the context ring. Clicking
//! it opens Settings → Usage ([`crate::usage_cockpit`]), where every
//! account's meters live and where accounts are switched. Both views share
//! [`AccountsSnapshotCache`], so a switch in either shows up in the other.
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, IntoElement, Render, Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, HarnessId};
use zeron_rpc::methods;

use crate::popover;
use crate::settings::accounts::{self, AccountsSnapshotCache, reports_usage, signs_in};
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::tachometer::Tachometer;
use crate::theme::Theme;

/// Forced probes hit the provider; the engine throttles them too, but the
/// gauge re-probes on every hover, so don't even ask more often than this.
pub(crate) const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// Background re-probe while a composer is alive: usage moves as turns run.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The binding limit: the most-used window of the account. Pure.
pub fn used_fraction(account: &AgentAccount) -> Option<f32> {
    account
        .usage_windows
        .iter()
        .map(|window| window.used_fraction.clamp(0.0, 1.0))
        .reduce(f32::max)
}

/// The harness's live account, when it reports usage. Pure.
pub fn active_account(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Option<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .find(|account| account.harness == harness && account.active)
}

/// `params` addressed to `target` (`None` = this device).
pub(crate) fn device_params(
    target: &Option<String>,
    mut params: serde_json::Value,
) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    params
}

/// Load `target`'s accounts into [`AccountsSnapshotCache`]. With
/// `paint_first`, a plain list goes first (the engine's persisted usage
/// paints at once); with `force_usage`, the probing list replaces it.
pub(crate) fn fetch_accounts(
    engine: EngineHandle,
    target: Option<String>,
    force_usage: bool,
    paint_first: bool,
    cx: &mut App,
) -> Task<()> {
    let plain = device_params(&target, serde_json::json!({ "forceUsage": false }));
    let params = device_params(&target, serde_json::json!({ "forceUsage": force_usage }));
    cx.spawn(async move |cx| {
        let fetch = |params: serde_json::Value| {
            let engine = engine.clone();
            async move {
                engine
                    .client()
                    .call(methods::LIST_AGENT_ACCOUNTS, params)
                    .await
                    .ok()
                    .and_then(|value| serde_json::from_value::<AgentAccountsSnapshot>(value).ok())
            }
        };
        let store = |snapshot: AgentAccountsSnapshot, cx: &mut gpui::AsyncApp| {
            let key = target.clone();
            cx.update(|cx| {
                cx.default_global::<AccountsSnapshotCache>()
                    .0
                    .insert(key, snapshot);
            });
        };
        if paint_first && let Some(snapshot) = fetch(plain).await {
            store(snapshot, cx);
        }
        if let Some(snapshot) = fetch(params).await {
            store(snapshot, cx);
        }
    })
}

/// Switch `account` live on `target` optimistically, like Settings →
/// Accounts: the cached rows flip at once and the engine's reply replaces
/// them; a refusal restores the list and resolves to the reason.
pub(crate) fn switch_account(
    engine: EngineHandle,
    target: Option<String>,
    account: &AgentAccount,
    cx: &mut App,
) -> Task<Result<(), String>> {
    let cache = &mut cx.default_global::<AccountsSnapshotCache>().0;
    let previous = cache.get(&target).cloned();
    if let Some(snapshot) = cache.get_mut(&target) {
        // Per-provider agents keep one live login per provider: only the
        // switched-to row's group changes.
        accounts::mark_switched(snapshot, account);
    }
    let params = device_params(
        &target,
        serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }),
    );
    cx.spawn(async move |cx| {
        let result = engine
            .client()
            .call(methods::ACTIVATE_AGENT_ACCOUNT, params)
            .await;
        cx.update(
            |cx| match result.map(serde_json::from_value::<AgentAccountsSnapshot>) {
                Ok(Ok(snapshot)) => {
                    cx.default_global::<AccountsSnapshotCache>()
                        .0
                        .insert(target, snapshot);
                    Ok(())
                }
                Ok(Err(_)) => {
                    fetch_accounts(engine, target, false, false, cx).detach();
                    Ok(())
                }
                Err(err) => {
                    if let Some(previous) = previous {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(target, previous);
                    }
                    Err(err.to_string())
                }
            },
        )
    })
}

/// Loads the accounts of the composer's target device and drives the gauge.
pub struct AccountUsage {
    state: Entity<AppState>,
    /// `None` = this device, as in [`AccountsSnapshotCache`].
    target: Option<String>,
    harness: Option<HarnessId>,
    /// Once any list has been asked for the current target.
    loaded: bool,
    last_forced: Option<Instant>,
    load_task: Option<Task<()>>,
    popup: popover::Popup<FooterCard>,
    tachometer: Tachometer,
    /// The chat whose turn state the gauge last saw, and whether a turn was
    /// running: a fresh turn revs the needle.
    last_turn: Option<(String, bool)>,
    _poll: Task<()>,
    _cache: Subscription,
    _state: Subscription,
}

impl AccountUsage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update(cx, |usage, cx| {
                        if usage.harness.is_some() {
                            usage.load(true, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            _state: cx.observe(&state, |_, _, cx| cx.notify()),
            state,
            target: None,
            harness: None,
            loaded: false,
            last_forced: None,
            load_task: None,
            popup: popover::Popup::default(),
            tachometer: Tachometer::default(),
            last_turn: None,
            _poll: poll,
            // Settings → Usage and Accounts write the same cache.
            _cache: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
        }
    }

    /// Point at the session's harness and device; loads on first sight of a
    /// device. Cheap enough to call every render.
    pub fn track(
        &mut self,
        harness: Option<HarnessId>,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let harness = harness.filter(|h| signs_in(*h) && reports_usage(*h));
        self.harness = harness;
        if self.target != target {
            self.target = target;
            self.loaded = false;
            self.last_forced = None;
        }
        if harness.is_some() && !self.loaded {
            self.loaded = true;
            self.load(true, cx);
        }
    }

    fn snapshot<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccountsSnapshot> {
        cx.try_global::<AccountsSnapshotCache>()?
            .0
            .get(&self.target)
    }

    /// Plain list first when nothing is cached, then the forced probe
    /// replaces it ([`fetch_accounts`]).
    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        if force_usage {
            if self
                .last_forced
                .is_some_and(|at| at.elapsed() < FORCE_MIN_INTERVAL)
            {
                return;
            }
            self.last_forced = Some(Instant::now());
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let paint_first = force_usage && self.snapshot(cx).is_none();
        self.load_task = Some(fetch_accounts(
            engine,
            self.target.clone(),
            force_usage,
            paint_first,
            cx,
        ));
    }

    /// The gauge's account: the harness's live one, when it has usage.
    fn account<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccount> {
        active_account(self.snapshot(cx)?, self.harness?)
            .filter(|account| !account.usage_windows.is_empty())
    }

    /// Whether a turn just started in the composer's chat (a rev).
    fn turn_started(&mut self, cx: &gpui::App) -> bool {
        let state = self.state.read(cx);
        let Some(chat) = state.selected_chat.clone() else {
            self.last_turn = None;
            return false;
        };
        let working = state
            .session_for(&chat)
            .is_some_and(|session| matches!(session.status, zeron_proto::SessionStatus::Working));
        let started = matches!(&self.last_turn, Some((seen, false)) if *seen == chat) && working;
        self.last_turn = Some((chat, working));
        started
    }

    /// A trigger click: open this ring's popover, or close it when the
    /// press found it open (the card's mouse-down-out already began that
    /// close — see `Popup::note_trigger_press`).
    fn toggle(&mut self, card: FooterCard, cx: &mut Context<Self>) {
        if self.popup.take_press_was_open() || self.popup.as_open() == Some(&card) {
            self.dismiss(cx);
            return;
        }
        self.popup.open(card);
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.popup.begin_close() {
            popover::reap_popup(cx, |usage: &mut Self| &mut usage.popup);
        }
        cx.notify();
    }

    /// A footer ring that opens `card` above itself, right-aligned like the
    /// model picker.
    fn trigger(
        &self,
        chip: gpui::Stateful<gpui::Div>,
        card: FooterCard,
        content: impl FnOnce(&Self, &mut Context<Self>) -> gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let chip = chip
            .relative()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |usage, _, _, _| {
                    usage
                        .popup
                        .note_trigger_press_matching(|open| *open == card);
                }),
            )
            .on_click(cx.listener(move |usage, _, _, cx| usage.toggle(card, cx)));
        if self.popup.get() != Some(&card) {
            return chip.into_any_element();
        }
        let content = content(self, cx)
            .on_mouse_down_out(cx.listener(|usage, _, _, cx| usage.dismiss(cx)))
            .into_any_element();
        chip.child(popover::anchored_menu_above_end(
            card.id(),
            content,
            self.popup.closing_since(),
        ))
        .into_any_element()
    }

    /// The tachometer chip: hover for the numbers, click for the cockpit.
    fn tachometer_chip(
        &mut self,
        account: &AgentAccount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let reading = crate::usage_cockpit::gauge_reading(account).unwrap_or(0.0);
        let reduce_motion = cx.reduce_motion();
        if self.turn_started(cx) {
            self.tachometer.rev(reduce_motion);
        }
        let (position, moving) = self.tachometer.frame(reading, reduce_motion);
        if moving {
            window.request_animation_frame();
        }
        let theme = Theme::of(cx);
        let tooltip = crate::usage_cockpit::gauge_tooltip(account, chrono::Utc::now());
        crate::tachometer::chip(
            "account-usage",
            position,
            reading,
            format!("{}%", (reading * 100.0).round() as u32),
            false,
            theme,
        )
        .role(gpui::Role::Button)
        .aria_label("Usage: open the usage cockpit")
        .tooltip(widgets::text_tooltip_above(tooltip))
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(crate::usage_cockpit::OpenUsageCockpit), cx);
        })
        .into_any_element()
    }
}

/// Which footer ring's popover is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCard {
    Context,
}

impl FooterCard {
    fn id(self) -> &'static str {
        match self {
            FooterCard::Context => "context-usage-menu",
        }
    }
}

/// The footer's gauge cluster: the account tachometer, then context
/// occupancy (which opens its popover on click).
impl Render for AccountUsage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let context = self.state.read(cx).context_usage;
        let account = self
            .account(cx)
            .cloned()
            .map(|account| self.tachometer_chip(&account, window, cx));
        let context = crate::context_usage::has_window(context).then(|| {
            let chip = crate::context_usage::chip(
                context,
                self.popup.get() == Some(&FooterCard::Context),
                &theme,
            );
            self.trigger(
                chip,
                FooterCard::Context,
                move |_, cx| crate::context_usage::card(context, &Theme::of(cx).for_popup()),
                cx,
            )
        });
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(account)
            .children(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AgentUsageWindow;

    fn account(harness: HarnessId, active: bool, used: &[f32]) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": format!("{harness:?}-{active}"),
            "harness": harness,
            "email": null,
            "planLabel": null,
            "active": active,
            "switchable": true,
            "usageWindows": used
                .iter()
                .map(|fraction| AgentUsageWindow {
                    label: "5h".into(),
                    used_fraction: *fraction,
                    resets_at: None,
                })
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn ring_shows_the_most_used_window() {
        assert_eq!(used_fraction(&account(HarnessId::Codex, true, &[])), None);
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[0.12, 0.64])),
            Some(0.64)
        );
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[1.4])),
            Some(1.0)
        );
    }

    #[test]
    fn active_account_is_scoped_to_the_harness() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account(HarnessId::ClaudeCode, true, &[0.3]),
                account(HarnessId::Codex, false, &[0.1]),
                account(HarnessId::Codex, true, &[0.2]),
            ],
            warnings: Vec::new(),
        };
        assert_eq!(
            active_account(&snapshot, HarnessId::Codex).map(|a| a.id.as_str()),
            Some("Codex-true")
        );
        assert!(active_account(&snapshot, HarnessId::Cursor).is_none());
    }
}
