//! Wave 2 Plugin Shard E2 registration (`discord`).

use pi_rs_chord::Result;

use crate::api::v1::PluginRegistry;

/// Registers the Shard E2 built-in plugin (`discord`) into `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    super::discord::register(registry)?;
    Ok(())
}
