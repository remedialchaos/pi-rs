//! Pure-Rust state and protocol logic shared by native tests and the WASM UI.
//! Ported from `apps/p2/src/web/`; no DOM dependency in this layer.

pub mod anchor;
pub mod artifact_kinds;
pub mod chat_context;
pub mod composer_state;
pub mod conversation_cache;
pub mod file_completion;
pub mod highlight_lines;
