//! The usage-limit banner attached above the composer of a Limited chat:
//! when the window resets and what happens then, with the recovery choices
//! (resume at reset, snooze until reset, dismiss). The chat's host engine
//! owns the timer; choices go to it through `SetChatLimitRecovery`.
use chrono::{DateTime, Local, TimeDelta, Utc};
use zeron_proto::{ChatLimit, LimitRecovery, LimitRecoveryAction};

use super::*;
use crate::settings::widgets::{self, ActionTone};

/// One button on the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BannerAction {
    ResumeAtReset,
    Snooze,
    /// Cancel a scheduled resume or snooze.
    Cancel,
    /// The reset passed, or nobody knows it: continue now.
    ResumeNow,
    /// No reset time: try again in an hour.
    RetryLater,
    Dismiss,
}

impl BannerAction {
    fn label(self) -> &'static str {
        match self {
            Self::ResumeAtReset => "Resume at reset",
            Self::Snooze => "Snooze until reset",
            Self::Cancel => "Cancel",
            Self::ResumeNow => "Resume now",
            Self::RetryLater => "Retry in 1 hour",
            Self::Dismiss => "Dismiss",
        }
    }

    /// The RPC action and the reset it picks, if it picks one.
    fn request(self, now: DateTime<Utc>) -> (LimitRecoveryAction, Option<DateTime<Utc>>) {
        match self {
            Self::ResumeAtReset => (LimitRecoveryAction::Resume, None),
            Self::Snooze => (LimitRecoveryAction::Snooze, None),
            Self::Cancel => (LimitRecoveryAction::Ask, None),
            Self::ResumeNow => (LimitRecoveryAction::Resume, Some(now)),
            Self::RetryLater => (LimitRecoveryAction::Resume, Some(now + TimeDelta::hours(1))),
            Self::Dismiss => (LimitRecoveryAction::Dismiss, None),
        }
    }
}

/// What the banner says and offers for a marker at `now`. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BannerModel {
    pub title: String,
    pub detail: String,
    pub actions: Vec<BannerAction>,
}

/// "3:40 PM" today, "Tue 3:40 PM" this week, "Oct 20, 3:40 PM" later.
pub(crate) fn local_moment_label(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = at.with_timezone(&Local);
    let today = now.with_timezone(&Local).date_naive();
    let days = (local.date_naive() - today).num_days();
    match days {
        0 => local.format("%-I:%M %p").to_string(),
        1..=6 => local.format("%a %-I:%M %p").to_string(),
        _ => local.format("%b %-d, %-I:%M %p").to_string(),
    }
}

pub(super) fn banner_model(limit: &ChatLimit, now: DateTime<Utc>) -> BannerModel {
    let title = match limit.window.as_deref() {
        Some(window) => format!("{window} usage limit reached"),
        None => "Usage limit reached".to_owned(),
    };
    let title = {
        let mut chars = title.chars();
        chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or(title)
    };
    use BannerAction::*;
    let (detail, actions) = match (limit.reset_at, limit.recovery) {
        (None, _) => (
            "The provider didn't say when it resets.".to_owned(),
            vec![RetryLater, ResumeNow, Dismiss],
        ),
        (Some(at), _) if at <= now => ("The limit has reset.".to_owned(), vec![ResumeNow, Dismiss]),
        (Some(at), LimitRecovery::ResumeAtReset) => (
            format!(
                "Resumes automatically at {}. Messages you send wait until then.",
                local_moment_label(at, now)
            ),
            vec![Cancel, Dismiss],
        ),
        (Some(at), LimitRecovery::Snoozed) => (
            format!("Snoozed until {}.", local_moment_label(at, now)),
            vec![Cancel, Dismiss],
        ),
        (Some(at), LimitRecovery::Ask) => (
            format!(
                "Resets at {}. Messages you send wait until then.",
                local_moment_label(at, now)
            ),
            vec![ResumeAtReset, Snooze, Dismiss],
        ),
    };
    BannerModel {
        title,
        detail,
        actions,
    }
}

