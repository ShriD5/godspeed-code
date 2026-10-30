//! Conductor view models: what the sidebar entry and the main chat's cards
//! show, derived from rows the client already holds. Pure, so the rules test
//! without a window. The cards render as transcript rows (`transcript.rs`);
//! the sidebar, sheet and Answer pop-up live in `shell/conductor_ui.rs`.

use chrono::{DateTime, Utc};
use zeron_proto::attention::AttentionItem;
use zeron_proto::{Chat, ChatIndicator, Conductor, Session};

/// Steps shown on a card.
pub const CARD_STEPS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    Working,
    /// Waiting on an approval or a question.
    NeedsYou,
    Failed,
    Done,
    Idle,
}

impl CardStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::NeedsYou => "Needs you",
            Self::Failed => "Failed",
            Self::Done => "Done",
            Self::Idle => "Idle",
        }
    }
}

/// One sub-agent card under a conductor's main chat.
#[derive(Debug, Clone, PartialEq)]
pub struct CardModel {
    pub chat_id: String,
    pub title: String,
    pub status: CardStatus,
    /// The last [`CARD_STEPS`] steps, oldest first.
    pub steps: Vec<String>,
    /// Sub-agents below this one.
    pub nested: usize,
    /// The item to answer when something below (or here) is waiting.
    pub attention: Option<AttentionItem>,
    /// When the sub-agent was started: places the card in the transcript.
    pub created_at: DateTime<Utc>,
}

impl CardModel {
    pub fn needs_you(&self) -> bool {
        self.attention.is_some()
    }
}

fn status_of(chat: &Chat, session: Option<&Session>, now: DateTime<Utc>) -> CardStatus {
    match zeron_proto::view::display_status(chat, session, now) {
        ChatIndicator::Working => CardStatus::Working,
        ChatIndicator::AwaitingInput | ChatIndicator::AwaitingApproval => CardStatus::NeedsYou,
        ChatIndicator::Errored | ChatIndicator::Limited => CardStatus::Failed,
        ChatIndicator::Completed => CardStatus::Done,
        ChatIndicator::Idle => CardStatus::Idle,
    }
}

/// The oldest waiting item for the card: its own, or one from a chat nested
/// below it (the engine stamps those with this card's id).
fn card_attention(
    card_chat: &str,
    conductor: &str,
    items: &[AttentionItem],
) -> Option<AttentionItem> {
    items
        .iter()
        .filter(|item| {
            item.conductor.as_ref().is_some_and(|link| {
                link.conductor_id == conductor && link.card_chat_id == card_chat
            })
        })
        .min_by_key(|item| item.created_at)
        .cloned()
}

/// Build the card for `chat` (a depth-1 child of the main chat).
pub fn card_model(
    chat: &Chat,
    session: Option<&Session>,
    conductor_id: &str,
    attention: &[AttentionItem],
    nested: usize,
    steps: &[String],
    now: DateTime<Utc>,
) -> CardModel {
    let attention = card_attention(&chat.id, conductor_id, attention);
    let status = if attention.is_some() {
        CardStatus::NeedsYou
    } else {
        status_of(chat, session, now)
    };
    let from = steps.len().saturating_sub(CARD_STEPS);
    CardModel {
        chat_id: chat.id.clone(),
        title: chat
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("Untitled")
            .to_string(),
        status,
        steps: steps[from..].to_vec(),
        nested,
        attention,
        created_at: chat.created_at,
    }
}

/// How many chats sit below `root` in the parent links (not counting `root`).
fn descendants(root: &str, chats: &[Chat]) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for chat in chats
            .iter()
            .filter(|c| !c.archived && c.parent_chat_id.as_deref() == Some(parent))
        {
            if chat.id != root && seen.insert(chat.id.as_str()) {
                frontier.push(chat.id.as_str());
            }
        }
    }
    seen.len()
}

