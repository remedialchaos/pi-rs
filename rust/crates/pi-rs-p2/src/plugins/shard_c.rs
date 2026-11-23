//! The built-in `p2` plugins of this shard.

use crate::{
    api::v1::PluginRegistry,
    plugins::{artifacts, mcp, radius, search},
};
use pi_rs_chord::Result;

/// Registers the plugins of this shard into `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    artifacts::register(registry)?;
    search::register(registry)?;
    mcp::register(registry)?;
    radius::register(registry)
}
