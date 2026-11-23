//! Built-in native `p2` plugins (Wave 2).

pub mod locations;
pub mod shard_b2;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_b2::register_builtins(registry)?;
    Ok(())
}
