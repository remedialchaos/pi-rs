//! Built-in Wave 2 plugins assigned to Shard B2.

use pi_rs_chord::Result;

use crate::api::v1::PluginRegistry;

/// Registers all Shard B2 built-in native `p2` plugins into `registry`.
pub fn register_builtins(registry: &mut PluginRegistry) -> Result<()> {
    super::locations::register(registry)?;
    super::models::register(registry)?;
    Ok(())
}
