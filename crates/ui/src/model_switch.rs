//! Model switch dividers: a quiet `claude-opus-5-5 → claude-sonnet-5-5` line
//! above the prompt whose turn ran on a different model or reasoning effort
//! than the turn before it.
//!
//! Read off the [`zeron_doc::TurnStats`] each finished turn is stamped with,
//! so a chat shows its switches on every device and after a restart. The turn
//! still running has no stamp yet and stands in with the chat's current pick.

use zeron_doc::{MessageRole, SessionMessageEntry};

/// What one turn ran on. `None` fields are unknown and never count as a
/// change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnModel {
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The divider label for a turn that ran on `to` after one that ran on
/// `from`, showing only what changed. Harness changes are left out: those
/// are different agents, not a switch.
pub fn label(from: &TurnModel, to: &TurnModel) -> Option<String> {
    fn changed<'a>(a: &'a Option<String>, b: &'a Option<String>) -> Option<(&'a str, &'a str)> {
        match (a, b) {
            (Some(a), Some(b)) if a != b => Some((a, b)),
            _ => None,
        }
    }
    if changed(&from.harness, &to.harness).is_some() {
        return None;
    }
    match (
        changed(&from.model, &to.model),
        changed(&from.effort, &to.effort),
    ) {
        (Some((a, b)), None) => Some(format!("{a} → {b}")),
        (Some((a, b)), Some((ea, eb))) => Some(format!("{a} {ea} → {b} {eb}")),
        (None, Some((ea, eb))) => Some(format!("Effort {ea} → {eb}")),
        (None, None) => None,
    }
}

/// Divider labels keyed by the index of the user entry that opens a switched
/// turn. `live` is the chat's current pick, standing in for the last turn
/// while nothing in it is stamped.
pub fn dividers(entries: &[SessionMessageEntry], live: &TurnModel) -> Vec<(usize, String)> {
    let starts: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.role == MessageRole::User)
        .map(|(ix, _)| ix)
        .collect();
    let mut out = Vec::new();
    let mut previous: Option<TurnModel> = None;
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(entries.len());
        let last = n + 1 == starts.len();
        let Some(current) =
            stamped(&entries[start + 1..end]).or_else(|| last.then(|| live.clone()))
        else {
            continue;
        };
        if let Some(previous) = &previous
            && let Some(label) = label(previous, &current)
        {
            out.push((start, label));
        }
        // Unknown fields carry the last known value, so a partial stamp never
        // reads as a switch on the next turn.
        let merged = match previous.take() {
            Some(previous) => TurnModel {
                harness: current.harness.or(previous.harness),
                model: current.model.or(previous.model),
                effort: current.effort.or(previous.effort),
            },
            None => current,
        };
        previous = Some(merged);
    }
    out
}