/// Cards for a conductor's main chat: only its direct, unarchived children,
/// oldest first. `steps` supplies a chat's recent steps by id.
pub fn cards_for_main_chat(
    conductor: &Conductor,
    chats: &[Chat],
    sessions: &[Session],
    attention: &[AttentionItem],
    steps: &dyn Fn(&str) -> Vec<String>,
    now: DateTime<Utc>,
) -> Vec<CardModel> {
    let mut children: Vec<&Chat> = chats
        .iter()
        .filter(|c| {
            !c.archived && c.parent_chat_id.as_deref() == Some(conductor.main_chat_id.as_str())
        })
        .collect();
    children.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    children
        .into_iter()
        .map(|chat| {
            let session = sessions.iter().find(|s| s.chat_id == chat.id);
            card_model(
                chat,
                session,
                &conductor.id,
                attention,
                descendants(&chat.id, chats),
                &steps(&chat.id),
                now,
            )
        })
        .collect()
}

/// Cards for `chat_id` when it is a live conductor's main chat, else none.
/// A card's one step is the sub-agent's last-message preview.
pub fn cards_for_chat(
    chat_id: &str,
    conductors: &[Conductor],
    chats: &[Chat],
    sessions: &[Session],
    attention: &[AttentionItem],
    now: DateTime<Utc>,
) -> Vec<CardModel> {
    let Some(conductor) = conductors
        .iter()
        .find(|c| c.main_chat_id == chat_id && !c.archived)
    else {
        return Vec::new();
    };
    let steps = |id: &str| -> Vec<String> {
        chats
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| c.last_message_preview.as_deref())
            .map(clean_preview)
            .filter(|line| !line.is_empty())
            .map(|line| vec![line])
            .unwrap_or_default()
    };
    cards_for_main_chat(conductor, chats, sessions, attention, &steps, now)
}

/// One plain line from a chat's last-message preview: markdown marks and the
/// live "Running…" tail dropped.
pub fn clean_preview(preview: &str) -> String {
    let plain: String = preview
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '#'))
        .collect();
    let line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    line.trim_end_matches("Running…")
        .trim_end_matches("Running...")
        .trim()
        .to_string()
}

/// The transcript entry a card follows: the last one written at or before the
/// sub-agent started (`entry_times` in epoch ms, transcript order). A card
/// older than every entry follows the first one.
pub fn anchor_entry(started_ms: i64, entry_times: &[i64]) -> Option<usize> {
    if entry_times.is_empty() {
        return None;
    }
    Some(entry_times.iter().rposition(|t| *t <= started_ms).unwrap_or(0))
}

/// Whether anything in the conductor waits on the user: an item linked to one
/// of its cards, or one in the main chat itself.
pub fn conductor_needs_you(conductor: &Conductor, attention: &[AttentionItem]) -> bool {
    attention.iter().any(|item| {
        item.conductor
            .as_ref()
            .is_some_and(|link| link.conductor_id == conductor.id)
            || item.chat_id.as_deref() == Some(conductor.main_chat_id.as_str())
    })
}

/// What the sidebar shows for one conductor.
#[derive(Debug, Clone, PartialEq)]
pub struct ConductorEntry {
    pub id: String,
    pub name: String,
    pub main_chat_id: String,
    pub repo_count: usize,
    pub needs_you: bool,
}

/// Live conductors, oldest first. Archived ones are left out of the sidebar.
pub fn sidebar_entries(
    conductors: &[Conductor],
    attention: &[AttentionItem],
) -> Vec<ConductorEntry> {
    let mut live: Vec<&Conductor> = conductors.iter().filter(|c| !c.archived).collect();
    live.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    live.into_iter()
        .map(|c| ConductorEntry {
            id: c.id.clone(),
            name: c.name.clone(),
            main_chat_id: c.main_chat_id.clone(),
            repo_count: c.space_ids.len(),
            needs_you: conductor_needs_you(c, attention),
        })
        .collect()
}

/// What an empty conductor chat shows before the first message.
pub struct ConductorBlank {
    pub name: String,
    pub summary: &'static str,
    pub job: String,
    pub hint: &'static str,
}

const BLANK_SUMMARY: &str = "A conductor is one long-lived chat for a folder of repos. You talk only to it. It starts and steers sub-agents, then reports back with a short verdict, and it keeps that thread across sessions.";

const BLANK_HINT: &str = "Standing instructions are in the session menu at the top right. Edit them any time. The conductor is told when they change.";

pub fn conductor_blank(conductor: &Conductor) -> ConductorBlank {
    let job = conductor
        .purpose
        .as_deref()
        .map(str::trim)
        .filter(|purpose| !purpose.is_empty())
        .map(|purpose| format!("Responsible for {purpose}."))
        .unwrap_or_else(|| {
            "No specific job yet. Say what it should own, such as reviewing pull requests on one repo."
                .into()
        });
    ConductorBlank {
        name: conductor.name.clone(),
        summary: BLANK_SUMMARY,
        job,
        hint: BLANK_HINT,
    }
}

