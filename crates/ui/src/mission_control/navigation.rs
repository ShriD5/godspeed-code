//! Mission Control's keyboard model, kept pure: which key does what, how
//! focus moves across the card grid, and where it lands when the board
//! changes under it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Move {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Move(Move),
    /// Open the focused card's chat (closing Mission Control).
    Open,
    /// Approve the focused card's pending low-risk approval, if any.
    Approve,
    /// Focus the `/` filter.
    Filter,
    Close,
}

/// Board keys: arrows or `hjkl` move, Enter opens, `a` approves, `/`
/// filters, Esc closes. Other modifiers pass through (⌘A stays select-all).
pub(crate) fn command_for(key: &str, shift: bool, other_modifiers: bool) -> Option<Command> {
    if other_modifiers {
        return None;
    }
    Some(match (key, shift) {
        ("h" | "left", false) => Command::Move(Move::Left),
        ("l" | "right", false) => Command::Move(Move::Right),
        ("k" | "up", false) => Command::Move(Move::Up),
        ("j" | "down", false) => Command::Move(Move::Down),
        ("enter", false) => Command::Open,
        ("a", false) => Command::Approve,
        ("/", false) => Command::Filter,
        ("escape", _) => Command::Close,
        _ => return None,
    })
}

/// Keys the filter field passes up while it has focus: the arrows keep
/// moving across the board and Enter opens, so the field never traps the
/// keyboard. Letters (including `hjkl` and `a`) stay text.
pub(crate) fn filter_command_for(key: &str, other_modifiers: bool) -> Option<Command> {
    if other_modifiers {
        return None;
    }
    Some(match key {
        "up" => Command::Move(Move::Up),
        "down" => Command::Move(Move::Down),
        "enter" => Command::Open,
        "escape" => Command::Close,
        _ => return None,
    })
}

/// Focus after one move across `rows` (the grid as drawn, see
/// [`super::board::Board::rows`]). Left/Right walk the display order across
/// row ends; Up/Down keep the column, clamped to a shorter row. Moves stop
/// at the edges. With nothing focused, any move lands on the first card.
pub(crate) fn step(rows: &[Vec<String>], focused: Option<&str>, by: Move) -> Option<String> {
    let at = focused.and_then(|id| {
        rows.iter()
            .enumerate()
            .find_map(|(r, row)| row.iter().position(|card| card == id).map(|c| (r, c)))
    });
    let Some((r, c)) = at else {
        return rows.first()?.first().cloned();
    };
    match by {
        Move::Left | Move::Right => {
            let order: Vec<&String> = rows.iter().flatten().collect();
            let ix = rows[..r].iter().map(Vec::len).sum::<usize>() + c;
            let next = match by {
                Move::Left => ix.saturating_sub(1),
                _ => (ix + 1).min(order.len() - 1),
            };
            Some(order[next].clone())
        }
        Move::Up | Move::Down => {
            let target = match by {
                Move::Up => r.checked_sub(1),
                _ => (r + 1 < rows.len()).then_some(r + 1),
            };
            let Some(target) = target else {
                return Some(rows[r][c].clone());
            };
            let row = &rows[target];
            row.get(c.min(row.len().saturating_sub(1))).cloned()
        }
    }
}

/// Keep focus on its card while it lasts; once it leaves the board (filtered
/// out, archived, gone stale), land on the card that took its place, or the
/// last one.
pub(crate) fn reconcile(
    order: &[String],
    focused: Option<&str>,
    last_index: usize,
) -> Option<String> {
    if let Some(id) = focused
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

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|id| id.to_string()).collect())
            .collect()
    }

    #[test]
    fn keys_map_to_commands() {
        use Command::*;
        assert_eq!(
            command_for("h", false, false),
            Some(Move(super::Move::Left))
        );
        assert_eq!(
            command_for("left", false, false),
            Some(Move(super::Move::Left))
        );
        assert_eq!(
            command_for("l", false, false),
            Some(Move(super::Move::Right))
        );
        assert_eq!(command_for("k", false, false), Some(Move(super::Move::Up)));
        assert_eq!(
            command_for("down", false, false),
            Some(Move(super::Move::Down))
        );
        assert_eq!(command_for("enter", false, false), Some(Open));
        assert_eq!(command_for("a", false, false), Some(Approve));
        assert_eq!(command_for("/", false, false), Some(Filter));
        assert_eq!(command_for("escape", false, false), Some(Close));
        assert_eq!(command_for("a", false, true), None, "cmd-a passes through");
        assert_eq!(command_for("A", true, false), None, "no batch approve here");
        assert_eq!(command_for("x", false, false), None);
        assert_eq!(
            filter_command_for("down", false),
            Some(Move(super::Move::Down))
        );
        assert_eq!(filter_command_for("enter", false), Some(Open));
        assert_eq!(filter_command_for("j", false), None, "letters stay text");
        assert_eq!(
            filter_command_for("left", false),
            None,
            "the caret owns left/right"
        );
    }

    #[test]
    fn left_and_right_walk_the_display_order_across_rows() {
        let rows = grid(&[&["a", "b", "c"], &["d"], &["e", "f"]]);
        assert_eq!(step(&rows, Some("b"), Move::Right).as_deref(), Some("c"));
        assert_eq!(step(&rows, Some("c"), Move::Right).as_deref(), Some("d"));
        assert_eq!(step(&rows, Some("d"), Move::Left).as_deref(), Some("c"));
        assert_eq!(step(&rows, Some("a"), Move::Left).as_deref(), Some("a"));
        assert_eq!(step(&rows, Some("f"), Move::Right).as_deref(), Some("f"));
    }

    #[test]
    fn up_and_down_keep_the_column_and_clamp_to_short_rows() {
        let rows = grid(&[&["a", "b", "c"], &["d"], &["e", "f"]]);
        assert_eq!(step(&rows, Some("c"), Move::Down).as_deref(), Some("d"));
        assert_eq!(step(&rows, Some("d"), Move::Down).as_deref(), Some("e"));
        assert_eq!(step(&rows, Some("f"), Move::Up).as_deref(), Some("d"));
        assert_eq!(step(&rows, Some("b"), Move::Up).as_deref(), Some("b"));
        assert_eq!(step(&rows, Some("f"), Move::Down).as_deref(), Some("f"));
        let wide = grid(&[&["a", "b"], &["c", "d"]]);
        assert_eq!(step(&wide, Some("b"), Move::Down).as_deref(), Some("d"));
        assert_eq!(step(&wide, Some("c"), Move::Up).as_deref(), Some("a"));
    }

    #[test]
    fn with_nothing_focused_any_move_lands_on_the_first_card() {
        let rows = grid(&[&["a", "b"], &["c"]]);
        assert_eq!(step(&rows, None, Move::Down).as_deref(), Some("a"));
        assert_eq!(step(&rows, Some("gone"), Move::Left).as_deref(), Some("a"));
        assert_eq!(step(&[], None, Move::Right), None);
    }

    #[test]
    fn a_card_that_leaves_hands_focus_to_the_next() {
        let before: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(reconcile(&before, Some("b"), 1).as_deref(), Some("b"));
        let after: Vec<String> = ["a", "c"].map(String::from).to_vec();
        assert_eq!(reconcile(&after, Some("b"), 1).as_deref(), Some("c"));
        assert_eq!(reconcile(&after, Some("z"), 9).as_deref(), Some("c"));
        assert_eq!(reconcile(&[], Some("a"), 0), None);
        assert_eq!(reconcile(&before, None, 0).as_deref(), Some("a"));
    }
}
