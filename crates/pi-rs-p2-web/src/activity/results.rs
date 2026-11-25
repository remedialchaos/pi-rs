//! What a turn made for the person, after its answer (upstream `activity/Results.tsx`).

use super::model::{ResultPlacement, ToolActivityTable};

/// One result shown in the chat, identified by its tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Result {
    pub tool_id: String,
    pub tool_name: String,
    pub inline: bool,
}

/// The results a turn shows: calls whose tool contributes a `hasResult`, at their placement.
///
/// `detail_one` mirrors upstream detail 1: inline results join those after the answer.
pub fn results_for(
    calls: &[(String, String)],
    tools: &ToolActivityTable,
    detail_one: bool,
) -> Vec<Result> {
    calls
        .iter()
        .filter_map(
            |(id, name)| match tools.get(name).and_then(|tool| tool.result_placement) {
                Some(ResultPlacement::After) => Some(Result {
                    tool_id: id.clone(),
                    tool_name: name.clone(),
                    inline: true,
                }),
                Some(ResultPlacement::Inline) if detail_one => Some(Result {
                    tool_id: id.clone(),
                    tool_name: name.clone(),
                    inline: true,
                }),
                _ => None,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::model::ToolActivity;
    use super::*;

    #[test]
    fn results_respect_placement() {
        let mut tools = ToolActivityTable::new();
        tools.insert(
            "artifact".into(),
            ToolActivity {
                result_placement: Some(ResultPlacement::After),
                ..Default::default()
            },
        );
        tools.insert(
            "inline".into(),
            ToolActivity {
                result_placement: Some(ResultPlacement::Inline),
                ..Default::default()
            },
        );
        let calls = vec![
            ("1".into(), "artifact".into()),
            ("2".into(), "inline".into()),
            ("3".into(), "bash".into()),
        ];
        assert_eq!(results_for(&calls, &tools, false).len(), 1);
        assert_eq!(results_for(&calls, &tools, true).len(), 2);
    }
}
