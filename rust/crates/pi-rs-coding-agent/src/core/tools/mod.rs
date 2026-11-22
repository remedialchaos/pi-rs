//! Tool implementations ported from `packages/coding-agent/src/core/tools/`.
//!
//! Each module is self-contained where possible: the file-system tools share
//! `path_utils` and `truncate`, and the edit tools share `edit_diff`. No module
//! confines paths to a sandbox; upstream deliberately accepts absolute paths
//! and `..` segments.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod find;
pub mod grep;
pub mod ls;
pub mod path_utils;
pub mod powershell;
pub mod read;
pub mod renderers;
pub mod tool_definition_wrapper;
pub mod truncate;
pub mod wrappers;
pub mod write;
