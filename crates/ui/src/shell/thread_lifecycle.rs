//! Thread lifecycle in the sidebar: the row menu's Settle, Snooze, Wake and
//! Mark as unread, the snooze times it offers or reads from what the user
//! types, and the one-step undo of the last such action (mod+z outside a
//! text field). The engine owns the state (`setChatLifecycle`); this picks
//! times and remembers what to put back.

use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Utc, Weekday,
};
use gpui::{
    AnyElement, Context, Entity, Focusable as _, SharedString, Subscription, Window, div,
    prelude::*, px,
};
use zeron_proto::{ChatIndicator, LifecycleState};

use super::{
    CHAT_MENU_ROW_SETTLE, CHAT_MENU_ROW_SNOOZE, CHAT_MENU_ROW_UNREAD, CHAT_MENU_ROW_WAKE,
    ChatMenuPage, Shell,
};
use crate::composer::limit_banner::local_moment_label;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::popover;
use crate::theme::Theme;

const MORNING: u32 = 9;
const EVENING: u32 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnoozePreset {
    OneHour,
    ThreeHours,
    ThisEvening,
    Tomorrow,
    NextWeek,
}

impl SnoozePreset {
    pub(super) const ALL: [Self; 5] = [
        Self::OneHour,
        Self::ThreeHours,
        Self::ThisEvening,
        Self::Tomorrow,
        Self::NextWeek,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::OneHour => "1 hour",
            Self::ThreeHours => "3 hours",
            Self::ThisEvening => "This evening",
            Self::Tomorrow => "Tomorrow",
            Self::NextWeek => "Next week",
        }
    }

    /// When a snooze picked at `now` ends: this evening at 6 PM (not offered
    /// once that is under an hour away), tomorrow at 9 AM, next Monday at
    /// 9 AM.
    pub(super) fn until<Tz: TimeZone>(self, now: &DateTime<Tz>) -> Option<DateTime<Utc>> {
        let today = now.date_naive();
        match self {
            Self::OneHour => Some(now.to_utc() + Duration::hours(1)),
            Self::ThreeHours => Some(now.to_utc() + Duration::hours(3)),
            Self::ThisEvening => local_at(now, today, EVENING, 0)
                .filter(|evening| *evening - now.to_utc() >= Duration::hours(1)),
            Self::Tomorrow => local_at(now, today.succ_opt()?, MORNING, 0),
            Self::NextWeek => local_at(now, next_weekday(today, Weekday::Mon), MORNING, 0),
        }
    }
}

/// A typed snooze end, as of `now` in its time zone: `in 2h`, `45m`, `3d`,
/// `9am`, `17:30`, `tomorrow`, `tomorrow 8am`, `fri`, `mon 14:00`,
/// `2026-10-20`, `2026-10-20 14:00`. A time already past today means
/// tomorrow; a day without a time means 9 AM. `None` for anything else or a
/// moment not ahead of `now`.
pub(super) fn parse_snooze<Tz: TimeZone>(input: &str, now: &DateTime<Tz>) -> Option<DateTime<Utc>> {
    let input = input.trim().to_lowercase();
    let words: Vec<&str> = input.split_whitespace().collect();
    let until = match words.as_slice() {
        [] => None,
        ["in", amount] | [amount] if parse_span(amount).is_some() => {
            parse_span(amount).map(|span| now.to_utc() + span)
        }
        [day] => day_at(now, day, None),
        [day, time] => day_at(now, day, Some(parse_time(time)?)),
        _ => None,
    }?;
    (until > now.to_utc()).then_some(until)
}

/// A day word (or a time alone) at `time`, defaulting to 9 AM on a day.
fn day_at<Tz: TimeZone>(
    now: &DateTime<Tz>,
    day: &str,
    time: Option<NaiveTime>,
) -> Option<DateTime<Utc>> {
    let today = now.date_naive();
    let at = |date: NaiveDate, time: Option<NaiveTime>| {
        let time = time.unwrap_or_else(|| NaiveTime::from_hms_opt(MORNING, 0, 0).unwrap());
        local_at(now, date, time.hour(), time.minute())
    };
    if time.is_none()
        && let Some(time) = parse_time(day)
    {
        let today_at = at(today, Some(time))?;
        return if today_at > now.to_utc() {
            Some(today_at)
        } else {
            at(today.succ_opt()?, Some(time))
        };
    }
    let date = match day {
        "today" => today,
        "tomorrow" | "tmrw" => today.succ_opt()?,
        _ => match parse_weekday(day) {
            Some(weekday) => next_weekday(today, weekday),
            None => NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?,
        },
    };
    at(date, time)
}