/// The latest stamp among a turn's replies, as the receipt reads it.
fn stamped(replies: &[SessionMessageEntry]) -> Option<TurnModel> {
    let stamps: Vec<&zeron_doc::TurnStats> = replies
        .iter()
        .filter_map(|e| e.turn_stats.as_ref())
        .collect();
    if stamps.is_empty() {
        return None;
    }
    let latest = |pick: fn(&zeron_doc::TurnStats) -> Option<&String>| {
        stamps.iter().rev().find_map(|s| pick(s)).cloned()
    };
    Some(TurnModel {
        harness: latest(|s| s.harness.as_ref()),
        model: latest(|s| s.model.as_ref()),
        effort: latest(|s| s.effort.as_ref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(harness: &str, model: &str, effort: Option<&str>) -> TurnModel {
        TurnModel {
            harness: Some(harness.into()),
            model: Some(model.into()),
            effort: effort.map(Into::into),
        }
    }

    fn entry(id: &str, role: MessageRole, stats: Option<TurnModel>) -> SessionMessageEntry {
        let mut entry: SessionMessageEntry = serde_json::from_value(serde_json::json!({
            "id": id,
            "role": if role == MessageRole::User { "user" } else { "assistant" },
            "parts": [],
            "createdAt": 0,
            "deviceId": "mac",
        }))
        .unwrap();
        entry.turn_stats = stats.map(|m| zeron_doc::TurnStats {
            harness: m.harness,
            model: m.model,
            effort: m.effort,
            ..Default::default()
        });
        entry
    }

    #[test]
    fn labels_show_only_what_changed() {
        let opus_high = model("claude-code", "claude-opus-5-5", Some("high"));
        let opus_xhigh = model("claude-code", "claude-opus-5-5", Some("xhigh"));
        let sonnet_high = model("claude-code", "claude-sonnet-5-5", Some("high"));
        let sonnet_low = model("claude-code", "claude-sonnet-5-5", Some("low"));
        assert_eq!(label(&opus_high, &opus_high), None);
        assert_eq!(
            label(&opus_high, &opus_xhigh).as_deref(),
            Some("Effort high → xhigh")
        );
        assert_eq!(
            label(&opus_high, &sonnet_high).as_deref(),
            Some("claude-opus-5-5 → claude-sonnet-5-5")
        );
        assert_eq!(
            label(&opus_xhigh, &sonnet_low).as_deref(),
            Some("claude-opus-5-5 xhigh → claude-sonnet-5-5 low")
        );
        assert_eq!(
            label(&opus_high, &model("codex", "gpt-5.5", Some("high"))),
            None
        );
        assert_eq!(
            label(&opus_high, &model("claude-code", "claude-opus-5-5", None)),
            None
        );
    }

    #[test]
    fn dividers_sit_on_the_prompt_of_each_switched_turn() {
        let opus = model("claude-code", "claude-opus-5-5", Some("high"));
        let sonnet = model("claude-code", "claude-sonnet-5-5", Some("high"));
        let entries = vec![
            entry("u1", MessageRole::User, None),
            entry("a1", MessageRole::Assistant, Some(opus.clone())),
            entry("u2", MessageRole::User, None),
            entry("a2", MessageRole::Assistant, Some(opus.clone())),
            entry("u3", MessageRole::User, None),
            entry("a3", MessageRole::Assistant, None),
            entry("a3b", MessageRole::Assistant, Some(sonnet.clone())),
            entry("u4", MessageRole::User, None),
            entry("a4", MessageRole::Assistant, Some(opus.clone())),
        ];
        assert_eq!(
            dividers(&entries, &TurnModel::default()),
            vec![
                (4, "claude-opus-5-5 → claude-sonnet-5-5".to_owned()),
                (7, "claude-sonnet-5-5 → claude-opus-5-5".to_owned()),
            ]
        );
    }

    #[test]
    fn the_running_turn_uses_the_live_pick_and_unstamped_turns_carry_over() {
        let opus = model("claude-code", "claude-opus-5-5", Some("high"));
        let entries = vec![
            entry("u1", MessageRole::User, None),
            entry("a1", MessageRole::Assistant, Some(opus.clone())),
            // A turn from before stamps existed.
            entry("u2", MessageRole::User, None),
            entry("a2", MessageRole::Assistant, None),
            entry("u3", MessageRole::User, None),
        ];
        let live = model("claude-code", "claude-opus-5-5", Some("xhigh"));
        assert_eq!(
            dividers(&entries, &live),
            vec![(4, "Effort high → xhigh".to_owned())]
        );
        // A partial stamp keeps the effort it doesn't name.
        let partial = TurnModel {
            effort: None,
            ..opus.clone()
        };
        let entries = vec![
            entry("u1", MessageRole::User, None),
            entry("a1", MessageRole::Assistant, Some(opus.clone())),
            entry("u2", MessageRole::User, None),
            entry("a2", MessageRole::Assistant, Some(partial)),
            entry("u3", MessageRole::User, None),
            entry("a3", MessageRole::Assistant, Some(opus)),
        ];
        assert!(dividers(&entries, &TurnModel::default()).is_empty());
    }
}
