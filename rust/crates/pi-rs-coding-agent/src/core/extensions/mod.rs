//! Extension host: event dispatch, registration, loading, contexts, and WASM.
//!
//! This module is the Rust counterpart of
//! `packages/coding-agent/src/core/extensions/`. Native built-ins and WASM
//! guests implement the same `Extension` trait and go through one
//! `ExtensionRunner`, so dispatch order, replaceability, conflict handling,
//! and `/reload` semantics are identical.

pub mod context;
pub mod loader;
pub mod runner;
pub mod types;
pub mod wasm;

pub use runner::ExtensionRunner;
pub use types::{
    Diagnostic, Extension, ExtensionCommandContext, ExtensionContext, ExtensionError,
    ExtensionHost, ExtensionInitContext, ExtensionMode, ExtensionRegistration,
    ExtensionToolContext, ExtensionUi, FlagValue, LoadExtensionsResult, ResourceDiagnostic,
    ToolOutcome,
};
