//! Settings → Usage, the usage cockpit: every signed-in account of every
//! provider on this device in one place, with its live 5-hour and weekly
//! meters, when each resets, its plan, and the chats Limited on it. The
//! composer's tachometer opens it; accounts switch from here.
//!
//! It reads the accounts list the probes already fill
//! ([`AccountsSnapshotCache`]): opening the page forces one probe (throttled
//! like the composer's), the composer's background poll keeps it current,
//! and nothing here ever spends model tokens.
//!
//! Below the meters, the history ([`crate::usage_history`]): tokens and
//! estimated API cost per day, by model, provider, project and chat.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::{
    Action, AnyElement, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, AgentUsageWindow, Chat, HarnessId};

use crate::account_usage::{FORCE_MIN_INTERVAL, fetch_accounts, switch_account};
use crate::composer::limit_banner::local_moment_label;
use crate::popover;
use crate::settings::accounts::{
    self, AccountsSnapshotCache, PROVIDERS, UsageLevel, switches_accounts, usage_color, usage_level,
};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;
use crate::usage_history::{UsageHistoryEvent, UsageHistoryView};

/// Open Settings → Usage (the composer's tachometer).
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct OpenUsageCockpit;

/// Open a chat just continued on another account, its composer staged with
/// the continuation (not sent).
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct OpenContinuedChat {
    pub chat_id: String,
    pub prompt: String,
}

/// How often the page re-renders its "resets in" countdowns. No fetch.
const COUNTDOWN_TICK: Duration = Duration::from_secs(30);
const METER_BAR_WIDTH: f32 = 112.0;
const METER_LABEL_WIDTH: f32 = 52.0;

// ---------------------------------------------------------------------------
// Pure: windows, resets, assembly
// ---------------------------------------------------------------------------

/// Which limit a usage window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// The rolling 5-hour session window (Claude, Codex).
    FiveHour,
    Weekly,
    Other,
}

/// Classify a probe's window label ("Session" is the engine's name for
/// the 5-hour window). Pure.
pub fn window_kind(label: &str) -> WindowKind {
    match label.trim().to_ascii_lowercase().as_str() {
        "session" | "5h" | "5-hour" | "five_hour" => WindowKind::FiveHour,
        "week" | "weekly" | "7-day" | "seven_day" => WindowKind::Weekly,
        _ => WindowKind::Other,
    }
}

/// The label a window shows under: "5-hour", "Weekly", or the probe's own.
pub fn window_label(window: &AgentUsageWindow) -> String {
    match window_kind(&window.label) {
        WindowKind::FiveHour => "5-hour".into(),
        WindowKind::Weekly => "Weekly".into(),
        WindowKind::Other => window.label.clone(),
    }
}

pub fn five_hour(account: &AgentAccount) -> Option<&AgentUsageWindow> {
    account
        .usage_windows
        .iter()
        .find(|w| window_kind(&w.label) == WindowKind::FiveHour)
}

pub fn weekly(account: &AgentAccount) -> Option<&AgentUsageWindow> {
    account
        .usage_windows
        .iter()
        .find(|w| window_kind(&w.label) == WindowKind::Weekly)
}

/// The account's windows in cockpit order: 5-hour, weekly, then the rest
/// as the probe listed them. Pure.
pub fn ordered_windows(account: &AgentAccount) -> Vec<&AgentUsageWindow> {
    let rank = |w: &&AgentUsageWindow| match window_kind(&w.label) {
        WindowKind::FiveHour => 0,
        WindowKind::Weekly => 1,
        WindowKind::Other => 2,
    };
    let mut windows: Vec<_> = account.usage_windows.iter().collect();
    windows.sort_by_key(rank);
    windows
}

/// What the tachometer reads: the 5-hour window, or for a provider without
/// one (Cursor's month, Copilot's premium requests) its most-used window.
pub fn gauge_reading(account: &AgentAccount) -> Option<f32> {
    five_hour(account)
        .map(|w| w.used_fraction.clamp(0.0, 1.0))
        .or_else(|| crate::account_usage::used_fraction(account))
}

