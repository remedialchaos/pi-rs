//! Built-in native `p2` plugins (Wave 2).

pub mod agent_plugins;
pub mod cache_warming;
pub mod codemode;
pub mod coding_tools;
pub mod dev_tools;
pub mod devices;
pub mod drafts;
pub mod file_index;
pub mod files;
pub mod fit_images;
pub mod logs;
pub mod powershell;
pub mod projects;
pub mod proxy_marker;
pub mod quiet_tools;
pub mod scope_prompt;
pub mod send_file;
pub mod settings;
pub mod shard_a;
pub mod shard_c2;
pub mod shard_d;
pub mod status;
pub mod subagents;
pub mod sync;
pub mod telemetry;
pub mod voice;
pub mod web_shell;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_a::register(registry)?;
    shard_c2::register(registry)?;
    shard_d::register(registry)?;
    Ok(())
}
