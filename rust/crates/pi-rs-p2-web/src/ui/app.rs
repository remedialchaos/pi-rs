//! App shell: workbench layout, boundary and error toast (upstream `AppLayout.tsx`,
//! `app-boundary.tsx`, `ErrorToast.tsx`).

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
) -> impl IntoView {
    let shown = shown.unwrap_or(MainPane::Chat);
    let drawer_open: RwSignal<bool> = RwSignal::new(false);
    let toggle_drawer = move |_| drawer_open.update(|open| *open = !*open);
    view! {
        <div
            class="workbench"
            data-pane=move || pane_signal.map_or(shown, |s| s.get()).pane_name()
            data-drawer-open=move || drawer_open.get().to_string()
        >
            <button type="button" class="drawer-toggle" aria-label="Toggle navigation" on:click=toggle_drawer>
                "☰"
            </button>
            <nav class="sidebar" aria-label="Sessions">
                {sidebar}
            </nav>
            <button
                type="button"
                class="drawer-scrim"
                aria-label="Close navigation"
                on:click=move |_| drawer_open.set(false)
            ></button>
            <main class="main" data-testid="main">
                {main}
            </main>
            {panel.map(|content| view! { <aside class="side-panel" aria-label="Details">{content}</aside> })}
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
) -> impl IntoView {
    view! {
        <AppBoundary>
            <Workbench sidebar=sidebar main=main panel=panel pane_signal=pane_signal.unwrap_or_else(|| RwSignal::new(MainPane::Chat)) />
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
