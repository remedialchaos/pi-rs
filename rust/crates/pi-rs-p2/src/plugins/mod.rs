//! Built-in native `p2` plugins (Wave 2).

pub mod agent_plugins;
pub mod artifacts;
pub mod cache_warming;
pub mod chat_mode;
pub mod codemode;
pub mod coding_tools;
pub mod dev_tools;
pub mod devices;
pub mod drafts;
pub mod file_index;
pub mod files;
pub mod fit_images;
pub mod logs;
pub mod mcp;
pub mod powershell;
pub mod projects;
pub mod prompt;
pub mod proxy_marker;
pub mod quiet_tools;
pub mod radius;
pub mod scope_prompt;
mod search;
pub mod send_file;
pub mod settings;
pub mod shard_a;
pub mod shard_b;
pub mod shard_c;
pub mod shard_c2;
pub mod shard_d;
pub mod shard_d3;
pub mod status;
pub mod subagents;
pub mod sync;
pub mod telemetry;
pub mod tree;
pub mod views;
pub mod voice;
pub mod web_shell;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_a::register(registry)?;
    shard_b::register(registry)?;
    shard_c::register(registry)?;
    shard_c2::register(registry)?;
    shard_d::register(registry)?;
    shard_d3::register(registry)?;
    Ok(())
}
