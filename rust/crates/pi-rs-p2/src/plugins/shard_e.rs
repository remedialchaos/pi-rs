//! Wave 2 Shard E built-in plugin registration.

use crate::api::v1::PluginRegistry;

pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    super::agent::register(registry)?;
    Ok(())
}