/// "resets in 2h 14m" (minutes under an hour, days and hours past a day);
/// "resets now" once due. Pure given `now`.
pub fn resets_in(resets_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (resets_at - now).num_seconds().max(0);
    let minutes = (seconds + 59) / 60;
    if minutes <= 0 {
        return "resets now".into();
    }
    let (days, hours, mins) = (minutes / 1_440, (minutes % 1_440) / 60, minutes % 60);
    if days > 0 {
        if hours > 0 {
            format!("resets in {days}d {hours}h")
        } else {
            format!("resets in {days}d")
        }
    } else if hours > 0 {
        if mins > 0 {
            format!("resets in {hours}h {mins}m")
        } else {
            format!("resets in {hours}h")
        }
    } else {
        format!("resets in {mins}m")
    }
}

/// One window as a line: "5-hour 64% · resets in 2h 14m (3:40 PM)".
pub fn window_summary(window: &AgentUsageWindow, now: DateTime<Utc>) -> String {
    let percent = (window.used_fraction.clamp(0.0, 1.0) * 100.0).round() as u32;
    let mut line = format!("{} {percent}%", window_label(window));
    if let Some(at) = window.resets_at {
        line.push_str(&format!(
            " · {} ({})",
            resets_in(at, now),
            local_moment_label(at, now)
        ));
    }
    line
}

/// The tachometer's hover: each window on its own line, 5-hour first.
pub fn gauge_tooltip(account: &AgentAccount, now: DateTime<Utc>) -> String {
    let mut lines: Vec<String> = ordered_windows(account)
        .into_iter()
        .map(|w| window_summary(w, now))
        .collect();
    if let Some(who) = account.email.as_ref().or(account.display_name.as_ref()) {
        lines.push(who.clone());
    }
    lines.push("Click for every account's usage".into());
    lines.join("\n")
}

/// A chat stopped on a limit, as the cockpit lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitedChat {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CockpitAccount {
    pub account: AgentAccount,
    /// Chats whose limit hit this login.
    pub limited: Vec<LimitedChat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CockpitProvider {
    pub harness: HarnessId,
    pub name: &'static str,
    pub accounts: Vec<CockpitAccount>,
    /// Limited chats of this provider whose login matches no listed account
    /// (signed out since, or never recorded).
    pub unattributed: Vec<LimitedChat>,
}

fn limited_chat(chat: &Chat) -> LimitedChat {
    LimitedChat {
        id: chat.id.clone(),
        title: chat
            .title
            .clone()
            .or_else(|| chat.last_message_preview.clone())
            .unwrap_or_else(|| "New session".into()),
    }
}

/// The cockpit's rows: providers in Settings order, each with its accounts
/// (engine order) and the Limited chats mapped to the login their limit
/// hit. Only chats hosted on `device` count (accounts are per device);
/// archived chats are left out. Pure.
pub fn assemble(
    snapshot: &AgentAccountsSnapshot,
    chats: &[Chat],
    device: Option<&str>,
) -> Vec<CockpitProvider> {
    let mut providers: Vec<CockpitProvider> = PROVIDERS
        .iter()
        .map(|(harness, name, _)| CockpitProvider {
            harness: *harness,
            name,
            accounts: accounts::provider_accounts(snapshot, *harness)
                .into_iter()
                .map(|account| CockpitAccount {
                    account: account.clone(),
                    limited: Vec::new(),
                })
                .collect(),
            unattributed: Vec::new(),
        })
        .collect();
    let limited = chats.iter().filter(|chat| {
        chat.limit.is_some() && !chat.archived && device.is_none_or(|d| chat.device_id == d)
    });
    for chat in limited {
        let limit = chat.limit.as_ref().expect("filtered");
        let harness = limit
            .login
            .as_ref()
            .map(|login| login.harness)
            .or_else(|| chat.config.as_ref().map(|c| c.harness));
        let Some(provider) = providers.iter_mut().find(|p| Some(p.harness) == harness) else {
            continue;
        };
        let fingerprint = limit.login.as_ref().and_then(|l| l.fingerprint.as_deref());
        let owner = fingerprint.and_then(|fingerprint| {
            provider
                .accounts
                .iter_mut()
                .find(|row| snapshot.fingerprint_with(&row.account).as_deref() == Some(fingerprint))
        });
        match owner {
            Some(row) => row.limited.push(limited_chat(chat)),
            None => provider.unattributed.push(limited_chat(chat)),
        }
    }
    providers.retain(|p| !p.accounts.is_empty() || !p.unattributed.is_empty());
    providers
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

pub enum UsagePageEvent {
    OpenChat(String),
}

pub struct UsagePage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    last_forced: Option<Instant>,
    load_task: Option<Task<()>>,
    switch_task: Option<Task<()>>,
    /// The account a switch is waiting on.
    switching: Option<String>,
    error: Option<SharedString>,
    history: Entity<UsageHistoryView>,
    _tick: Task<()>,
    _cache: Subscription,
    _state: Subscription,
    _history: Subscription,
}

