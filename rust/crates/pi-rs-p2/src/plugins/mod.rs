//! Built-in native `p2` plugins (Wave 2).

use crate::api::v1::PluginRegistry;

/// Registers all built-in native `p2` plugins into `registry`.
pub fn register_builtins(_registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    Ok(())
}
