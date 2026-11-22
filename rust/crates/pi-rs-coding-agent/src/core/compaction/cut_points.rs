//! Cut point detection for context compaction.
//!
//! Ports upstream `findCutPoint`. A cut never lands on a tool result: tool
//! results follow their tool call, so keeping from a valid cut point keeps any
//! trailing results attached to the assistant message that produced them.

use super::{MessageValue, estimate_message_tokens, role_of};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CutPoint {
    /// Index of the first message to keep.
    pub index: usize,
    /// User-like message starting the turn that contains `index`, if the cut
    /// falls in the middle of a turn.
    pub turn_start_index: Option<usize>,
    /// Whether the cut splits a turn (the cut itself is not a turn start).
    pub is_split_turn: bool,
}

/// Whether a role may be used as a cut point (upstream `isCutPointMessage`).
fn is_cut_point_role(role: &str) -> bool {
    matches!(
        role,
        "user" | "assistant" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

/// Whether a role starts a new turn (upstream `isTurnStartMessage`).
fn is_turn_start_role(role: &str) -> bool {
    matches!(
        role,
        "user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

/// Find a cut point over the whole message list.
pub fn find_cut_point(messages: &[MessageValue], keep_recent_tokens: usize) -> CutPoint {
    find_cut_point_in_range(messages, 0, messages.len(), keep_recent_tokens)
}

/// Find a cut point in `[start_index, end_index)`.
///
/// Walks backwards from the newest message accumulating estimated tokens until
/// the budget is met, then advances to the closest valid cut point at or after
/// that message. Metadata-only entries are implicitly absent here: every
/// payload in `messages` is context-visible, so no backward metadata scan is
/// needed (upstream scans projected session entries for that case).
pub fn find_cut_point_in_range(
    messages: &[MessageValue],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: usize,
) -> CutPoint {
    let start = start_index.min(messages.len());
    let end = end_index.min(messages.len()).max(start);
    if start == end {
        return CutPoint {
            index: start,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let cut_points: Vec<usize> = (start..end)
        .filter(|&index| is_cut_point_role(role_of(&messages[index])))
        .collect();
    if cut_points.is_empty() {
        return CutPoint {
            index: start,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated = 0;
    let mut cut_index = cut_points[0];
    for index in (start..end).rev() {
        let tokens = estimate_message_tokens(&messages[index]);
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            cut_index = cut_points
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
                .or_else(|| cut_points.last().copied())
                .unwrap_or(cut_index);
            break;
        }
    }

    let starts_turn = is_turn_start_role(role_of(&messages[cut_index]));
    let turn_start_index = if starts_turn {
        None
    } else {
        (start..=cut_index)
            .rev()
            .find(|&index| is_turn_start_role(role_of(&messages[index])))
    };
    CutPoint {
        index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user(text: &str) -> MessageValue {
        json!({"role": "user", "content": text})
    }

    fn assistant(text: &str) -> MessageValue {
        json!({
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stopReason": "stop"
        })
    }

    fn tool_result(text: &str) -> MessageValue {
        json!({
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "read",
            "content": [{"type": "text", "text": text}]
        })
    }

    #[test]
    fn keeps_everything_when_budget_is_large() {
        let messages = vec![user("1"), assistant("a"), user("2"), assistant("b")];
        let cut = find_cut_point(&messages, 50_000);
        assert_eq!(
            cut,
            CutPoint {
                index: 0,
                turn_start_index: None,
                is_split_turn: false
            }
        );
    }

    #[test]
    fn returns_start_when_no_valid_cut_points() {
        let messages = vec![tool_result("only a result")];
        let cut = find_cut_point(&messages, 1);
        assert_eq!(cut.index, 0);
        assert!(!cut.is_split_turn);
    }

    #[test]
    fn never_cuts_on_a_tool_result() {
        let messages = vec![
            user("read the file"),
            json!({
                "role": "assistant",
                "content": [{"type": "toolCall", "id": "c1", "name": "read", "arguments": {"path": "big.txt"}}],
                "stopReason": "toolUse"
            }),
            tool_result(&"x".repeat(8000)),
        ];
        let cut = find_cut_point(&messages, 1000);
        assert_eq!(cut.index, 1);
        assert_ne!(role_of(&messages[cut.index]), "toolResult");
        assert_eq!(role_of(&messages[cut.index + 1]), "toolResult");
        assert!(cut.is_split_turn);
        assert_eq!(cut.turn_start_index, Some(0));
    }

    #[test]
    fn marks_assistant_cut_as_split_turn() {
        let messages = vec![
            user("turn 1"),
            assistant("a1"),
            user("turn 2"),
            assistant("a2-1"),
            assistant("a2-2"),
            assistant("a2-3"),
        ];
        let cut = find_cut_point(&messages, 8);
        if role_of(&messages[cut.index]) == "assistant" {
            assert!(cut.is_split_turn);
            assert_eq!(cut.turn_start_index, Some(2));
        } else {
            assert!(!cut.is_split_turn);
        }
    }

    #[test]
    fn respects_range_bounds() {
        let messages = vec![user("0"), user("1"), user("2"), user("3")];
        let cut = find_cut_point_in_range(&messages, 1, 3, 0);
        assert_eq!(cut.index, 2);
        let empty = find_cut_point_in_range(&messages, 3, 3, 0);
        assert_eq!(empty.index, 3);
        assert!(!empty.is_split_turn);
    }

    #[test]
    fn custom_messages_count_towards_the_budget_and_start_turns() {
        let messages = vec![
            user("hi"),
            assistant("hello"),
            json!({"role": "custom", "customType": "note", "content": "x".repeat(4000)}),
            assistant("ok"),
        ];
        let tiny = find_cut_point(&messages, 1);
        assert_eq!(tiny.index, 3);
        assert!(tiny.is_split_turn);
        assert_eq!(tiny.turn_start_index, Some(2));
        let larger = find_cut_point(&messages, 2);
        assert_eq!(larger.index, 2);
        assert!(!larger.is_split_turn);
    }

    #[test]
    fn cut_point_is_monotonic_in_budget() {
        let messages = vec![
            user("aaaa"),
            assistant("bbbb"),
            user("cccc"),
            assistant("dddd"),
        ];
        let mut previous = messages.len();
        for budget in 0..40 {
            let index = find_cut_point(&messages, budget).index;
            assert!(index <= previous);
            previous = index;
        }
    }
}
