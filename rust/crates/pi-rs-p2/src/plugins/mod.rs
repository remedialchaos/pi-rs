//! Built-in native `p2` plugins (Wave 2).

pub mod chat_mode;
pub mod shard_d3;
pub mod views;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_d3::register(registry)?;
    Ok(())
}