/// `90m`, `2h`, `3d` (also `90min`, `2hr`, `3days`).
fn parse_span(text: &str) -> Option<Duration> {
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let number: i64 = number.parse().ok().filter(|n| *n > 0)?;
    match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => Some(Duration::minutes(number)),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(Duration::hours(number)),
        "d" | "day" | "days" => Some(Duration::days(number)),
        _ => None,
    }
}

/// `17:30`, `9am`, `9:30pm`, `12am`.
fn parse_time(text: &str) -> Option<NaiveTime> {
    let (clock, meridiem) = match text.strip_suffix("am") {
        Some(clock) => (clock, Some(false)),
        None => match text.strip_suffix("pm") {
            Some(clock) => (clock, Some(true)),
            None => (text, None),
        },
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None if meridiem.is_some() => (clock.parse::<u32>().ok()?, 0),
        None => return None,
    };
    let hour = match meridiem {
        Some(pm) if (1..=12).contains(&hour) => hour % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        None => hour,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// A weekday by any prefix of its name of three letters or more.
fn parse_weekday(text: &str) -> Option<Weekday> {
    const NAMES: [(&str, Weekday); 7] = [
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ];
    NAMES
        .into_iter()
        .find(|(name, _)| text.len() >= 3 && name.starts_with(text))
        .map(|(_, weekday)| weekday)
}

/// The next `weekday` strictly after `today`.
fn next_weekday(today: NaiveDate, weekday: Weekday) -> NaiveDate {
    let ahead = (weekday.num_days_from_monday() + 7 - today.weekday().num_days_from_monday()) % 7;
    today + Duration::days(i64::from(if ahead == 0 { 7 } else { ahead }))
}

/// `hour:minute` on `date` in `now`'s zone; a time skipped by a DST jump
/// lands an hour later.
fn local_at<Tz: TimeZone>(
    now: &DateTime<Tz>,
    date: NaiveDate,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Utc>> {
    let naive = date.and_hms_opt(hour, minute, 0)?;
    let zone = now.timezone();
    zone.from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            zone.from_local_datetime(&(naive + Duration::hours(1)))
                .earliest()
        })
        .map(|at| at.to_utc())
}

/// The last sidebar lifecycle action, kept for one undo: the state it left
/// the chat in, so the undo never overwrites anything newer.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum LifecycleUndo {
    State {
        chat_id: String,
        before: LifecycleState,
        after: LifecycleState,
    },
    Unread {
        chat_id: String,
        seen_at: DateTime<Utc>,
    },
}

/// The Snooze submenu's typed-time field.
pub(super) struct SnoozeCustom {
    chat_id: String,
    input: Entity<ComposerInput>,
    focus_pending: bool,
    _events: Subscription,
}

/// A thread running or waiting on the user stays in the list.
fn live(status: ChatIndicator) -> bool {
    matches!(
        status,
        ChatIndicator::Working | ChatIndicator::AwaitingInput | ChatIndicator::AwaitingApproval
    )
}

