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

/// Preview kind for downloadable/sent files rendered in [`FileCard`].
pub fn file_preview_kind(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" => Some("image"),
        "mp4" | "webm" | "mov" | "m4v" => Some("video"),
        _ => None,
    }
}

/// Sent or artifact file card: shows a fixed-height media thumbnail when previewable,
/// with the file row opening the viewer and the bordered button downloading the file.
#[component]
pub fn FileCard(
    title: String,
    meta: String,
    href: String,
    download: String,
    #[prop(optional)] thumbnail: Option<String>,
    on_open: Callback<()>,
) -> impl IntoView {
    let media = thumbnail
        .as_deref()
        .and_then(|_| file_preview_kind(&download));
    let thumb_url = thumbnail.unwrap_or_default();
    let open_thumb = on_open;
    let open_main = on_open;
    let title_label = format!("Open {title}");
    let download_label = format!("Download {title}");
    view! {
        <div class="file-card" data-testid="file-card">
            {media.map(|kind| {
                let src = if kind == "video" {
                    format!("{thumb_url}#t=0.1")
                } else {
                    thumb_url.clone()
                };
                view! {
                    <button
                        type="button"
                        class="file-card-thumbnail"
                        tabindex="-1"
                        aria-hidden="true"
                        on:click=move |_| open_thumb.run(())
                    >
                        {if kind == "image" {
                            view! { <img src=src.clone() alt="" decoding="async" loading="lazy" /> }.into_any()
                        } else {
                            view! { <video src=src.clone() muted=true playsinline=true preload="metadata" /> }.into_any()
                        }}
                    </button>
                }
            })}
            <div class="file-card-row">
                <button
                    type="button"
                    class="file-card-main"
                    aria-label=title_label
                    on:click=move |_| open_main.run(())
                >
                    <span class="file-card-text">
                        <strong>{title}</strong>
                        <small>{meta}</small>
                    </span>
                </button>
                <a class="file-card-download" href=href download=download aria-label=download_label>
                    "Download"
                </a>
            </div>
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

    #[test]
    fn file_preview_and_webkit_media_css_contracts() {
        assert_eq!(file_preview_kind("photo.PNG"), Some("image"));
        assert_eq!(file_preview_kind("clip.mp4"), Some("video"));
        assert_eq!(file_preview_kind("notes.txt"), None);
        let css = crate::assets::STYLE_CSS;
        assert!(css.contains(".file-card-thumbnail"));
        assert!(css.contains("grid-template: minmax(0, 1fr) / minmax(0, 1fr);"));
        assert!(css.contains(".artifact-md:not(.inline)"));
    }
}