impl Composer {
    /// The banner for the selected chat, when it is Limited.
    pub(super) fn render_limit_banner(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let limit = self.state.read(cx).selected_chat_row()?.limit.clone()?;
        let model = banner_model(&limit, Utc::now());
        let tone = theme.limited;
        let buttons = model.actions.iter().map(|action| {
            let action = *action;
            let button_tone = match action {
                BannerAction::ResumeAtReset | BannerAction::ResumeNow => ActionTone::Outlined,
                _ => ActionTone::Quiet,
            };
            widgets::text_action(theme, button_tone, action.label())
                .id(SharedString::from(format!("limit-banner-{action:?}")))
                .tab_index(0)
                .role(gpui::Role::Button)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.choose_limit_recovery(action, cx);
                }))
        });
        Some(
            crate::motion::fade_in(
                "composer-limit-banner",
                div()
                    .id("composer-limit-banner")
                    .debug_selector(|| "composer-limit-banner".into())
                    .mx(px(4.0))
                    .mt(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(tone.opacity(0.22))
                    .bg(tone.opacity(0.06))
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(px(16.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                crate::icons::icon(crate::icons::CLOCK_CIRCLE)
                                    .size(px(14.0))
                                    .text_color(tone),
                            )
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(model.title),
                            ),
                    )
                    .child(div().text_color(theme.text_muted).child(model.detail))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(4.0))
                            .children(buttons),
                    ),
            )
            .into_any_element(),
        )
    }

    /// Send a banner choice to the chat's host engine.
    fn choose_limit_recovery(&mut self, action: BannerAction, cx: &mut Context<Self>) {
        let (engine, chat_id, host) = {
            let state = self.state.read(cx);
            let Some(engine) = state.engine().cloned() else {
                return;
            };
            let Some(chat) = state.selected_chat_row() else {
                return;
            };
            let host = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
                .then(|| chat.device_id.clone());
            (engine, chat.id.clone(), host)
        };
        let (rpc_action, reset_at) = action.request(Utc::now());
        let mut params = serde_json::json!({ "chatId": chat_id, "action": rpc_action });
        if let Some(at) = reset_at {
            params["resetAt"] = serde_json::json!(at);
        }
        if let Some(host) = host {
            params["targetDeviceId"] = serde_json::json!(host);
        }
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_CHAT_LIMIT_RECOVERY, params)
                .await;
            if let Err(err) = result {
                tracing::warn!(error = %err, "limit recovery choice failed");
                this.update(cx, |composer, cx| {
                    composer.failure =
                        Some(format!("Couldn't update the usage limit: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn limit(reset: Option<DateTime<Utc>>, recovery: LimitRecovery) -> ChatLimit {
        ChatLimit {
            limited_at: Utc.with_ymd_and_hms(2026, 10, 15, 10, 0, 0).unwrap(),
            reset_at: reset,
            reset_source: None,
            window: Some("5-hour".into()),
            recovery,
            login: None,
        }
    }

    #[test]
    fn the_banner_offers_what_the_marker_allows() {
        use BannerAction::*;
        let now = Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap();
        let later = Some(now + TimeDelta::hours(2));
        let asking = banner_model(&limit(later, LimitRecovery::Ask), now);
        assert_eq!(asking.title, "5-hour usage limit reached");
        assert!(asking.detail.starts_with("Resets at "));
        assert_eq!(asking.actions, [ResumeAtReset, Snooze, Dismiss]);
        let armed = banner_model(&limit(later, LimitRecovery::ResumeAtReset), now);
        assert!(armed.detail.starts_with("Resumes automatically at "));
        assert_eq!(armed.actions, [Cancel, Dismiss]);
        let snoozed = banner_model(&limit(later, LimitRecovery::Snoozed), now);
        assert!(snoozed.detail.starts_with("Snoozed until "));
        let unknown = banner_model(&limit(None, LimitRecovery::Ask), now);
        assert_eq!(unknown.actions, [RetryLater, ResumeNow, Dismiss]);
        let passed = banner_model(&limit(Some(now), LimitRecovery::Ask), now);
        assert_eq!(passed.actions, [ResumeNow, Dismiss]);
        assert_eq!(
            RetryLater.request(now),
            (LimitRecoveryAction::Resume, Some(now + TimeDelta::hours(1)))
        );
    }
}
