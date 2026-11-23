//! The browser entry point. Compiled only for `wasm32-unknown-unknown` so native builds
//! and tests stay clean (ADR-011). `#[wasm_bindgen(start)]` runs `mount_app` on load.

#[cfg(target_arch = "wasm32")]
mod imp {
    use leptos::mount::mount_to_body;
    use leptos::prelude::*;
    use wasm_bindgen::prelude::*;

    use crate::ui::app::AppLayout;
    use crate::ui::chat::{Chat, ChatMessage, append_stream_token};
    use crate::ui::composer::Composer;

    /// Mounts the p2 web app into `<body>`.
    pub fn mount_app() {
        mount_to_body(|| {
            let messages: RwSignal<Vec<ChatMessage>> = RwSignal::new(vec![]);
            let draft: RwSignal<String> = RwSignal::new(String::new());
            let sidebar = view! {
                <div class="sidebar-content" data-testid="sidebar">
                    <span class="sidebar-title">"Sessions"</span>
                </div>
            }
            .into_any();
            let main = view! {
                <div class="main-content">
                    <Chat messages=messages />
                    <Composer
                        draft=draft
                        on_send=Callback::new(move |text: String| {
                            messages.update(|items| {
                                append_stream_token(items, "local", &text);
                            });
                        })
                    />
                </div>
            }
            .into_any();
            view! { <AppLayout sidebar=sidebar main=main panel=None /> }
        });
    }

    /// wasm-bindgen entry: called once when the WASM module loads.
    #[wasm_bindgen(start)]
    pub fn start() {
        mount_app();
    }
}

#[cfg(target_arch = "wasm32")]
pub use imp::mount_app;
