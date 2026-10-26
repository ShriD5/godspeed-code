//! Mission Control's board, kept pure: which chats get a card, what each card
//! says, how cards group (device, then project) and sort, the top strip's
//! totals, and the `/` filter. Everything comes from rows `AppState` already
//! holds — no RPC, no model tokens.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use zeron_proto::attention::{AttentionItem, AttentionKind};
use zeron_proto::{Chat, ChatIndicator, HarnessId, LifecycleState};

use crate::state::AppState;

/// Failed and finished chats stay on the board this long after their last
/// message; live ones (working, waiting, limited) stay however old.
pub(crate) const RECENT_HOURS: i64 = 24;

/// A card's status, declared most urgent first (the in-project sort order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CardStatus {
    NeedsYou,
    Failed,
    Limited,
    Working,
    Finished,
}

impl CardStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Failed => "Failed",
            Self::Limited => "Limited",
            Self::Working => "Working",
            Self::Finished => "Finished",
        }
    }

    /// The sidebar's display status, folded into the board's five. A settled
    /// turn still waiting on background work reads as working, like its row.
    pub(crate) fn from_indicator(status: ChatIndicator, background_work: u32) -> Self {
        match status {
            ChatIndicator::Working => Self::Working,
            ChatIndicator::AwaitingInput | ChatIndicator::AwaitingApproval => Self::NeedsYou,
            ChatIndicator::Limited => Self::Limited,
            ChatIndicator::Errored => Self::Failed,
            ChatIndicator::Completed | ChatIndicator::Idle if background_work > 0 => Self::Working,
            ChatIndicator::Completed | ChatIndicator::Idle => Self::Finished,
        }
    }

    /// On the board whatever its age.
    pub(crate) fn live(self) -> bool {
        matches!(self, Self::NeedsYou | Self::Limited | Self::Working)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Card {
    pub chat_id: String,
    pub title: String,
    pub harness: Option<HarnessId>,
    pub status: CardStatus,
    /// "Working · 12m", "Needs you · 3m", "Finished · 2h".
    pub status_label: String,
    /// Finished or failed and not seen yet on any device.
    pub fresh: bool,
    /// The pending request for a card that needs you, else the last message.
    pub activity: Option<String>,
    pub subagents: u32,
    pub diff: Option<(u32, u32)>,
    pub branch: Option<String>,
    /// The chat runs in a linked worktree rather than its project's checkout.
    pub worktree: bool,
    /// The low-risk approval `a` answers for this card, by attention item id.
    pub approvable: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub project: String,
    pub device: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectGroup {
    pub key: String,
    pub label: String,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeviceGroup {
    pub device_id: String,
    pub name: String,
    pub local: bool,
    pub online: bool,
    pub projects: Vec<ProjectGroup>,
}

impl DeviceGroup {
    pub(crate) fn card_count(&self) -> usize {
        self.projects.iter().map(|p| p.cards.len()).sum()
    }
}

/// The top strip's live counts, across the whole board (filter or not).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub working: usize,
    pub needs_you: usize,
    pub limited: usize,
    pub finished_today: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Board {
    pub totals: Totals,
    pub devices: Vec<DeviceGroup>,
}

impl Board {
    pub(crate) fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub(crate) fn cards(&self) -> impl Iterator<Item = &Card> {
        self.devices
            .iter()
            .flat_map(|d| d.projects.iter())
            .flat_map(|p| p.cards.iter())
    }

    pub(crate) fn card(&self, chat_id: &str) -> Option<&Card> {
        self.cards().find(|card| card.chat_id == chat_id)
    }

    /// Card ids in display order, which is the order Left/Right walk.
    pub(crate) fn order(&self) -> Vec<String> {
        self.cards().map(|card| card.chat_id.clone()).collect()
    }

    /// The grid as drawn: every project starts a new row of `columns` cards.
    /// Up/Down move between these rows.
    pub(crate) fn rows(&self, columns: usize) -> Vec<Vec<String>> {
        let columns = columns.max(1);
        self.devices
            .iter()
            .flat_map(|d| d.projects.iter())
            .flat_map(|p| {
                p.cards
                    .chunks(columns)
                    .map(|row| row.iter().map(|card| card.chat_id.clone()).collect())
                    .collect::<Vec<Vec<String>>>()
            })
            .collect()
    }

    /// Only the cards matching every word of `query` (title, project, device,
    /// branch, agent, status and activity), dropping groups left empty. The
    /// totals keep describing everything.
    pub(crate) fn filtered(&self, query: &str) -> Board {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|word| word.to_lowercase())
            .collect();
        if words.is_empty() {
            return self.clone();
        }
        let devices = self
            .devices
            .iter()
            .filter_map(|device| {
                let projects: Vec<ProjectGroup> = device
                    .projects
                    .iter()
                    .filter_map(|project| {
                        let cards: Vec<Card> = project
                            .cards
                            .iter()
                            .filter(|card| {
                                let haystack = card_haystack(card);
                                words.iter().all(|word| haystack.contains(word.as_str()))
                            })
                            .cloned()
                            .collect();
                        (!cards.is_empty()).then(|| ProjectGroup {
                            cards,
                            ..project.clone()
                        })
                    })
                    .collect();
                (!projects.is_empty()).then(|| DeviceGroup {
                    projects,
                    ..device.clone()
                })
            })
            .collect();
        Board {
            totals: self.totals,
            devices,
        }
    }
}

fn card_haystack(card: &Card) -> String {
    let harness = card
        .harness
        .map(crate::shell::harness_updates::agent_name)
        .unwrap_or_default();
    [
        card.title.as_str(),
        card.project.as_str(),
        card.device.as_str(),
        card.branch.as_deref().unwrap_or_default(),
        harness,
        card.status.label(),
        card.activity.as_deref().unwrap_or_default(),
    ]
    .join("\n")
    .to_lowercase()
}

/// A project's cards while assembling: (key, label, cards).
type Bucket = (String, String, Vec<Card>);

/// Assemble the board from the app state. `today_start` is the local
/// midnight the "finished today" count runs from; `diff` reads a chat's
/// +/- counts where something already watches them (the sidebar's source).
pub(crate) fn assemble(
    state: &AppState,
    now: DateTime<Utc>,
    today_start: DateTime<Utc>,
    diff: &dyn Fn(&Chat) -> Option<(u32, u32)>,
) -> Board {
    let recent_since = now - TimeDelta::hours(RECENT_HOURS);
    let mut attention: HashMap<&str, Vec<&AttentionItem>> = HashMap::new();
    for item in &state.attention.items {
        if let Some(chat_id) = item.chat_id.as_deref() {
            attention.entry(chat_id).or_default().push(item);
        }
    }
    let local = state.local_device_id.as_deref();

    let mut totals = Totals::default();
    // Device id -> its projects, in first-seen order.
    let mut devices: Vec<(String, Vec<Bucket>)> = Vec::new();
    for chat in state.visible_chats() {
        // Rows naming a space this device never synced are not drawn
        // anywhere (the sidebar's rule).
        let space = match chat.space_id.as_deref() {
            Some(id) => match state.space_row(id) {
                Some(space) => Some(space),
                None => continue,
            },
            None => None,
        };
        let indicator = state.display_status_for(chat, now);
        let background = state.background_work_for(&chat.id, now);
        let status = CardStatus::from_indicator(indicator, background);
        let last_activity = chat.last_message_at.unwrap_or(chat.created_at);
        let put_away = chat.lifecycle.at(now) != LifecycleState::Active;
        if !status.live() && (put_away || last_activity < recent_since) {
            continue;
        }
        let items = attention.get(chat.id.as_str());
        let asking = items.and_then(|items| {
            items
                .iter()
                .find(|item| matches!(item.kind, AttentionKind::Approval | AttentionKind::Question))
                .copied()
        });
        let (project_key, project) = match space {
            Some(space) => (
                zeron_proto::view::project_key(space),
                state.representative_space(space).display_name().to_string(),
            ),
            None => ("home".to_string(), "No project".to_string()),
        };
        let device = state
            .device_name(&chat.device_id)
            .unwrap_or("Unknown device")
            .to_string();
        let card = Card {
            chat_id: chat.id.clone(),
            title: zeron_proto::view::single_line(chat.title.as_deref().unwrap_or("New session")),
            harness: chat.config.as_ref().map(|config| config.harness),
            status,
            status_label: status_label(state, chat, status, indicator, background, asking, now),
            fresh: matches!(indicator, ChatIndicator::Completed | ChatIndicator::Errored),
            activity: activity_line(chat, status, asking),
            subagents: state.running_subagents_for(&chat.id, now),
            diff: diff(chat).filter(|(add, del)| add + del > 0),
            branch: chat
                .branch
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string),
            worktree: match (chat.checkout_id.as_deref(), space) {
                (Some(own), Some(space)) => {
                    space.checkout_id.as_deref().is_some_and(|root| root != own)
                }
                _ => false,
            },
            approvable: items.and_then(|items| {
                items
                    .iter()
                    .find(|item| item.batch_approvable())
                    .map(|item| item.id.clone())
            }),
            last_activity,
            project: project.clone(),
            device,
        };
        match status {
            CardStatus::Working => totals.working += 1,
            CardStatus::NeedsYou => totals.needs_you += 1,
            CardStatus::Limited => totals.limited += 1,
            CardStatus::Finished if last_activity >= today_start => totals.finished_today += 1,
            CardStatus::Finished | CardStatus::Failed => {}
        }
        let projects = match devices.iter_mut().find(|(id, _)| *id == chat.device_id) {
            Some((_, projects)) => projects,
            None => {
                devices.push((chat.device_id.clone(), Vec::new()));
                &mut devices.last_mut().expect("just pushed").1
            }
        };
        match projects.iter_mut().find(|(key, _, _)| *key == project_key) {
            Some((_, _, cards)) => cards.push(card),
            None => projects.push((project_key, project, vec![card])),
        }
    }

    let mut devices: Vec<DeviceGroup> = devices
        .into_iter()
        .map(|(device_id, projects)| {
            let mut projects: Vec<ProjectGroup> = projects
                .into_iter()
                .map(|(key, label, mut cards)| {
                    cards.sort_by(|a, b| {
                        a.status
                            .cmp(&b.status)
                            .then_with(|| b.last_activity.cmp(&a.last_activity))
                            .then_with(|| a.chat_id.cmp(&b.chat_id))
                    });
                    ProjectGroup { key, label, cards }
                })
                .collect();
            // The most urgent project first, then the most recently active.
            projects.sort_by(|a, b| {
                let lead = |p: &ProjectGroup| p.cards[0].status;
                let latest = |p: &ProjectGroup| p.cards.iter().map(|c| c.last_activity).max();
                lead(a)
                    .cmp(&lead(b))
                    .then_with(|| latest(b).cmp(&latest(a)))
                    .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
                    .then_with(|| a.key.cmp(&b.key))
            });
            DeviceGroup {
                name: state
                    .device_name(&device_id)
                    .unwrap_or("Unknown device")
                    .to_string(),
                local: local == Some(device_id.as_str()),
                online: state.device_online(&device_id, now),
                device_id,
                projects,
            }
        })
        .collect();
    // This device first, then the others by name.
    devices.sort_by(|a, b| {
        b.local
            .cmp(&a.local)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
    Board { totals, devices }
}

fn status_label(
    state: &AppState,
    chat: &Chat,
    status: CardStatus,
    indicator: ChatIndicator,
    background: u32,
    asking: Option<&AttentionItem>,
    now: DateTime<Utc>,
) -> String {
    let ago = |at: DateTime<Utc>| match zeron_proto::view::format_time_ago(at, now).as_str() {
        "now" => "just now".to_string(),
        other => other.to_string(),
    };
    let last = chat.last_message_at.unwrap_or(chat.created_at);
    match status {
        CardStatus::Working
            if matches!(indicator, ChatIndicator::Completed | ChatIndicator::Idle) =>
        {
            crate::shell::spaces::waiting_on_label(background)
        }
        CardStatus::Working => {
            let since = state
                .pending_send_started(&chat.id, now)
                .or_else(|| state.session_for(&chat.id)?.started_at);
            match since {
                Some(since) => format!(
                    "Working · {}",
                    crate::shell::spaces::working_duration_label(now - since)
                ),
                None => "Working".to_string(),
            }
        }
        CardStatus::NeedsYou => {
            let since = asking.map_or(last, |item| item.created_at);
            format!("Needs you · {}", ago(since))
        }
        CardStatus::Limited => match chat.limit.as_ref().and_then(|limit| limit.reset_at) {
            Some(reset) if reset > now => format!(
                "Limited · back in {}",
                crate::shell::spaces::working_duration_label(reset - now)
            ),
            _ => "Limited".to_string(),
        },
        CardStatus::Failed => format!("Failed · {}", ago(last)),
        CardStatus::Finished => format!("Finished · {}", ago(last)),
    }
}

fn activity_line(
    chat: &Chat,
    status: CardStatus,
    asking: Option<&AttentionItem>,
) -> Option<String> {
    let pending = (status == CardStatus::NeedsYou)
        .then_some(asking)
        .flatten()
        .map(|item| match &item.approval {
            Some(approval) => approval.title.clone(),
            None if !item.summary.is_empty() => item.summary.clone(),
            None => item.title.clone(),
        });
    pending
        .or_else(|| chat.last_message_preview.clone())
        .map(|text| zeron_proto::view::single_line(&text))
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use zeron_proto::attention::{AttentionAction, AttentionSnapshot};
    use zeron_proto::{ApprovalRisk, Device, Session, SessionStatus, Space};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
    }

    fn today() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 0, 0, 0).unwrap()
    }

    fn minutes_ago(m: i64) -> DateTime<Utc> {
        now() - TimeDelta::minutes(m)
    }

    fn chat(id: &str, device: &str, space: Option<&str>, last_min: i64) -> Chat {
        let mut chat: Chat = serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": device,
            "title": format!("Chat {id}"),
            "archived": false,
            "cwd": null,
            "branch": null,
            "checkoutId": null,
            "config": { "harness": "claude-code", "model": null, "reasoning": null, "sandbox": "workspace-write" },
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": now() - TimeDelta::days(3),
        }))
        .unwrap();
        chat.space_id = space.map(str::to_string);
        chat.last_message_at = Some(minutes_ago(last_min));
        // Seen, unless a test says otherwise.
        chat.last_seen_at = chat.last_message_at;
        chat
    }

    fn space(id: &str, device: &str, name: &str) -> Space {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": device,
            "path": format!("/src/{name}"),
            "name": null,
            "gitDetected": true,
            "gitCheckedAt": null,
            "checkoutId": format!("checkout-{id}"),
            "repositoryId": format!("repo-{name}"),
            "createdAt": now() - TimeDelta::days(10),
        }))
        .unwrap()
    }

    fn device(id: &str, name: &str) -> Device {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "platform": "macos",
            "lastSeenAt": now(),
        }))
        .unwrap()
    }

    fn session(chat_id: &str, device: &str, status: SessionStatus, approval: bool) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: chat_id.into(),
            device_id: device.into(),
            status,
            started_at: Some(minutes_ago(12)),
            updated_at: now(),
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: approval,
        }
    }

    fn approval_item(id: &str, chat_id: &str, risk: ApprovalRisk) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind: AttentionKind::Approval,
            chat_id: Some(chat_id.into()),
            title: format!("Approve {id}"),
            space_id: None,
            project: None,
            harness: None,
            created_at: minutes_ago(3),
            risk: Some(risk),
            summary: String::new(),
            actions: vec![AttentionAction::Approve, AttentionAction::Open],
            request_id: None,
            approval: None,
            agent_run: None,
            conductor: None,
        }
    }

    fn state() -> AppState {
        let mut state = AppState::new();
        state.local_device_id = Some("mac".into());
        state.devices = vec![device("mac", "MacBook"), device("box", "Studio")];
        state.spaces = vec![
            space("s-gs", "mac", "godspeed"),
            space("s-pp", "mac", "palmpong"),
            space("s-box", "box", "godspeed"),
        ];
        state
    }

    fn no_diff(_: &Chat) -> Option<(u32, u32)> {
        None
    }

    fn board(state: &AppState) -> Board {
        assemble(state, now(), today(), &no_diff)
    }

    fn ids(cards: &[Card]) -> Vec<&str> {
        cards.iter().map(|c| c.chat_id.as_str()).collect()
    }

    #[test]
    fn indicators_fold_into_five_statuses() {
        use ChatIndicator as I;
        assert_eq!(
            CardStatus::from_indicator(I::Working, 0),
            CardStatus::Working
        );
        assert_eq!(
            CardStatus::from_indicator(I::AwaitingInput, 0),
            CardStatus::NeedsYou
        );
        assert_eq!(
            CardStatus::from_indicator(I::AwaitingApproval, 0),
            CardStatus::NeedsYou
        );
        assert_eq!(
            CardStatus::from_indicator(I::Limited, 0),
            CardStatus::Limited
        );
        assert_eq!(
            CardStatus::from_indicator(I::Errored, 0),
            CardStatus::Failed
        );
        assert_eq!(
            CardStatus::from_indicator(I::Completed, 0),
            CardStatus::Finished
        );
        assert_eq!(CardStatus::from_indicator(I::Idle, 0), CardStatus::Finished);
        assert_eq!(
            CardStatus::from_indicator(I::Idle, 2),
            CardStatus::Working,
            "background work keeps a settled turn working"
        );
    }

    #[test]
    fn cards_group_by_device_then_project_with_this_device_first() {
        let mut state = state();
        state.chats = vec![
            chat("remote", "box", Some("s-box"), 5),
            chat("gs-1", "mac", Some("s-gs"), 30),
            chat("pp-1", "mac", Some("s-pp"), 10),
            chat("gs-2", "mac", Some("s-gs"), 20),
            chat("home", "mac", None, 40),
        ];
        let board = board(&state);
        let devices: Vec<(&str, bool)> = board
            .devices
            .iter()
            .map(|d| (d.name.as_str(), d.local))
            .collect();
        assert_eq!(devices, [("MacBook", true), ("Studio", false)]);
        let mac: Vec<(&str, Vec<&str>)> = board.devices[0]
            .projects
            .iter()
            .map(|p| (p.label.as_str(), ids(&p.cards)))
            .collect();
        // All finished: the most recently active project leads.
        assert_eq!(
            mac,
            [
                ("palmpong", vec!["pp-1"]),
                ("godspeed", vec!["gs-2", "gs-1"]),
                ("No project", vec!["home"]),
            ]
        );
        assert_eq!(board.devices[1].projects[0].label, "godspeed");
        assert_eq!(board.devices[0].card_count(), 4);
    }

    #[test]
    fn statuses_map_and_sort_most_urgent_first() {
        let mut state = state();
        state.chats = vec![
            chat("done", "mac", Some("s-gs"), 1),
            chat("work", "mac", Some("s-gs"), 50),
            chat("ask", "mac", Some("s-gs"), 90),
            chat("limit", "mac", Some("s-gs"), 70),
            chat("fail", "mac", Some("s-gs"), 60),
        ];
        state.chats[2].last_seen_at = None;
        let limited = state.chats.iter_mut().find(|c| c.id == "limit").unwrap();
        limited.limit = Some(zeron_proto::ChatLimit {
            limited_at: minutes_ago(70),
            reset_at: Some(now() + TimeDelta::minutes(95)),
            reset_source: None,
            window: None,
            recovery: Default::default(),
            login: None,
        });
        let failed = state.chats.iter_mut().find(|c| c.id == "fail").unwrap();
        failed.last_seen_at = None;
        state.sessions = vec![
            session("work", "mac", SessionStatus::Working, false),
            session("ask", "mac", SessionStatus::AwaitingInput, true),
            session("fail", "mac", SessionStatus::Errored, false),
        ];
        let board = board(&state);
        let cards = &board.devices[0].projects[0].cards;
        assert_eq!(ids(cards), ["ask", "fail", "limit", "work", "done"]);
        let labels: Vec<&str> = cards.iter().map(|c| c.status_label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Needs you · 1h",
                "Failed · 1h",
                "Limited · back in 1h 35m",
                "Working · 12m",
                "Finished · 1m",
            ]
        );
        assert!(cards[1].fresh, "an unseen failure is fresh");
        assert!(!cards[4].fresh, "a seen finish has settled");
        assert_eq!(
            board.totals,
            Totals {
                working: 1,
                needs_you: 1,
                limited: 1,
                finished_today: 1,
            }
        );
    }

    #[test]
    fn only_live_or_recent_chats_get_cards() {
        let mut state = state();
        state.chats = vec![
            chat("old-done", "mac", Some("s-gs"), 60 * 30),
            chat("old-working", "mac", Some("s-gs"), 60 * 30),
            chat("yesterday", "mac", Some("s-gs"), 60 * 13),
            chat("settled", "mac", Some("s-gs"), 5),
            chat("archived", "mac", Some("s-gs"), 5),
            chat("dangling", "mac", Some("gone"), 5),
            chat("worker", "mac", Some("s-gs"), 5),
        ];
        state.chats[3].lifecycle =
            zeron_proto::ChatLifecycle::new(LifecycleState::Settled, minutes_ago(4));
        state.chats[4].archived = true;
        state.chats[6].parent_chat_id = Some("yesterday".into());
        state.sessions = vec![session("old-working", "mac", SessionStatus::Working, false)];
        let board = board(&state);
        assert_eq!(board.order(), ["old-working", "yesterday"]);
        assert_eq!(
            board.totals.finished_today, 0,
            "yesterday's finish is not today's"
        );
    }

    #[test]
    fn cards_carry_the_activity_branch_and_low_risk_approval() {
        let mut state = state();
        let mut asking = chat("ask", "mac", Some("s-gs"), 2);
        asking.branch = Some("feat/mission".into());
        asking.checkout_id = Some("checkout-worktree".into());
        asking.last_message_preview = Some("Reading\nshell.rs".into());
        let mut risky = chat("risky", "mac", Some("s-gs"), 2);
        risky.last_message_preview = Some("Ran the tests".into());
        state.chats = vec![asking, risky];
        state.sessions = vec![
            session("ask", "mac", SessionStatus::AwaitingInput, true),
            session("risky", "mac", SessionStatus::AwaitingInput, true),
        ];
        state.sessions[0].running_subagents = 2;
        state.attention = AttentionSnapshot {
            device_id: "mac".into(),
            items: vec![
                approval_item("low", "ask", ApprovalRisk::Low),
                approval_item("high", "risky", ApprovalRisk::High),
            ],
        };
        let board = assemble(&state, now(), today(), &|chat: &Chat| {
            (chat.id == "ask").then_some((12, 3))
        });
        let ask = board.card("ask").unwrap();
        assert_eq!(ask.approvable.as_deref(), Some("low"));
        assert_eq!(ask.activity.as_deref(), Some("Approve low"));
        assert_eq!(ask.branch.as_deref(), Some("feat/mission"));
        assert!(ask.worktree);
        assert_eq!(ask.subagents, 2);
        assert_eq!(ask.diff, Some((12, 3)));
        assert_eq!(ask.status_label, "Needs you · 3m");
        let risky = board.card("risky").unwrap();
        assert_eq!(
            risky.approvable, None,
            "only low-risk approvals are offered"
        );
        assert!(!risky.worktree);
        assert_eq!(risky.diff, None);
    }

    #[test]
    fn the_filter_matches_every_word_and_drops_empty_groups() {
        let mut state = state();
        let mut a = chat("a", "mac", Some("s-gs"), 1);
        a.title = Some("Fix the sidebar".into());
        let mut b = chat("b", "mac", Some("s-pp"), 2);
        b.title = Some("Tune the serve".into());
        let mut c = chat("c", "box", Some("s-box"), 3);
        c.title = Some("Fix flaky test".into());
        state.chats = vec![a, b, c];
        let board = board(&state);
        let fix = board.filtered("fix");
        assert_eq!(fix.order(), ["a", "c"]);
        assert_eq!(fix.totals, board.totals, "totals ignore the filter");
        let studio = board.filtered("FIX studio");
        assert_eq!(studio.order(), ["c"]);
        assert_eq!(studio.devices.len(), 1);
        assert_eq!(board.filtered("palmpong").order(), ["b"]);
        assert!(board.filtered("nothing here").is_empty());
        assert_eq!(board.filtered("  ").order(), board.order());
    }

    #[test]
    fn rows_break_per_project_at_the_column_count() {
        let mut state = state();
        state.chats = (0..5)
            .map(|i| chat(&format!("gs-{i}"), "mac", Some("s-gs"), i))
            .chain([chat("pp", "mac", Some("s-pp"), 10)])
            .collect();
        let rows = board(&state).rows(2);
        let rows: Vec<Vec<&str>> = rows
            .iter()
            .map(|row| row.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            rows,
            [
                vec!["gs-0", "gs-1"],
                vec!["gs-2", "gs-3"],
                vec!["gs-4"],
                vec!["pp"],
            ]
        );
    }
}