impl EventEmitter<UsagePageEvent> for UsagePage {}

impl UsagePage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(COUNTDOWN_TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |page: &mut Self, _, cx| {
            if page.last_forced.is_none() {
                page.load(true, cx);
            }
            cx.notify();
        });
        let history = cx.new(|cx| UsageHistoryView::new(state.clone(), cx));
        let history_sub = cx.subscribe(&history, |_, _, event: &UsageHistoryEvent, cx| match event {
            UsageHistoryEvent::OpenChat(id) => cx.emit(UsagePageEvent::OpenChat(id.clone())),
        });
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            last_forced: None,
            load_task: None,
            switch_task: None,
            switching: None,
            error: None,
            history,
            _tick: tick,
            _cache: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
            _state: state_sub,
            _history: history_sub,
        };
        page.load(true, cx);
        page
    }

    fn snapshot<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccountsSnapshot> {
        cx.try_global::<AccountsSnapshotCache>()?.0.get(&None)
    }

    fn load(&mut self, force: bool, cx: &mut Context<Self>) {
        if force
            && self
                .last_forced
                .is_some_and(|at| at.elapsed() < FORCE_MIN_INTERVAL)
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if force {
            self.last_forced = Some(Instant::now());
        }
        let paint_first = self.snapshot(cx).is_none();
        self.load_task = Some(fetch_accounts(engine, None, force, paint_first, cx));
    }

    fn switch(&mut self, account: AgentAccount, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.switching = Some(account.id.clone());
        let task = switch_account(engine, None, &account, cx);
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |page, cx| {
                page.switching = None;
                if let Err(error) = result {
                    page.error = Some(format!("Couldn't switch accounts: {error}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    /// One meter line: label, bar, percent, and when it resets (local time
    /// on hover).
    fn meter(
        window: &AgentUsageWindow,
        key: SharedString,
        now: DateTime<Utc>,
        theme: &Theme,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let level = usage_level(fraction);
        let fill = usage_color(level, theme).opacity(match level {
            UsageLevel::Normal => 0.8,
            _ => 0.9,
        });
        let reset = window.resets_at.map(|at| resets_in(at, now));
        div()
            .id(key)
            .h(px(18.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(crate::typography::ui_rems(11.5))
            .when_some(window.resets_at, |el, at| {
                el.tooltip(widgets::text_tooltip(format!(
                    "{} resets at {}",
                    window_label(window),
                    local_moment_label(at, now)
                )))
            })
            .child(
                div()
                    .w(px(METER_LABEL_WIDTH))
                    .flex_none()
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(window_label(window))),
            )
            .child(
                div()
                    .w(px(METER_BAR_WIDTH))
                    .flex_none()
                    .h(px(5.0))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(theme.wash(0.08))
                    .when(fraction > 0.0, |el| {
                        el.child(
                            div()
                                .h_full()
                                .w(gpui::relative(fraction.max(0.015)))
                                .rounded_full()
                                .bg(fill),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(34.0))
                    .flex_none()
                    .text_right()
                    .text_color(match level {
                        UsageLevel::Normal => theme.text,
                        _ => usage_color(level, theme),
                    })
                    .child(SharedString::from(format!(
                        "{}%",
                        (fraction * 100.0).round() as u32
                    ))),
            )
            .child(
                div()
                    .w(px(108.0))
                    .flex_none()
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(reset.unwrap_or_default())),
            )
            .into_any_element()
    }

    /// The Limited chats under an account, each opening its chat.
    fn limited_rows(
        &self,
        chats: &[LimitedChat],
        key: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if chats.is_empty() {
            return None;
        }
        Some(
            div()
                .mt(px(6.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(chats.iter().enumerate().map(|(ix, chat)| {
                    let id = chat.id.clone();
                    div()
                        .id(SharedString::from(format!("usage-limited-{key}-{ix}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .cursor_pointer()
                        .rounded(px(6.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                        .tab_index(0)
                        .role(gpui::Role::Link)
                        .focus_visible(|s| s.border_1().border_color(theme.accent))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(UsagePageEvent::OpenChat(id.clone()));
                        }))
                        .child(
                            crate::icons::icon(crate::icons::CLOCK_CIRCLE)
                                .size(px(12.0))
                                .flex_none()
                                .text_color(theme.limited),
                        )
                        .child(div().flex_none().text_color(theme.limited).child("Limited"))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(chat.title.clone())),
                        )
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }

    fn account_row(
        &self,
        row: &CockpitAccount,
        first: bool,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let account = &row.account;
        let name: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(plan) = &account.plan_label {
            meta.push(SharedString::from(plan.clone()).into_any_element());
        }
        if let Some(error) = &account.usage_error {
            meta.push(SharedString::from(error.clone()).into_any_element());
        } else if account.usage_windows.is_empty() && accounts::reports_usage(account.harness) {
            meta.push(SharedString::from("No usage reported yet").into_any_element());
        }
        let key = account.id.clone();
        let action: AnyElement = if account.active {
            widgets::badge_active(theme, "In use").into_any_element()
        } else if account.switchable && switches_accounts(account.harness) {
            let busy = self.switching.is_some();
            let target = account.clone();
            widgets::text_action(
                theme,
                widgets::ActionTone::Outlined,
                if self.switching.as_deref() == Some(account.id.as_str()) {
                    "Switching…"
                } else {
                    "Switch"
                },
            )
            .id(SharedString::from(format!("usage-switch-{key}")))
            .flex_none()
            .tab_index(0)
            .role(gpui::Role::Button)
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .when(busy, |el| el.opacity(0.5).cursor_default())
            .when(!busy, |el| {
                el.on_click(cx.listener(move |page, _, _, cx| page.switch(target.clone(), cx)))
            })
            .into_any_element()
        } else {
            div().into_any_element()
        };
        let windows = ordered_windows(account);
        widgets::card_row(theme, first)
            .items_start()
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.0))
                    .child(widgets::row_title(theme, name))
                    .when(!meta.is_empty(), |el| {
                        el.child(widgets::meta_line(theme, meta))
                    })
                    .children(self.limited_rows(&row.limited, &key, theme, cx)),
            )
            .when(!windows.is_empty(), |el| {
                el.child(div().flex_none().flex().flex_col().gap(px(2.0)).children(
                    windows.iter().enumerate().map(|(ix, window)| {
                        Self::meter(
                            window,
                            SharedString::from(format!("usage-meter-{key}-{ix}")),
                            now,
                            theme,
                        )
                    }),
                ))
            })
            .child(
                div()
                    .flex_none()
                    .w(px(96.0))
                    .flex()
                    .justify_end()
                    .child(action),
            )
            .into_any_element()
    }

    fn provider_section(
        &self,
        provider: &CockpitProvider,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut card = widgets::section_card(theme).mt_0();
        for (ix, row) in provider.accounts.iter().enumerate() {
            card = card.child(self.account_row(row, ix == 0, now, theme, cx));
        }
        if !provider.unattributed.is_empty() {
            let key = format!("{:?}-unattributed", provider.harness);
            card = card.child(
                widgets::card_row(theme, provider.accounts.is_empty())
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::row_title(theme, "Account not recorded"))
                            .children(self.limited_rows(&provider.unattributed, &key, theme, cx)),
                    ),
            );
        }
        widgets::section(theme, provider.name, card).into_any_element()
    }
}

impl popover::ScrollRailHost for UsagePage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for UsagePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let now = Utc::now();
        let providers = {
            let state = self.state.read(cx);
            self.snapshot(cx)
                .map(|snapshot| assemble(snapshot, &state.chats, state.local_device_id.as_deref()))
        };
        let empty_note = |copy: &'static str| {
            div()
                .mt(px(32.0))
                .px(px(8.0))
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(copy))
                .into_any_element()
        };
        let body: Vec<AnyElement> = match &providers {
            None => vec![empty_note("Reading usage…")],
            Some(providers) if providers.is_empty() => vec![empty_note(
                "No signed-in accounts on this device. Connect one in Settings → Providers.",
            )],
            Some(providers) => providers
                .iter()
                .map(|provider| self.provider_section(provider, now, &theme, cx))
                .collect(),
        };
        let refreshing = self
            .last_forced
            .is_some_and(|at| at.elapsed() < FORCE_MIN_INTERVAL);
        let refresh = widgets::action_button(&theme, widgets::ActionTone::Quiet)
            .id("usage-refresh")
            .flex_none()
            .tab_index(0)
            .role(gpui::Role::Button)
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .when(refreshing, |el| el.opacity(0.5).cursor_default())
            .when(!refreshing, |el| {
                el.on_click(cx.listener(|page, _, _, cx| {
                    page.load(true, cx);
                    page.history.update(cx, |history, cx| history.load(cx));
                    cx.notify();
                }))
            })
            .child(
                crate::icons::icon(crate::icons::REFRESH)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from("Refresh"));
        let scrollbar = popover::rail(self, "usage-page-scrollbar", &theme, cx);
        div()
            .id("usage-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("usage-page")
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
                                                .child(widgets::page_header(&theme, "Usage", None))
                                                .child(widgets::page_subtitle(
                                                    &theme,
                                                    "Plan limits for every signed-in account on this \
                                                     device, the chats waiting on them, and what \
                                                     your agents used over time. Nothing here \
                                                     spends tokens.",
                                                )),
                                        )
                                        .child(refresh),
                                )
                                .children(body)
                                .child(self.history.clone()),
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
    use chrono::TimeZone as _;
    use zeron_proto::{ChatLimit, LimitLogin, LimitRecovery, login_fingerprint};

    fn window(label: &str, used: f32, resets_at: Option<DateTime<Utc>>) -> AgentUsageWindow {
        AgentUsageWindow {
            label: label.into(),
            used_fraction: used,
            resets_at,
        }
    }

    fn account(
        id: &str,
        harness: HarnessId,
        active: bool,
        windows: Vec<AgentUsageWindow>,
    ) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": harness,
            "email": format!("{id}@example.com"),
            "planLabel": "Max",
            "active": active,
            "switchable": true,
            "usageWindows": windows,
        }))
        .unwrap()
    }

    fn chat(id: &str, harness: HarnessId, device: &str, fingerprint: Option<String>) -> Chat {
        let mut chat: Chat = serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": device,
            "title": format!("Chat {id}"),
            "archived": false,
            "cwd": null,
            "branch": null,
            "checkoutId": null,
            "config": { "harness": harness, "model": null, "reasoning": null, "sandbox": "workspace-write" },
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": Utc::now(),
        }))
        .unwrap();
        chat.limit = Some(ChatLimit {
            limited_at: Utc::now(),
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
    fn windows_are_classified_and_ordered_five_hour_first() {
        assert_eq!(window_kind("Session"), WindowKind::FiveHour);
        assert_eq!(window_kind("5h"), WindowKind::FiveHour);
        assert_eq!(window_kind("Week"), WindowKind::Weekly);
        assert_eq!(window_kind("Month"), WindowKind::Other);
        let codex = account(
            "c",
            HarnessId::Codex,
            true,
            vec![window("Week", 0.2, None), window("Session", 0.64, None)],
        );
        let labels: Vec<String> = ordered_windows(&codex)
            .into_iter()
            .map(window_label)
            .collect();
        assert_eq!(labels, ["5-hour", "Weekly"]);
        assert_eq!(gauge_reading(&codex), Some(0.64));
        assert_eq!(weekly(&codex).map(|w| w.used_fraction), Some(0.2));
        // No 5-hour window: the binding one.
        let cursor = account(
            "u",
            HarnessId::Cursor,
            true,
            vec![window("Month", 1.3, None)],
        );
        assert_eq!(gauge_reading(&cursor), Some(1.0));
        assert_eq!(
            gauge_reading(&account("e", HarnessId::Codex, true, vec![])),
            None
        );
    }

    #[test]
    fn resets_read_as_a_countdown() {
        let now = Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap();
        let at = |secs: i64| now + chrono::TimeDelta::seconds(secs);
        assert_eq!(resets_in(at(2 * 3600 + 14 * 60), now), "resets in 2h 14m");
        assert_eq!(resets_in(at(3600), now), "resets in 1h");
        assert_eq!(
            resets_in(at(59), now),
            "resets in 1m",
            "rounds up, never 0m"
        );
        assert_eq!(resets_in(at(12 * 60), now), "resets in 12m");
        assert_eq!(
            resets_in(at(3 * 86_400 + 4 * 3600 + 59), now),
            "resets in 3d 4h"
        );
        assert_eq!(resets_in(at(2 * 86_400), now), "resets in 2d");
        assert_eq!(resets_in(at(-30), now), "resets now");
        let line = window_summary(&window("Session", 0.642, Some(at(2 * 3600 + 14 * 60))), now);
        assert!(
            line.starts_with("5-hour 64% · resets in 2h 14m ("),
            "{line}"
        );
        assert_eq!(
            window_summary(&window("Month", 0.5, None), now),
            "Month 50%"
        );
    }

    #[test]
    fn the_tooltip_lists_both_windows_and_who() {
        let now = Utc::now();
        let a = account(
            "me",
            HarnessId::ClaudeCode,
            true,
            vec![window("Week", 0.22, None), window("Session", 0.64, None)],
        );
        let tooltip = gauge_tooltip(&a, now);
        let lines: Vec<&str> = tooltip.lines().collect();
        assert_eq!(lines[0], "5-hour 64%");
        assert_eq!(lines[1], "Weekly 22%");
        assert_eq!(lines[2], "me@example.com");
    }

    #[test]
    fn the_cockpit_groups_accounts_and_maps_limited_chats() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account(
                    "codex-1",
                    HarnessId::Codex,
                    true,
                    vec![window("Session", 0.1, None)],
                ),
                account(
                    "claude-1",
                    HarnessId::ClaudeCode,
                    false,
                    vec![window("Session", 1.0, None)],
                ),
                account(
                    "claude-2",
                    HarnessId::ClaudeCode,
                    true,
                    vec![window("Session", 0.3, None)],
                ),
            ],
            warnings: Vec::new(),
        };
        let chats = vec![
            chat(
                "hit-1",
                HarnessId::ClaudeCode,
                "here",
                login_fingerprint(["claude-1"].into_iter()),
            ),
            chat(
                "gone",
                HarnessId::ClaudeCode,
                "here",
                login_fingerprint(["signed-out"].into_iter()),
            ),
            chat(
                "remote",
                HarnessId::ClaudeCode,
                "elsewhere",
                login_fingerprint(["claude-1"].into_iter()),
            ),
            chat(
                "codex",
                HarnessId::Codex,
                "here",
                login_fingerprint(["codex-1"].into_iter()),
            ),
        ];
        let mut archived = chat(
            "old",
            HarnessId::Codex,
            "here",
            login_fingerprint(["codex-1"].into_iter()),
        );
        archived.archived = true;
        let mut not_limited = chat("fine", HarnessId::Codex, "here", None);
        not_limited.limit = None;
        let mut all = chats.clone();
        all.extend([archived, not_limited]);

        let cockpit = assemble(&snapshot, &all, Some("here"));
        // Settings order (Claude before Codex), accounts in engine order.
        assert_eq!(cockpit.len(), 2);
        assert_eq!(cockpit[0].harness, HarnessId::ClaudeCode);
        assert_eq!(cockpit[0].name, "Claude Code");
        let ids: Vec<&str> = cockpit[0]
            .accounts
            .iter()
            .map(|r| r.account.id.as_str())
            .collect();
        assert_eq!(ids, ["claude-1", "claude-2"]);
        assert_eq!(
            cockpit[0].accounts[0].limited,
            [LimitedChat {
                id: "hit-1".into(),
                title: "Chat hit-1".into()
            }]
        );
        assert!(cockpit[0].accounts[1].limited.is_empty());
        assert_eq!(cockpit[0].unattributed.len(), 1);
        assert_eq!(cockpit[0].unattributed[0].id, "gone");
        assert_eq!(
            cockpit[1].accounts[0].limited.len(),
            1,
            "archived and unlimited chats skipped"
        );
        // Providers with nothing to show are left out.
        assert!(cockpit.iter().all(|p| p.harness != HarnessId::Cursor));
        assert!(assemble(&AgentAccountsSnapshot::default(), &[], None).is_empty());
    }
}

#[cfg(feature = "demo-fixture")]
impl UsagePage {
    pub fn fixture_history(&self) -> Entity<UsageHistoryView> {
        self.history.clone()
    }
}
