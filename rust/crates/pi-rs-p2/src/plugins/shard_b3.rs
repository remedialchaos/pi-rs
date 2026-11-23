//! Workspace and terminal plugin registration.

use pi_rs_chord::Result;

use crate::api::v1::PluginRegistry;

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    super::workspace::register(registry)
}
