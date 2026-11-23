//! Built-in native `p2` plugins (Wave 2).

pub mod artifacts;
pub mod mcp;
pub mod radius;
mod search;
pub mod shard_a;
pub mod shard_c;

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    shard_a::register(registry)?;
    shard_c::register(registry)
}
