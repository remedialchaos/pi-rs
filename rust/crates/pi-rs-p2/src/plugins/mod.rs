//! Built-in native `p2` plugins (Wave 2).

pub mod devices;
pub mod drafts;
pub mod file_index;
pub mod logs;
pub mod proxy_marker;
pub mod shard_d;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_d::register(registry)?;
    Ok(())
}