impl Shell {
    /// The row menu's lifecycle entries: Settle (or Unsettle), Snooze, Wake
    /// while snoozed, and Mark as unread (or read). Archived chats have none.
    pub(super) fn render_lifecycle_menu_rows(
        &mut self,
        menu: gpui::Div,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let now = Utc::now();
        let row = {
            let state = self.state.read(cx);
            state
                .chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .map(|chat| {
                    (
                        chat.lifecycle.at(now),
                        chat.unseen(),
                        live(state.display_status_for(chat, now)),
                        chat.archived,
                    )
                })
        };
        let Some((lifecycle, unseen, live, false)) = row else {
            return menu;
        };
        let settled = lifecycle == LifecycleState::Settled;
        let snoozed = matches!(lifecycle, LifecycleState::Snoozed { .. });
        let settle_id = chat_id.to_owned();
        let settle = popover::menu_row(theme, false, format!("chat-menu-settle-{chat_id}"))
            .id("chat-menu-settle")
            .when(live && !settled, |el| el.opacity(0.4).cursor_default())
            .when(!live || settled, |el| {
                el.on_click(cx.listener(move |this, _, _, cx| {
                    let next = if settled {
                        LifecycleState::Active
                    } else {
                        LifecycleState::Settled
                    };
                    this.change_lifecycle(settle_id.clone(), next, cx)
                }))
            })
            .child(
                icon(icons::CHECKLIST)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from(if settled {
                "Unsettle"
            } else {
                "Settle"
            }));
        let snooze = if live {
            popover::menu_row(theme, false, format!("chat-menu-snooze-{chat_id}"))
                .id("chat-menu-snooze")
                .opacity(0.4)
                .cursor_default()
                .child(
                    icon(icons::CLOCK_CIRCLE)
                        .size(px(16.0))
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().child(SharedString::from("Snooze")))
                .into_any_element()
        } else {
            self.render_chat_submenu_trigger(
                chat_id,
                ChatMenuPage::Snooze,
                icons::CLOCK_CIRCLE,
                "Snooze",
                theme,
                cx,
            )
        };
        let mut menu = menu
            .child(self.chat_menu_hover_row(CHAT_MENU_ROW_SETTLE, settle, cx))
            .child(self.chat_menu_hover_row(CHAT_MENU_ROW_SNOOZE, snooze, cx));
        if snoozed {
            let wake_id = chat_id.to_owned();
            let wake = popover::menu_row(theme, false, format!("chat-menu-wake-{chat_id}"))
                .id("chat-menu-wake")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.change_lifecycle(wake_id.clone(), LifecycleState::Active, cx)
                }))
                .child(icon(icons::SUN).size(px(16.0)).text_color(theme.text_muted))
                .child(SharedString::from("Wake now"));
            menu = menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_WAKE, wake, cx));
        }
        let unread_id = chat_id.to_owned();
        let unread = popover::menu_row(theme, false, format!("chat-menu-unread-{chat_id}"))
            .id("chat-menu-unread")
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_chat_unread(unread_id.clone(), cx)),
            )
            .child(
                icon(if unseen {
                    icons::EYE
                } else {
                    icons::EYE_CLOSED
                })
                .size(px(16.0))
                .text_color(theme.text_muted),
            )
            .child(SharedString::from(if unseen {
                "Mark as read"
            } else {
                "Mark as unread"
            }));
        menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_UNREAD, unread, cx))
    }

    /// The Snooze submenu: the presets with when each ends, then a typed time.
    pub(super) fn render_snooze_submenu(
        &mut self,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let local = Local::now();
        let mut card = popover::popover_card(theme)
            .w(px(244.0))
            .relative()
            .flex()
            .flex_col()
            .gap(px(2.0));
        for preset in SnoozePreset::ALL {
            let Some(until) = preset.until(&local) else {
                continue;
            };
            let id = chat_id.to_owned();
            let slug = preset.label().to_lowercase().replace(' ', "-");
            card = card.child(
                popover::menu_row(theme, false, format!("chat-snooze-{slug}-{chat_id}"))
                    .id(SharedString::from(format!("chat-snooze-{slug}")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.change_lifecycle(id.clone(), LifecycleState::Snoozed { until }, cx)
                    }))
                    .child(div().flex_1().child(SharedString::from(preset.label())))
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(local_moment_label(until, now))),
                    ),
            );
        }
        card = card.child(popover::menu_separator());
        let custom = self
            .snooze_custom
            .as_ref()
            .filter(|custom| custom.chat_id == chat_id)
            .map(|custom| custom.input.clone());
        card = match custom {
            Some(input) => {
                let typed = input.read(cx).text().trim().to_owned();
                let hint = match parse_snooze(&typed, &local) {
                    Some(until) => format!("Until {}", local_moment_label(until, now)),
                    None if typed.is_empty() => "Type a time, then press Return".to_owned(),
                    None => "Try “fri 9am”, “in 2h” or “2026-10-20 14:00”".to_owned(),
                };
                card.child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(super::chat_title_editor(
                            "chat-snooze-custom-field".into(),
                            input,
                            theme,
                        ))
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(hint)),
                        ),
                )
            }
            None => {
                let id = chat_id.to_owned();
                card.child(
                    popover::menu_row(theme, false, format!("chat-snooze-custom-{chat_id}"))
                        .id("chat-snooze-custom")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_snooze_custom(id.clone(), cx)
                        }))
                        .child(
                            icon(icons::CALENDAR)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Pick a time…")),
                )
            }
        };
        let entity = cx.entity().downgrade();
        card.child(
            gpui::canvas(
                move |bounds, _, cx| {
                    let _ = entity.update(cx, |this, _| this.chat_submenu_bounds = Some(bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element()
    }

    fn open_snooze_custom(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            ComposerInput::new("fri 9am, in 2h, 2026-10-20 14:00", cx)
                .with_single_line()
                .with_text_metrics(13.0, 17.0)
                .with_accessibility_role(gpui::Role::TextInput)
        });
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.submit_snooze_custom(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        self.snooze_custom = Some(SnoozeCustom {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    /// Focus a just-opened typed-time field (opened without window access).
    pub(super) fn focus_snooze_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(custom) = self.snooze_custom.as_mut() else {
            return;
        };
        if std::mem::take(&mut custom.focus_pending) {
            let focus = custom.input.focus_handle(cx);
            window.focus(&focus, cx);
        }
    }

    fn submit_snooze_custom(&mut self, cx: &mut Context<Self>) {
        let Some(custom) = self.snooze_custom.as_ref() else {
            return;
        };
        let typed = custom.input.read(cx).text();
        if let Some(until) = parse_snooze(&typed, &Local::now()) {
            let chat_id = custom.chat_id.clone();
            self.change_lifecycle(chat_id, LifecycleState::Snoozed { until }, cx);
        }
    }

    /// Settle, snooze or wake a chat and remember how to undo it.
    pub(super) fn change_lifecycle(
        &mut self,
        chat_id: String,
        lifecycle: LifecycleState,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        let now = Utc::now();
        let Some(before) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .map(|chat| chat.lifecycle.at(now))
        else {
            return;
        };
        if before == lifecycle {
            return;
        }
        self.state.update(cx, |state, cx| {
            state.set_chat_lifecycle(&chat_id, lifecycle, None, cx)
        });
        let notice = match lifecycle {
            LifecycleState::Active => "Back in the list".to_owned(),
            LifecycleState::Settled => "Settled".to_owned(),
            LifecycleState::Snoozed { until } => {
                format!("Snoozed until {}", local_moment_label(until, now))
            }
        };
        self.remember_lifecycle_undo(
            LifecycleUndo::State {
                chat_id,
                before,
                after: lifecycle,
            },
            notice,
            cx,
        );
    }

    /// The settle shortcut: settles the open chat, or unsettles it when it
    /// already is. A chat still running can't settle.
    pub(super) fn settle_selected_chat(&mut self, cx: &mut Context<Self>) {
        let now = Utc::now();
        let target = {
            let state = self.state.read(cx);
            state
                .selected_chat_row()
                .filter(|chat| !chat.archived)
                .map(|chat| {
                    (
                        chat.id.clone(),
                        chat.lifecycle.at(now) == LifecycleState::Settled,
                        live(state.display_status_for(chat, now)),
                    )
                })
        };
        match target {
            Some((chat_id, true, _)) => self.change_lifecycle(chat_id, LifecycleState::Active, cx),
            Some((_, false, true)) => self.show_notice(
                crate::toast::ToastKind::Info,
                "Can't settle a session while it's running",
                cx,
            ),
            Some((chat_id, false, false)) => {
                self.change_lifecycle(chat_id, LifecycleState::Settled, cx)
            }
            None => {}
        }
    }

    fn toggle_chat_unread(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let unseen = self
            .state
            .read(cx)
            .chats
            .iter()
            .any(|chat| chat.id == chat_id && chat.unseen());
        if unseen {
            self.state
                .update(cx, |state, cx| state.mark_chat_seen(&chat_id, cx));
            return;
        }
        let seen_at = self
            .state
            .update(cx, |state, cx| state.mark_chat_unread(&chat_id, cx));
        if let Some(seen_at) = seen_at {
            self.remember_lifecycle_undo(
                LifecycleUndo::Unread { chat_id, seen_at },
                "Marked as unread".to_owned(),
                cx,
            );
        }
    }

    fn remember_lifecycle_undo(
        &mut self,
        undo: LifecycleUndo,
        notice: String,
        cx: &mut Context<Self>,
    ) {
        self.lifecycle_undo = Some(undo);
        let hint = crate::settings::badge_combo("mod-z");
        crate::toast::info(cx, format!("{notice} · {hint} to undo"));
        cx.notify();
    }

    /// Undo the last lifecycle action, unless the chat has moved on since.
    pub(super) fn undo_lifecycle_action(&mut self, cx: &mut Context<Self>) {
        let Some(undo) = self.lifecycle_undo.take() else {
            return;
        };
        let now = Utc::now();
        let undone = self.state.update(cx, |state, cx| match undo {
            LifecycleUndo::State {
                chat_id,
                before,
                after,
            } => {
                let unchanged = state
                    .chats
                    .iter()
                    .any(|chat| chat.id == chat_id && chat.lifecycle.at(now) == after);
                if unchanged {
                    state.set_chat_lifecycle(&chat_id, before, Some(after), cx);
                }
                unchanged
            }
            LifecycleUndo::Unread { chat_id, seen_at } => {
                state.restore_chat_seen(&chat_id, seen_at, cx);
                true
            }
        });
        crate::toast::info(
            cx,
            if undone {
                "Undone"
            } else {
                "The thread changed since; nothing to undo"
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// Friday 16 Oct 2026, 2:30 PM at UTC+5:30.
    fn now() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(5 * 3600 + 1800)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 16, 14, 30, 0)
            .unwrap()
    }

    fn local(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        now()
            .timezone()
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
            .to_utc()
    }

    #[test]
    fn presets_land_on_local_mornings_and_evenings() {
        let now = now();
        let until = |preset: SnoozePreset| preset.until(&now);
        assert_eq!(until(SnoozePreset::OneHour), Some(local(16, 15, 30)));
        assert_eq!(until(SnoozePreset::ThreeHours), Some(local(16, 17, 30)));
        assert_eq!(until(SnoozePreset::ThisEvening), Some(local(16, 18, 0)));
        assert_eq!(until(SnoozePreset::Tomorrow), Some(local(17, 9, 0)));
        assert_eq!(until(SnoozePreset::NextWeek), Some(local(19, 9, 0)));

        let evening = now + Duration::hours(3);
        assert_eq!(SnoozePreset::ThisEvening.until(&evening), None);
    }

    #[test]
    fn typed_times_read_spans_clock_times_days_and_dates() {
        let now = now();
        let parse = |text: &str| parse_snooze(text, &now);
        assert_eq!(parse("in 2h"), Some(local(16, 16, 30)));
        assert_eq!(parse("45m"), Some(local(16, 15, 15)));
        assert_eq!(parse("3d"), Some(local(19, 14, 30)));
        assert_eq!(parse("5pm"), Some(local(16, 17, 0)));
        assert_eq!(
            parse("9am"),
            Some(local(17, 9, 0)),
            "past today means tomorrow"
        );
        assert_eq!(parse("17:30"), Some(local(16, 17, 30)));
        assert_eq!(parse("Tomorrow"), Some(local(17, 9, 0)));
        assert_eq!(parse("tomorrow 8:15pm"), Some(local(17, 20, 15)));
        assert_eq!(parse("fri"), Some(local(23, 9, 0)), "the next one");
        assert_eq!(parse("thursday 10am"), Some(local(22, 10, 0)));
        assert_eq!(parse("2026-10-20 14:00"), Some(local(20, 14, 0)));
        assert_eq!(parse("2026-10-20"), Some(local(20, 9, 0)));
    }

    #[test]
    fn typed_times_refuse_nonsense_and_the_past() {
        let now = now();
        for text in [
            "",
            "soon",
            "13pm",
            "25:00",
            "in 0h",
            "frizzle",
            "2026-10-01",
            "today 9am",
        ] {
            assert_eq!(parse_snooze(text, &now), None, "{text:?}");
        }
    }
}
