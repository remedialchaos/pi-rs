//! Leptos CSR WASM browser frontend for `p2` (Milestone 3.2-web, ADR-011).
//!
//! Port of `apps/p2/src/web/`. Pure-Rust state/protocol logic lives in `state`,
//! `command_menu`, `activity`, `gestures`, `cost`, `chord_bridge` and `markdown`,
//! all testable on the host and compiled to `wasm32-unknown-unknown`. The Leptos
//! `csr` components live in `ui`; the WASM entry point is in `wasm` and gated on
//! `target_arch = "wasm32"` so native builds and tests stay clean.
#![forbid(unsafe_code)]

pub mod activity;
pub mod assets;
pub mod browser_plugins;
pub mod chord_bridge;
pub mod command_menu;
pub mod cost;
pub mod gestures;
pub mod markdown;
pub mod state;
pub mod ui;
pub mod wasm;
