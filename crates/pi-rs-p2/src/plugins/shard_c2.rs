//! Wave 2 Plugin Shard C2 registration (`subagents`, `voice`, `discord`).

use pi_rs_chord::Result;

use crate::api::v1::PluginRegistry;

/// Registers the Shard C2 built-in plugins into `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    super::subagents::register(registry)?;
    super::voice::register(registry)?;
    Ok(())
}