/// The explanation for a conductor's main chat while it has no messages and
/// no sub-agents yet. `None` once the transcript is still loading, or once
/// the chat has something to show.
pub fn conductor_blank_for(
    chat_id: &str,
    transcript_ready: bool,
    transcript_empty: bool,
    conductors: &[Conductor],
    chats: &[Chat],
) -> Option<ConductorBlank> {
    if !transcript_ready || !transcript_empty {
        return None;
    }
    let conductor = conductors
        .iter()
        .find(|conductor| !conductor.archived && conductor.main_chat_id == chat_id)?;
    let has_child = chats.iter().any(|chat| {
        !chat.archived && chat.parent_chat_id.as_deref() == Some(chat_id)
    });
    if has_child {
        return None;
    }
    Some(conductor_blank(conductor))
}

/// The line under a conductor's name.
pub fn repo_summary(count: usize) -> String {
    match count {
        0 => "No git repos yet".into(),
        1 => "1 repo".into(),
        n => format!("{n} repos"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use zeron_proto::attention::{AttentionAction, AttentionKind, ConductorLink};

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + minute * 60, 0).unwrap()
    }

    fn conductor() -> Conductor {
        Conductor {
            id: "cond".into(),
            name: "Acme".into(),
            purpose: None,
            folder: "/code/acme".into(),
            device_id: "dev".into(),
            main_chat_id: "main".into(),
            space_ids: vec!["s1".into()],
            config: None,
            notes: String::new(),
            instructions: String::new(),
            archived: false,
            created_at: at(0),
        }
    }

    fn chat(id: &str, parent: Option<&str>, minute: i64) -> Chat {
        let mut chat: Chat = serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "archived": false,
            "createdAt": at(minute),
        }))
        .unwrap();
        chat.parent_chat_id = parent.map(str::to_string);
        chat.title = Some(id.to_string());
        chat
    }

    fn waiting(card: &str, chat_id: &str, minute: i64) -> AttentionItem {
        AttentionItem {
            id: format!("approval:{chat_id}"),
            kind: AttentionKind::Approval,
            chat_id: Some(chat_id.into()),
            title: chat_id.into(),
            space_id: None,
            project: None,
            harness: None,
            created_at: at(minute),
            risk: None,
            summary: String::new(),
            actions: vec![AttentionAction::Open],
            request_id: None,
            approval: None,
            agent_run: None,
            conductor: Some(ConductorLink {
                conductor_id: "cond".into(),
                card_chat_id: card.into(),
            }),
        }
    }

    #[test]
    fn cards_list_only_depth_one_children_oldest_first() {
        let chats = vec![
            chat("main", None, 0),
            chat("b", Some("main"), 2),
            chat("a", Some("main"), 1),
            chat("a1", Some("a"), 3),
            chat("a2", Some("a1"), 4),
            chat("other", None, 5),
        ];
        let cards = cards_for_main_chat(&conductor(), &chats, &[], &[], &|_| vec![], at(10));
        let ids: Vec<_> = cards.iter().map(|c| c.chat_id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(cards[0].nested, 2);
        assert_eq!(cards[1].nested, 0);
    }

    #[test]
    fn archived_children_get_no_card() {
        let mut gone = chat("gone", Some("main"), 1);
        gone.archived = true;
        let chats = vec![chat("main", None, 0), gone];
        assert!(
            cards_for_main_chat(&conductor(), &chats, &[], &[], &|_| vec![], at(10)).is_empty()
        );
    }

    #[test]
    fn a_card_keeps_the_last_three_steps() {
        let steps: Vec<String> = (1..=5).map(|n| format!("step {n}")).collect();
        let card = card_model(
            &chat("a", Some("main"), 1),
            None,
            "cond",
            &[],
            0,
            &steps,
            at(5),
        );
        assert_eq!(card.steps, ["step 3", "step 4", "step 5"]);
        let few = card_model(
            &chat("a", Some("main"), 1),
            None,
            "cond",
            &[],
            0,
            &steps[..1],
            at(5),
        );
        assert_eq!(few.steps, ["step 1"]);
    }

    #[test]
    fn a_waiting_descendant_lights_its_depth_one_card_with_the_oldest_item() {
        let chats = vec![
            chat("main", None, 0),
            chat("a", Some("main"), 1),
            chat("a1", Some("a"), 2),
            chat("b", Some("main"), 3),
        ];
        let items = vec![waiting("a", "a1", 9), waiting("a", "a", 5)];
        let cards = cards_for_main_chat(&conductor(), &chats, &[], &items, &|_| vec![], at(10));
        assert_eq!(cards[0].status, CardStatus::NeedsYou);
        assert_eq!(cards[0].attention.as_ref().unwrap().id, "approval:a");
        assert!(!cards[1].needs_you());
        assert_ne!(cards[1].status, CardStatus::NeedsYou);
    }

    #[test]
    fn needs_you_covers_cards_and_the_main_chat_but_not_other_conductors() {
        let c = conductor();
        assert!(!conductor_needs_you(&c, &[]));
        assert!(conductor_needs_you(&c, &[waiting("a", "a1", 1)]));
        let mut main_item = waiting("a", "main", 1);
        main_item.conductor = None;
        assert!(conductor_needs_you(&c, &[main_item]));
        let mut foreign = waiting("a", "x", 1);
        foreign.conductor.as_mut().unwrap().conductor_id = "other".into();
        assert!(!conductor_needs_you(&c, &[foreign]));
    }

    #[test]
    fn sidebar_hides_archived_conductors_and_orders_oldest_first() {
        let mut older = conductor();
        older.id = "old".into();
        older.created_at = at(-5);
        let mut archived = conductor();
        archived.id = "arch".into();
        archived.archived = true;
        let entries = sidebar_entries(&[conductor(), archived, older], &[]);
        let ids: Vec<_> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["old", "cond"]);
    }

    #[test]
    fn a_card_follows_the_last_entry_before_its_start() {
        assert_eq!(anchor_entry(50, &[]), None);
        assert_eq!(anchor_entry(50, &[10, 40, 60]), Some(1));
        assert_eq!(anchor_entry(40, &[10, 40, 60]), Some(1));
        assert_eq!(anchor_entry(5, &[10, 40]), Some(0));
        assert_eq!(anchor_entry(99, &[10, 40]), Some(1));
    }

    #[test]
    fn previews_lose_markdown_and_the_running_tail() {
        assert_eq!(
            clean_preview("**Verdict: safe**\n with `caveats`. Running…"),
            "Verdict: safe with caveats."
        );
        assert_eq!(clean_preview("Running…"), "");
    }

    #[test]
    fn only_a_conductor_main_chat_gets_cards() {
        let chats = vec![chat("main", None, 0), chat("a", Some("main"), 1)];
        let conductors = [conductor()];
        assert_eq!(
            cards_for_chat("main", &conductors, &chats, &[], &[], at(5)).len(),
            1
        );
        assert!(cards_for_chat("a", &conductors, &chats, &[], &[], at(5)).is_empty());
    }

    #[test]
    fn repo_summary_has_an_empty_state() {
        assert_eq!(repo_summary(0), "No git repos yet");
        assert_eq!(repo_summary(1), "1 repo");
        assert_eq!(repo_summary(3), "3 repos");
    }

    #[test]
    fn an_empty_main_chat_explains_the_conductor_and_its_job() {
        let mut conductor = conductor();
        conductor.purpose = Some("Review pull requests on api".into());
        let chats = vec![chat("main", None, 0)];
        let blank = conductor_blank_for("main", true, true, &[conductor.clone()], &chats).unwrap();
        assert_eq!(blank.name, "Acme");
        assert!(blank.summary.contains("long-lived"));
        assert!(blank.summary.contains("sub-agents"));
        assert_eq!(blank.job, "Responsible for Review pull requests on api.");
        assert!(blank.hint.contains("top right"));
        assert!(conductor_blank_for("main", false, true, &[conductor.clone()], &chats).is_none());
        assert!(conductor_blank_for("main", true, false, &[conductor.clone()], &chats).is_none());
        let with_child = vec![chat("main", None, 0), chat("a", Some("main"), 1)];
        assert!(conductor_blank_for("main", true, true, &[conductor], &with_child).is_none());
        assert!(conductor_blank_for("plain", true, true, &[], &chats).is_none());
    }
}
