//! Activity UI: wheels, sheet, thought slot and results (upstream `activity/`).

use crate::activity::detail::describe;
use crate::activity::label::{Described, Label, LabelStyle, plain};
use crate::activity::model::{
    Step, StepStatus, ToolActivityTable, WheelEntry, elapsed, step_state,
};
use crate::activity::results::Result as ToolResult;
use crate::activity::thought_slot::wheel_thought;
use leptos::prelude::*;

/// A verb/object label with its kind's icon (upstream `Label.tsx`).
#[component]
pub fn LabelText(label: Label) -> impl IntoView {
    let kind = label.kind.icon();
    let text = plain(&label);
    let verb = label.verb.clone().unwrap_or_default();
    let object = label.object.clone().unwrap_or_default();
    let style = label.style.map(LabelStyle::as_str).unwrap_or("");
    let extra = label.extra.clone().unwrap_or_default();
    view! {
        <span class="label-text" data-kind=kind aria-label=text>
            <span class="activity-icon" data-icon=kind></span>
            {(!verb.is_empty()).then(|| view! { <span class="label-verb">{verb.clone()}</span> })}
            {(!object.is_empty()).then(|| view! {
                <span class="label-object" data-style=style>{object.clone()}</span>
            })}
            {(!extra.is_empty()).then(|| view! {
                <span class="label-extra">" · "{extra.clone()}</span>
            })}
        </span>
    }
}

/// One tool call row in a wheel (upstream `ActivityRow.tsx`).
#[component]
pub fn ActivityRow(step: Step, described: Described, tools: ToolActivityTable) -> impl IntoView {
    let state = step_state(&step, &tools);
    let running = step.status == StepStatus::Running;
    let key = step.id.clone();
    let description = described.description.clone();
    view! {
        <div class="activity-row" data-status=state_label(step.status) data-step-id=key>
            <LabelText label=described.label.clone() />
            {description.map(|text| view! { <span class="activity-description">{text}</span> })}
            <span class="activity-state" data-running=running.to_string()>{state}</span>
        </div>
    }
}

fn state_label(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Running => "running",
        StepStatus::Done => "done",
        StepStatus::Error => "error",
    }
}

/// A running wheel's thought slot below it (upstream `ThoughtSlot.tsx`).
#[component]
pub fn ThoughtSlot(entries: Vec<WheelEntry>, working: bool) -> impl IntoView {
    let thought = wheel_thought(&entries, working);
    let visible = thought.is_some().to_string();
    let content = thought.map(|words| {
        let live = words.live.to_string();
        view! { <span class="thought-text" data-live=live>{words.text}</span> }
    });
    view! {
        <div class="thought-slot" data-visible=visible>
            {content}
        </div>
    }
}

/// The expandable right-hand sheet for a run's steps (upstream `ActivitySheet.tsx`).
#[component]
pub fn ActivitySheet(
    steps: Vec<Step>,
    tools: ToolActivityTable,
    #[prop(optional)] started_at: Option<u64>,
) -> impl IntoView {
    let duration = started_at.map(elapsed);
    let rows: Vec<(Step, Described, ToolActivityTable)> = steps
        .into_iter()
        .map(|step| {
            let described = describe(&step, &tools);
            (step, described, tools.clone())
        })
        .collect();
    view! {
        <div class="activity-sheet" data-testid="activity-sheet">
            {duration.map(|text| view! { <span class="activity-duration">{text}</span> })}
            <For
                each=move || rows.clone()
                key=|entry| entry.0.id.clone()
                children={move |entry: (Step, Described, ToolActivityTable)| {
                    view! { <ActivityRow step=entry.0 described=entry.1 tools=entry.2 /> }
                }}
            />
        </div>
    }
}

/// The results a turn made for the person (upstream `Results.tsx`).
#[component]
pub fn Results(results: Vec<ToolResult>) -> impl IntoView {
    view! {
        <div class="results" data-testid="results">
            <For
                each=move || results.clone()
                key=|result| result.tool_id.clone()
                children={move |result: ToolResult| {
                    view! { <div class="result" data-tool=result.tool_name data-inline=result.inline.to_string()></div> }
                }}
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_labels() {
        assert_eq!(state_label(StepStatus::Running), "running");
        assert_eq!(state_label(StepStatus::Done), "done");
        assert_eq!(state_label(StepStatus::Error), "error");
    }
}
