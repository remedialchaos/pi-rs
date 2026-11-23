//! Composer UI and file completion popover (upstream `file-completion.tsx`, `Composer.tsx`).

use crate::state::file_completion::{Popover, PopoverKey};
use leptos::prelude::*;

/// The file completion popover above the input (upstream `FileCompletionPopover`).
#[component]
pub fn FileCompletionPopover(popover: Popover, on_choose: Callback<usize>) -> impl IntoView {
    let items = popover.items.clone();
    let selected = popover.selected;
    let hint = popover.hint.clone();
    let options: Vec<(usize, crate::state::file_completion::CompletionItem, bool)> = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| (index, item, index == selected))
        .collect();
    view! {
        <div class="file-completion" role="listbox" aria-label="File completion">
            <For
                each=move || options.clone()
                key=|entry| entry.0
                children={move |entry: (usize, crate::state::file_completion::CompletionItem, bool)| {
                    let index = entry.0;
                    let item = entry.1;
                    let is_selected = entry.2;
                    view! {
                        <div
                            class="file-completion-option"
                            role="option"
                            tabindex="-1"
                            aria-selected=is_selected.to_string()
                            class:selected=is_selected
                            on:mousedown=move |_| on_choose.run(index)
                        >
                            <span class="file-completion-label">{item.label.clone()}</span>
                            {item.description.clone().map(|description| view! {
                                <span class="file-completion-path">{description}</span>
                            })}
                        </div>
                    }
                }}
            />
            {hint.map(|text| view! { <div class="file-completion-hint">{text}</div> })}
        </div>
    }
}

/// The message composer: a textarea with reactive draft state and a send button.
#[component]
pub fn Composer(
    draft: RwSignal<String>,
    on_send: Callback<String>,
    #[prop(default = true)] clear_on_send: bool,
) -> impl IntoView {
    let send = {
        move |_| {
            let text = draft.get();
            if !text.trim().is_empty() {
                on_send.run(text);
                if clear_on_send {
                    draft.set(String::new());
                }
            }
        }
    };
    view! {
        <div class="composer" data-testid="composer">
            <textarea
                class="composer-input"
                aria-label="Message"
                rows="1"
                placeholder="Message Pi"
                prop:value=move || draft.get()
                on:input=move |event| draft.set(event_target_value(&event))
                on:keydown=move |event| {
                    if event.key() == "Enter" && !event.shift_key() {
                        event.prevent_default();
                        let text = draft.get();
                        if !text.trim().is_empty() {
                            on_send.run(text);
                            if clear_on_send {draft.set(String::new());}
                        }
                    }
                }
            ></textarea>
            <button type="button" class="composer-send" aria-label="Send" on:click=send>"↑"</button>
        </div>
    }
}

/// The pure key-to-popover-action mapping used by the textarea's key handler.
pub fn popover_key(key: &str, shift: bool) -> Option<PopoverKey> {
    match key {
        "ArrowDown" => Some(PopoverKey::ArrowDown),
        "ArrowUp" => Some(PopoverKey::ArrowUp),
        "Escape" => Some(PopoverKey::Escape),
        "Enter" if !shift => Some(PopoverKey::Enter),
        "Tab" if !shift => Some(PopoverKey::Tab),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping_respects_shift() {
        assert_eq!(popover_key("ArrowDown", false), Some(PopoverKey::ArrowDown));
        assert_eq!(popover_key("Enter", false), Some(PopoverKey::Enter));
        assert_eq!(popover_key("Enter", true), None);
        assert_eq!(popover_key("Tab", true), None);
        assert_eq!(popover_key("a", false), None);
    }
}
