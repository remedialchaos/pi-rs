//! App shell: workbench layout, boundary and error toast (upstream `AppLayout.tsx`,
//! `app-boundary.tsx`, `ErrorToast.tsx`).

use crate::gestures::drag_size::{clamp_size, size_style};
#[cfg(target_arch = "wasm32")]
use crate::gestures::drawer_swipe;
use leptos::prelude::*;

/// An error to show in the toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastError {
    pub message: String,
}

/// The app's error boundary: keeps an error signal and renders a toast for it.
/// Upstream's `AppBoundary` catches render errors and `ErrorToast` shows them.
#[component]
pub fn AppBoundary(children: Children) -> impl IntoView {
    let error: RwSignal<Option<ToastError>> = RwSignal::new(None);
    let clear = Callback::new(move |_| error.set(None));
    view! {
        <div class="app-boundary">
            {children()}
            <ErrorToast error=error on_dismiss=clear />
        </div>
    }
}

/// A transient error toast, driven by a signal (upstream `ErrorToast.tsx`).
#[component]
pub fn ErrorToast(error: RwSignal<Option<ToastError>>, on_dismiss: Callback<()>) -> impl IntoView {
    view! {
        <div class="error-toast" role="status" aria-live="polite" data-visible=move || error.get().is_some().to_string()>
            {move || error.get().map(|failure| view! {
                <span class="error-toast-message">{failure.message}</span>
                <button type="button" class="error-toast-dismiss" aria-label="Dismiss" on:click=move |_| on_dismiss.run(())>
                    "×"
                </button>
            })}
        </div>
    }
}

/// Which pane is shown in the workbench's main area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainPane {
    Chat,
    Terminal,
    Workspace,
}

/// The three-pane workbench: sidebar, main area and an optional right-hand panel.
/// Pane widths are CSS variables set by the drag handles (`--sidebar-width`, `--panel-width`).
#[component]
pub fn Workbench(
    /// The sidebar's content.
    sidebar: AnyView,
    /// The main area's content.
    main: AnyView,
    /// The right-hand panel, when open.
    panel: Option<AnyView>,
    /// Which pane the main area shows.
    #[prop(optional)]
    shown: Option<MainPane>,
    /// Reactive main pane signal when driven dynamically.
    #[prop(optional)]
    pane_signal: Option<RwSignal<MainPane>>,
    /// Reactive sidebar collapse signal.
    #[prop(optional)]
    sidebar_collapsed: Option<RwSignal<bool>>,
    /// Reactive sidebar width signal in px.
    #[prop(optional)]
    sidebar_width: Option<RwSignal<u32>>,
    /// Reactive right panel width signal in px.
    #[prop(optional)]
    panel_width: Option<RwSignal<u32>>,
    /// Callback invoked when user drags sidebar or panel resize handle.
    #[prop(optional)]
    on_resize_sidebar: Option<Callback<u32>>,
    #[prop(optional)] on_resize_panel: Option<Callback<u32>>,
) -> impl IntoView {
    let shown = shown.unwrap_or(MainPane::Chat);
    let drawer_open: RwSignal<bool> = RwSignal::new(false);
    let swipe_start_x: RwSignal<Option<f64>> = RwSignal::new(None);
    let dragging_sidebar = RwSignal::new(false);
    let dragging_panel = RwSignal::new(false);
    let toggle_drawer = move |_| drawer_open.update(|open| *open = !*open);
    let collapsed = sidebar_collapsed.unwrap_or_else(|| RwSignal::new(false));
    let sb_w = sidebar_width.unwrap_or_else(|| RwSignal::new(260));
    let pnl_w = panel_width.unwrap_or_else(|| RwSignal::new(380));

    view! {
        <div
            class="workbench"
            data-pane=move || pane_signal.map_or(shown, |s| s.get()).pane_name()
            data-drawer-open=move || drawer_open.get().to_string()
            data-sidebar-collapsed=move || collapsed.get().to_string()
            style=move || format!(
                "{}; {}",
                size_style("--sidebar-width", sb_w.get()),
                size_style("--panel-width", pnl_w.get())
            )
            on:touchstart=move |ev| {
                #[cfg(target_arch = "wasm32")]
                if let Ok(touches) = js_sys::Reflect::get(&ev, &"touches".into())
                    && let Ok(first) = js_sys::Reflect::get_u32(&touches, 0)
                    && let Ok(cx) = js_sys::Reflect::get(&first, &"clientX".into())
                    && let Some(x) = cx.as_f64()
                {
                    swipe_start_x.set(Some(x));
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (&ev, swipe_start_x);
            }
            on:touchend=move |ev| {
                #[cfg(target_arch = "wasm32")]
                if let Some(start_x) = swipe_start_x.get_untracked()
                    && let Ok(touches) = js_sys::Reflect::get(&ev, &"changedTouches".into())
                    && let Ok(first) = js_sys::Reflect::get_u32(&touches, 0)
                    && let Ok(cx) = js_sys::Reflect::get(&first, &"clientX".into())
                    && let Some(x) = cx.as_f64()
                {
                    let dx = x - start_x;
                    let open = drawer_open.get_untracked();
                    if drawer_swipe::may_take(dx > 0.0, open) {
                        let p = drawer_swipe::progress(open, dx, 260.0, 1.0);
                        drawer_open.set(drawer_swipe::settles_open(0.0, 1.0, p));
                    }
                    swipe_start_x.set(None);
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (&ev, swipe_start_x);
            }
            on:pointermove=move |ev| {
                if dragging_sidebar.get_untracked() {
                    let w = clamp_size(f64::from(ev.client_x()), 180.0, 480.0);
                    sb_w.set(w);
                    if let Some(cb) = on_resize_sidebar {
                        cb.run(w);
                    }
                } else if dragging_panel.get_untracked() {
                    #[cfg(target_arch = "wasm32")]
                    let win_w = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1200.0);
                    #[cfg(not(target_arch = "wasm32"))]
                    let win_w = 1200.0f64;
                    let w = clamp_size(win_w - f64::from(ev.client_x()), 260.0, 760.0);
                    pnl_w.set(w);
                    if let Some(cb) = on_resize_panel {
                        cb.run(w);
                    }
                }
            }
            on:pointerup=move |_| {
                dragging_sidebar.set(false);
                dragging_panel.set(false);
            }
        >
            <button type="button" class="drawer-toggle" aria-label="Toggle navigation" on:click=toggle_drawer>
                "☰"
            </button>
            <nav class="sidebar" aria-label="Sessions">
                {sidebar}
            </nav>
            <div
                class="resize-handle resize-sidebar"
                role="separator"
                aria-label="Resize sidebar"
                on:pointerdown=move |_| dragging_sidebar.set(true)
            ></div>
            <button
                type="button"
                class="drawer-scrim"
                aria-label="Close navigation"
                on:click=move |_| drawer_open.set(false)
            ></button>
            <main class="main" data-testid="main">
                {main}
            </main>
            {panel.map(|content| view! {
                <div
                    class="resize-handle resize-panel"
                    role="separator"
                    aria-label="Resize panel"
                    on:pointerdown=move |_| dragging_panel.set(true)
                ></div>
                <aside class="side-panel" aria-label="Details">{content}</aside>
            })}
        </div>
    }
}

impl MainPane {
    fn pane_name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Terminal => "terminal",
            Self::Workspace => "workspace",
        }
    }
}

