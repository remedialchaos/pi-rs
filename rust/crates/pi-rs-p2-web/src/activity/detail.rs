//! What a step's detail shows (upstream `activity/model.ts` + `activity/Detail.tsx`).

use super::label::{Described, Label, LabelStyle};
use super::model::{
    ResultPlacement, Step, StepStatus, ToolActivityTable, call_status, first_line, last_line,
    parsed_args, short_argument,
};
use crate::state::chat_context::relative_path;
use serde_json::Value;

/// A nested call made by another tool (upstream `NestedCall`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub status: String,
    pub output: String,
    pub error: Option<String>,
    pub depth: usize,
}

/// A failed call's first output line, when it failed (upstream `failureOf`).
pub fn failure_of(step: &Step) -> Option<String> {
    if step.status == StepStatus::Error {
        let first = first_line(&step.output);
        if first.is_empty() { None } else { Some(first) }
    } else {
        None
    }
}

/// A step for the sheet: its tool's words, or the tool's name and short argument (upstream `describe`).
pub fn describe(step: &Step, _tools: &ToolActivityTable) -> Described {
    let failure = failure_of(step);
    Described {
        label: Label {
            kind: super::label::Kind::Tool,
            verb: Some(step.name.clone()),
            object: None,
            style: None,
            extra: None,
        },
        description: failure,
        mono: true,
    }
}

/// A step for the ticker: its label in the present tense while it runs (upstream `live`).
pub fn live(step: &Step, tools: &ToolActivityTable) -> Label {
    let described = describe(step, tools);
    if step.status == StepStatus::Running
        && let Some(brief) = tools.get(&step.name).and_then(|tool| tool.brief.clone())
    {
        return Label {
            verb: Some(brief),
            ..described.label
        };
    }
    described.label
}

/// A call's main argument for its row in the sheet, relative to the session's cwd.
pub fn argument_of(
    _name: &str,
    args: &serde_json::Map<String, Value>,
    cwd: Option<&str>,
) -> String {
    let argument = short_argument(args);
    relative_path(cwd, &argument)
}

/// Where a call's result for the person shows, if any (upstream `placement`).
pub fn placement(name: &str, tools: &ToolActivityTable) -> Option<ResultPlacement> {
    tools.get(name).and_then(|tool| tool.result_placement)
}

/// Consecutive calls of a script with the same tool and arguments, as one timeline entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallGroup {
    pub key: String,
    pub count: usize,
    pub label: Label,
    pub description: Option<String>,
    pub status: StepStatus,
}

/// A call and the calls it made, repeated identical calls in a row merged (upstream `groupCalls`).
pub fn group_calls(
    calls: &[NestedCall],
    tools: &ToolActivityTable,
    parent: &str,
) -> Vec<CallGroup> {
    let mut groups: Vec<CallGroup> = vec![];
    for call in calls {
        if let Some(last) = groups.last_mut()
            && last.label.verb.as_deref() == Some(call.name.as_str())
            && last.label.object.as_deref()
                == Some(short_argument(&parsed_args(&call.args)).as_str())
            && last.label.extra.as_deref() == (call.depth > 0).then_some("nested")
        {
            last.count += 1;
            last.status = merge_status(last.status, call_status(&call.status));
            continue;
        }
        let status = call_status(&call.status);
        groups.push(CallGroup {
            key: call.id.clone(),
            count: 1,
            label: Label {
                kind: super::label::Kind::Tool,
                verb: Some(call.name.clone()),
                object: Some(short_argument(&parsed_args(&call.args))).filter(|s| !s.is_empty()),
                style: Some(LabelStyle::Code),
                extra: (call.depth > 0).then(|| "nested".to_owned()),
            },
            description: call.error.clone().or_else(|| last_line(&call.output)),
            status,
        });
    }
    // Repeated calls use the parent tool's words for repeats, else the generic summary.
    for group in &mut groups {
        if group.count > 1 {
            let failed = group.count; // refined by callers when statuses are available
            group.description = tools
                .get(parent)
                .and_then(|tool| tool.repeated.map(|repeat| repeat(group.count, failed)))
                .or_else(|| {
                    Some(format!(
                        "used {} {} times",
                        group.label.verb.as_deref().unwrap_or("tool"),
                        group.count
                    ))
                });
        }
    }
    groups
}

fn merge_status(a: StepStatus, b: StepStatus) -> StepStatus {
    match (a, b) {
        (StepStatus::Running, _) | (_, StepStatus::Running) => StepStatus::Running,
        (StepStatus::Error, _) | (_, StepStatus::Error) => StepStatus::Error,
        _ => StepStatus::Done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_uses_failure_as_description() {
        let mut step = Step::new("1", "bash", StepStatus::Error);
        step.output = "exit 1".into();
        let described = describe(&step, &ToolActivityTable::new());
        assert_eq!(described.description.as_deref(), Some("exit 1"));
        assert!(described.mono);
    }

    #[test]
    fn argument_of_is_relative_to_cwd() {
        let mut args = serde_json::Map::new();
        args.insert(
            "path".into(),
            Value::String("/home/pi/project/src/a.rs".into()),
        );
        assert_eq!(
            argument_of("read", &args, Some("/home/pi/project")),
            "src/a.rs"
        );
    }

    #[test]
    fn group_calls_merges_identical_runs() {
        fn repeated(count: usize, failed: usize) -> String {
            format!("{count} runs, {failed} failed")
        }
        let mut tools = ToolActivityTable::new();
        tools.insert(
            "script".into(),
            super::super::model::ToolActivity {
                repeated: Some(repeated),
                ..Default::default()
            },
        );
        let calls = vec![
            NestedCall {
                id: "1".into(),
                name: "bash".into(),
                args: serde_json::json!({"cmd":"test"}),
                status: "done".into(),
                output: String::new(),
                error: None,
                depth: 0,
            },
            NestedCall {
                id: "2".into(),
                name: "bash".into(),
                args: serde_json::json!({"cmd":"test"}),
                status: "done".into(),
                output: String::new(),
                error: None,
                depth: 0,
            },
        ];
        let groups = group_calls(&calls, &tools, "script");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].description.as_deref(), Some("2 runs, 2 failed"));
    }
}
