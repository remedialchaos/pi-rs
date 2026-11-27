//! Composer UI and file completion popover (upstream `file-completion.tsx`, `Composer.tsx`).

use crate::state::file_completion::{Popover, PopoverKey};
use leptos::prelude::*;

/// A file selected via `<input type="file">`, drag-and-drop, or clipboard paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedUploadFile {
    pub name: String,
    pub size: u64,
    pub mime_type: Option<String>,
    pub bytes: Vec<u8>,
}

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

#[cfg(target_arch = "wasm32")]
fn read_js_file_list(
    file_list: wasm_bindgen::JsValue,
    on_attach: Callback<Vec<SelectedUploadFile>>,
) {
    use wasm_bindgen::JsCast;
    let Ok(len_val) = js_sys::Reflect::get(&file_list, &"length".into()) else {
        return;
    };
    let len = len_val.as_f64().unwrap_or(0.0) as u32;
    if len == 0 {
        return;
    }
    let mut files = Vec::with_capacity(len as usize);
    for idx in 0..len {
        if let Ok(item) = js_sys::Reflect::get_u32(&file_list, idx)
            && !item.is_undefined()
            && !item.is_null()
        {
            files.push(item);
        }
    }
    if files.is_empty() {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        let mut selected = Vec::with_capacity(files.len());
        for file in files {
            let name = js_sys::Reflect::get(&file, &"name".into())
                .ok()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "attachment".to_owned());
            let size = js_sys::Reflect::get(&file, &"size".into())
                .ok()
                .and_then(|v| v.as_f64())
                .map(|n| n.max(0.0) as u64)
                .unwrap_or(0);
            let mime_type = js_sys::Reflect::get(&file, &"type".into())
                .ok()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let Ok(ab_fn) = js_sys::Reflect::get(&file, &"arrayBuffer".into()) else {
                continue;
            };
            let Ok(func) = ab_fn.dyn_into::<js_sys::Function>() else {
                continue;
            };
            let Ok(promise_val) = func.call0(&file) else {
                continue;
            };
            let Ok(promise) = promise_val.dyn_into::<js_sys::Promise>() else {
                continue;
            };
            let Ok(buf) = wasm_bindgen_futures::JsFuture::from(promise).await else {
                continue;
            };
            let bytes = js_sys::Uint8Array::new(&buf).to_vec();
            selected.push(SelectedUploadFile {
                name,
                size: bytes.len().max(size as usize) as u64,
                mime_type,
                bytes,
            });
        }
        if !selected.is_empty() {
            on_attach.run(selected);
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn handle_file_input_change(
    event: web_sys::Event,
    on_attach: Option<Callback<Vec<SelectedUploadFile>>>,
) {
    let Some(cb) = on_attach else {
        return;
    };
    let Some(target) = event.target() else {
        return;
    };
    if let Ok(files) = js_sys::Reflect::get(&target, &"files".into())
        && !files.is_null()
        && !files.is_undefined()
    {
        read_js_file_list(files, cb);
    }
    let _ = js_sys::Reflect::set(&target, &"value".into(), &"".into());
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_file_input_change(
    _event: leptos::ev::Event,
    _on_attach: Option<Callback<Vec<SelectedUploadFile>>>,
) {
}

#[cfg(target_arch = "wasm32")]
fn handle_drop_files(
    event: leptos::ev::DragEvent,
    on_attach: Option<Callback<Vec<SelectedUploadFile>>>,
) {
    event.prevent_default();
    let Some(cb) = on_attach else {
        return;
    };
    if let Ok(dt) = js_sys::Reflect::get(&event, &"dataTransfer".into())
        && let Ok(files) = js_sys::Reflect::get(&dt, &"files".into())
        && !files.is_null()
        && !files.is_undefined()
    {
        read_js_file_list(files, cb);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_drop_files(
    _event: leptos::ev::DragEvent,
    _on_attach: Option<Callback<Vec<SelectedUploadFile>>>,
) {
}

#[cfg(target_arch = "wasm32")]
fn handle_paste_files(
    event: leptos::ev::ClipboardEvent,
    on_attach: Option<Callback<Vec<SelectedUploadFile>>>,
) {
    let Some(cb) = on_attach else {
        return;
    };
    if let Ok(cd) = js_sys::Reflect::get(&event, &"clipboardData".into())
        && let Ok(files) = js_sys::Reflect::get(&cd, &"files".into())
        && !files.is_null()
        && !files.is_undefined()
    {
        let len = js_sys::Reflect::get(&files, &"length".into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if len > 0.0 {
            event.prevent_default();
            read_js_file_list(files, cb);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_paste_files(
    _event: leptos::ev::ClipboardEvent,
    _on_attach: Option<Callback<Vec<SelectedUploadFile>>>,
) {
}

/// The message composer: a textarea with reactive draft state, file attachment controls,
/// file completion popover, and send/stop buttons.
#[component]
pub fn Composer(
    draft: RwSignal<String>,
    on_send: Callback<String>,
    #[prop(default = true)] clear_on_send: bool,
    #[prop(optional)] on_attach_files: Option<Callback<Vec<SelectedUploadFile>>>,
    #[prop(optional)] can_send_empty: Option<Signal<bool>>,
    #[prop(optional)] streaming: Option<Signal<bool>>,
    #[prop(optional)] on_stop: Option<Callback<()>>,
    #[prop(optional)] completion: Option<RwSignal<Option<Popover>>>,
    #[prop(optional)] on_choose_completion: Option<Callback<usize>>,
) -> impl IntoView {
    let file_input = NodeRef::<leptos::html::Input>::new();
    let dragging = RwSignal::new(false);
    let can_submit = move |text: &str| {
        !text.trim().is_empty() || can_send_empty.is_some_and(|sig| sig.get_untracked())
    };
    let send = {
        move |_| {
            let text = draft.get();
            if can_submit(&text) {
                on_send.run(text);
                if clear_on_send {
                    draft.set(String::new());
                }
            }
        }
    };
    view! {
        <div
            class="composer"
            data-testid="composer"
            on:dragover=move |ev| {
                ev.prevent_default();
                dragging.set(true);
            }
            on:dragleave=move |_| dragging.set(false)
            on:drop=move |ev| {
                dragging.set(false);
                handle_drop_files(ev, on_attach_files);
            }
        >
            {move || {
                let pop = completion.and_then(|c| c.get());
                pop.map(|p| {
                    let choose = on_choose_completion.unwrap_or_else(|| Callback::new(move |_| {}));
                    view! { <FileCompletionPopover popover=p on_choose=choose /> }
                })
            }}
            {move || dragging.get().then(|| view! {
                <div class="drop-overlay" data-testid="composer-drop-overlay">"Drop files to attach"</div>
            })}
            <input
                node_ref=file_input
                type="file"
                multiple
                class="composer-file-input"
                data-testid="composer-file-input"
                style="display: none"
                on:change=move |ev| handle_file_input_change(ev, on_attach_files)
            />
            {on_attach_files.map(|_| view! {
                <button
                    type="button"
                    class="composer-attach"
                    aria-label="Attach file"
                    data-testid="composer-attach-btn"
                    on:click=move |_| {
                        if let Some(el) = file_input.get() {
                            el.click();
                        }
                    }
                >"+"</button>
            })}
            <textarea
                class="composer-input"
                aria-label="Message"
                rows="1"
                placeholder="Message Pi"
                prop:value=move || draft.get()
                on:input=move |event| draft.set(event_target_value(&event))
                on:paste=move |event| handle_paste_files(event, on_attach_files)
                on:keydown=move |event| {
                    if let (Some(comp_sig), Some(mapped)) =
                        (completion, popover_key(&event.key(), event.shift_key()))
                    {
                        let has_popover = comp_sig.with_untracked(Option::is_some);
                        if has_popover {
                            event.prevent_default();
                            match mapped {
                                PopoverKey::ArrowDown | PopoverKey::ArrowUp => {
                                    comp_sig.update(|slot| {
                                        if let Some(p) = slot.as_mut() {
                                            let _ = p.on_key(mapped, event.shift_key());
                                        }
                                    });
                                }
                                PopoverKey::Escape => comp_sig.set(None),
                                PopoverKey::Enter | PopoverKey::Tab => {
                                    let idx = comp_sig.with_untracked(|slot| slot.as_ref().map(|p| p.selected));
                                    if let (Some(idx), Some(choose)) = (idx, on_choose_completion) {
                                        choose.run(idx);
                                    }
                                }
                            }
                            return;
                        }
                    }
                    if event.key() == "Enter" && !event.shift_key() {
                        event.prevent_default();
                        let text = draft.get();
                        if can_submit(&text) {
                            on_send.run(text);
                            if clear_on_send {draft.set(String::new());}
                        }
                    }
                }
            ></textarea>
            {move || {
                if streaming.is_some_and(|s| s.get()) {
                    view! {
                        <button
                            type="button"
                            class="composer-stop"
                            aria-label="Stop"
                            data-testid="composer-stop-btn"
                            on:click=move |_| {
                                if let Some(cb) = on_stop {
                                    cb.run(());
                                }
                            }
                        >"■"</button>
                    }.into_any()
                } else {
                    view! {
                        <button type="button" class="composer-send" aria-label="Send" on:click=send>"↑"</button>
                    }.into_any()
                }
            }}
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