/// The application's root layout.
#[component]
pub fn AppLayout(
    sidebar: AnyView,
    main: AnyView,
    panel: Option<AnyView>,
    #[prop(optional)] pane_signal: Option<RwSignal<MainPane>>,
    #[prop(optional)] sidebar_collapsed: Option<RwSignal<bool>>,
    #[prop(optional)] sidebar_width: Option<RwSignal<u32>>,
    #[prop(optional)] panel_width: Option<RwSignal<u32>>,
    #[prop(optional)] on_resize_sidebar: Option<Callback<u32>>,
    #[prop(optional)] on_resize_panel: Option<Callback<u32>>,
) -> impl IntoView {
    let sb_c = sidebar_collapsed.unwrap_or_else(|| RwSignal::new(false));
    let sb_w = sidebar_width.unwrap_or_else(|| RwSignal::new(260));
    let pnl_w = panel_width.unwrap_or_else(|| RwSignal::new(380));
    let on_sb = on_resize_sidebar.unwrap_or_else(|| Callback::new(|_| {}));
    let on_pnl = on_resize_panel.unwrap_or_else(|| Callback::new(|_| {}));
    view! {
        <AppBoundary>
            <Workbench
                sidebar=sidebar
                main=main
                panel=panel
                pane_signal=pane_signal.unwrap_or_else(|| RwSignal::new(MainPane::Chat))
                sidebar_collapsed=sb_c
                sidebar_width=sb_w
                panel_width=pnl_w
                on_resize_sidebar=on_sb
                on_resize_panel=on_pnl
            />
        </AppBoundary>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_names_match_css() {
        assert_eq!(MainPane::Chat.pane_name(), "chat");
        assert_eq!(MainPane::Terminal.pane_name(), "terminal");
        assert_eq!(MainPane::Workspace.pane_name(), "workspace");
    }
}
