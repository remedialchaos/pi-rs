//! Built-in native `p2` plugins (Wave 2).

use crate::api::v1::PluginRegistry;

pub mod shard_b;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_b::register(registry)?;
    Ok(())
}
