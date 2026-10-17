//! The inbox's keyboard model, kept pure: which key does what, the order
//! items are shown and walked in, and where the selection lands when the
//! queue changes under it.

use zeron_proto::attention::{AttentionItem, AttentionKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboxCommand {
    Next,
    Previous,
    Approve,
    /// Decline an approval; on anything else, mark it seen.
    Decline,
    Open,
    ApproveLowRisk,
}

/// `j`/`k` (or the arrows) move, `a` approves, `d` declines, Enter opens,
/// Shift+A approves every low-risk item. Other modifiers pass through.
pub(crate) fn command_for(key: &str, shift: bool, other_modifiers: bool) -> Option<InboxCommand> {
    if other_modifiers {
        return None;
    }
    Some(match (key, shift) {
        ("j" | "down", false) => InboxCommand::Next,
        ("k" | "up", false) => InboxCommand::Previous,
        ("a", true) | ("A", _) => InboxCommand::ApproveLowRisk,
        ("a", false) => InboxCommand::Approve,
        ("d", false) => InboxCommand::Decline,
        ("enter", false) => InboxCommand::Open,
        _ => return None,
    })
}

/// Display groups, in order.
pub(crate) const GROUP_ORDER: [AttentionKind; 6] = [
    AttentionKind::Approval,
    AttentionKind::Question,
    AttentionKind::Failed,
    AttentionKind::Limited,
    AttentionKind::TimedOut,
    AttentionKind::Finished,
];

/// Items grouped by kind in [`GROUP_ORDER`], keeping the queue order
/// (risk, then age) inside each group. Empty groups are left out.
pub(crate) fn grouped(items: &[AttentionItem]) -> Vec<(AttentionKind, Vec<&AttentionItem>)> {
    GROUP_ORDER
        .iter()
        .map(|kind| {
            (
                *kind,
                items.iter().filter(|i| i.kind == *kind).collect::<Vec<_>>(),
            )
        })
        .filter(|(_, group)| !group.is_empty())
        .collect()
}

/// Item ids in the order they are shown, which is the order j/k walk.
pub(crate) fn display_order(items: &[AttentionItem]) -> Vec<String> {
    grouped(items)
        .into_iter()
        .flat_map(|(_, group)| group.into_iter().map(|item| item.id.clone()))
        .collect()
}

/// The selection after moving `delta` rows, clamped to the ends. With
/// nothing selected, any move lands on the first row.
pub(crate) fn step(order: &[String], selected: Option<&str>, delta: isize) -> Option<String> {
    let current = selected.and_then(|id| order.iter().position(|o| o == id));
    let next = match current {
        None => 0,
        Some(at) => at
            .saturating_add_signed(delta)
            .min(order.len().saturating_sub(1)),
    };
    order.get(next).cloned()
}

/// Keep the selection on its item while it lasts; once answered, land on
/// the row that took its place (the next one), or the last row.
pub(crate) fn reconcile(
    order: &[String],
    selected: Option<&str>,
    last_index: usize,
) -> Option<String> {
    if let Some(id) = selected
        && order.iter().any(|o| o == id)
    {
        return Some(id.to_string());
    }
    if order.is_empty() {
        return None;
    }
    order.get(last_index.min(order.len() - 1)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use zeron_proto::ApprovalRisk;
    use zeron_proto::attention::AttentionAction;

    fn item(id: &str, kind: AttentionKind, risk: Option<ApprovalRisk>) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind,
            chat_id: Some(format!("chat-{id}")),
            title: id.into(),
            space_id: None,
            project: None,
            harness: None,
            created_at: Utc.with_ymd_and_hms(2026, 10, 17, 9, 0, 0).unwrap(),
            risk,
            summary: String::new(),
            actions: vec![AttentionAction::Open],
            request_id: None,
            approval: None,
            agent_run: None,
        }
    }

    fn ids(order: &[String]) -> Vec<&str> {
        order.iter().map(String::as_str).collect()
    }

    #[test]
    fn keys_map_to_commands() {
        use InboxCommand::*;
        assert_eq!(command_for("j", false, false), Some(Next));
        assert_eq!(command_for("down", false, false), Some(Next));
        assert_eq!(command_for("k", false, false), Some(Previous));
        assert_eq!(command_for("up", false, false), Some(Previous));
        assert_eq!(command_for("a", false, false), Some(Approve));
        assert_eq!(command_for("a", true, false), Some(ApproveLowRisk));
        assert_eq!(command_for("A", true, false), Some(ApproveLowRisk));
        assert_eq!(command_for("d", false, false), Some(Decline));
        assert_eq!(command_for("enter", false, false), Some(Open));
        assert_eq!(
            command_for("a", false, true),
            None,
            "cmd-a stays select-all"
        );
        assert_eq!(command_for("x", false, false), None);
        assert_eq!(command_for("j", true, false), None);
    }

    #[test]
    fn groups_follow_kind_order_and_keep_queue_order_inside() {
        // Queue order interleaves a question between approval risks.
        let items = vec![
            item("high", AttentionKind::Approval, Some(ApprovalRisk::High)),
            item("question", AttentionKind::Question, None),
            item("low", AttentionKind::Approval, Some(ApprovalRisk::Low)),
            item("failed", AttentionKind::Failed, None),
            item("done", AttentionKind::Finished, None),
        ];
        let groups = grouped(&items);
        let kinds: Vec<AttentionKind> = groups.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            [
                AttentionKind::Approval,
                AttentionKind::Question,
                AttentionKind::Failed,
                AttentionKind::Finished
            ]
        );
        assert_eq!(
            ids(&display_order(&items)),
            ["high", "low", "question", "failed", "done"]
        );
    }

    #[test]
    fn j_and_k_walk_the_display_order_and_stop_at_the_ends() {
        let order: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(step(&order, None, 1).as_deref(), Some("a"));
        assert_eq!(step(&order, None, -1).as_deref(), Some("a"));
        assert_eq!(step(&order, Some("a"), 1).as_deref(), Some("b"));
        assert_eq!(step(&order, Some("c"), 1).as_deref(), Some("c"));
        assert_eq!(step(&order, Some("a"), -1).as_deref(), Some("a"));
        assert_eq!(step(&order, Some("gone"), 1).as_deref(), Some("a"));
        assert_eq!(step(&[], None, 1), None);
    }

    #[test]
    fn an_answered_item_hands_the_selection_to_the_next_row() {
        let before: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(reconcile(&before, Some("b"), 1).as_deref(), Some("b"));
        let after: Vec<String> = ["a", "c"].map(String::from).to_vec();
        assert_eq!(reconcile(&after, Some("b"), 1).as_deref(), Some("c"));
        let tail: Vec<String> = ["a"].map(String::from).to_vec();
        assert_eq!(reconcile(&tail, Some("c"), 2).as_deref(), Some("a"));
        assert_eq!(reconcile(&[], Some("a"), 0), None);
        assert_eq!(reconcile(&before, None, 0).as_deref(), Some("a"));
    }
}
