//! Real mounted dynamic WASM/surface plugin views (replacing the static revision placeholder).

use crate::browser_plugins::{PluginDescriptor, safe_plugin_url};
use leptos::prelude::*;

/// Renders a live dynamic plugin view:
/// - `surface_url`: sandboxed `<iframe>` surface with `postMessage` bridge metadata
/// - `wasm_url`: live mounted WASM host container that instantiates the plugin module
/// - `schema`/`contract`: declarative reactive plugin surface
#[component]
pub fn DynamicPluginView(
    descriptor: PluginDescriptor,
    session_id: Option<String>,
    on_close: Callback<()>,
) -> impl IntoView {
    let title = descriptor.title.clone();
    let id = descriptor.id.clone();
    let rev = descriptor.revision.clone();
    let safe_surface = descriptor.surface_url.as_deref().and_then(safe_plugin_url);
    let safe_wasm = descriptor.wasm_url.as_deref().and_then(safe_plugin_url);
    let schema = descriptor.schema.clone();
    let contract = descriptor.contract.clone();

    let host_ref = NodeRef::<leptos::html::Div>::new();
    #[cfg(target_arch = "wasm32")]
    if let Some(wasm_src) = safe_wasm.clone() {
        let plugin_id = id.clone();
        let plugin_rev = rev.clone();
        Effect::new(move |_| {
            if let Some(el) = host_ref.get() {
                let src = wasm_src.clone();
                let pid = plugin_id.clone();
                let prev = plugin_rev.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    mount_wasm_plugin(&el, &pid, &prev, &src).await;
                });
            }
        });
    }

    view! {
        <div class="sheet" role="dialog" aria-label=title.clone() data-testid=format!("panel-{id}")>
            <button type="button" class="sheet-head" data-testid="side-panel-close" on:click=move |_| on_close.run(())>
                {format!("{title} ×")}
            </button>
            {if let Some(src) = safe_surface {
                let full_src = if let Some(sid) = session_id.as_deref() {
                    let sep = if src.contains('?') { '&' } else { '?' };
                    format!("{src}{sep}session={sid}&rev={rev}")
                } else {
                    src
                };
                view! {
                    <div class="dynamic-plugin-surface" data-plugin-id=id.clone() data-plugin-rev=rev.clone()>
                        <iframe
                            class="dynamic-plugin-frame"
                            data-testid=format!("dynamic-surface-{id}")
                            sandbox="allow-scripts allow-forms"
                            src=full_src
                        ></iframe>
                    </div>
                }.into_any()
            } else if let Some(wasm_src) = safe_wasm {
                view! {
                    <div
                        node_ref=host_ref
                        class="dynamic-plugin-wasm-host"
                        data-testid=format!("dynamic-wasm-{id}")
                        data-plugin-id=id.clone()
                        data-plugin-rev=rev.clone()
                        data-wasm-url=wasm_src
                    >
                        <p class="muted small">{format!("Mounting WASM plugin {id}@{rev}...")}</p>
                    </div>
                }.into_any()
            } else {
                let _ = &host_ref;
                view! {
                    <div class="dynamic-plugin-schema" data-testid=format!("dynamic-schema-{id}") data-plugin-rev=rev.clone()>
                        {contract.map(|c| view! { <p class="small muted">{format!("Contract: {c} · rev {rev}")}</p> })}
                        {schema.map(|s| view! { <pre class="plugin-schema-preview">{s.to_string()}</pre> })}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

#[cfg(target_arch = "wasm32")]
async fn mount_wasm_plugin(
    host: &web_sys::HtmlDivElement,
    plugin_id: &str,
    revision: &str,
    wasm_url: &str,
) {
    use wasm_bindgen::{JsCast, JsValue};
    let _ = host.set_attribute("data-mount-state", "loading");
    let Ok(resp) = gloo_net::http::Request::get(wasm_url).send().await else {
        let _ = host.set_attribute("data-mount-state", "error");
        host.set_inner_text(&format!("Failed to load WASM plugin {plugin_id}"));
        return;
    };
    let Ok(bytes) = resp.binary().await else {
        let _ = host.set_attribute("data-mount-state", "error");
        host.set_inner_text(&format!("Failed to read WASM bytes for {plugin_id}"));
        return;
    };
    let imports = js_sys::Object::new();
    let promise = js_sys::WebAssembly::instantiate_buffer(&bytes, &imports);
    match wasm_bindgen_futures::JsFuture::from(promise).await {
        Ok(result) => {
            let _ = host.set_attribute("data-mount-state", "mounted");
            if let Ok(instance) = js_sys::Reflect::get(&result, &"instance".into())
                && let Ok(exports) = js_sys::Reflect::get(&instance, &"exports".into())
                && let Ok(mount_fn) = js_sys::Reflect::get(&exports, &"pi_mount".into())
                && let Some(f) = mount_fn.dyn_ref::<js_sys::Function>()
            {
                let _ = f.call2(
                    &JsValue::NULL,
                    &JsValue::from_str(plugin_id),
                    &JsValue::from_str(revision),
                );
            }
            host.set_inner_text(&format!("Mounted {plugin_id} ({revision})"));
        }
        Err(_) => {
            let _ = host.set_attribute("data-mount-state", "error");
            host.set_inner_text(&format!("Invalid WASM module for {plugin_id}"));
        }
    }
}
