//! Wave 2 Plugin Shard D2 registration (`history`, `familiar`, `rollups`, `agent`, `backup`).

use pi_rs_chord::Result;

use crate::api::v1::PluginRegistry;

/// Registers the Shard D2 built-in plugins into `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    super::history::register(registry)?;
    super::familiar::register(registry)?;
    super::rollups::register(registry)?;
    Ok(())
}
