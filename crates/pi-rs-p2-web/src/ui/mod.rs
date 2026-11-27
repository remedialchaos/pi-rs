//! Leptos `0.8.x` CSR UI components (upstream `apps/p2/src/web/`).
//!
//! Components use fine-grained signals (`RwSignal`, `Memo`) so streaming transcripts and
//! Chord deltas update leaf DOM nodes without a virtual DOM diff. The `mount_app` entry
//! point lives in [`crate::wasm`] and is compiled only for `wasm32`.

pub mod activity_ui;
pub mod app;
pub mod chat;
pub mod command_menu;
pub mod composer;
pub mod cost_ui;
pub mod dynamic_plugin;
pub mod notices;
pub mod panels;
pub mod plugin_panels;
pub mod session_stats;
pub mod sessions_sidebar;
pub mod terminal;
pub mod views;
pub mod workspace;
