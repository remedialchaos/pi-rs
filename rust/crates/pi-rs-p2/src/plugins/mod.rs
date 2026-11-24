//! Built-in native `p2` plugins (Wave 2).

pub mod familiar;
pub mod history;
pub mod rollups;
pub mod shard_d2;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_d2::register(registry)?;
    Ok(())
}
