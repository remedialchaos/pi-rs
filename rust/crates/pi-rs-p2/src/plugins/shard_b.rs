//! Shard B of Wave 2's built-in native p2 plugins: `locations`, `ports`, `models`, `envs`,
//! `workspace` and `terminals`, each under its own module.

#[path = "ports/mod.rs"]
pub mod ports;

use crate::api::v1::PluginRegistry;

/// Registers every shard B plugin into `registry`.
pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    ports::server::register(registry)?;
    Ok(())
}
