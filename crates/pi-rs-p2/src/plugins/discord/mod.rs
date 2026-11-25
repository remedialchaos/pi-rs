//! Built-in `discord` plugin: Discord bot fleet, channel/thread relay, and session ingress.

pub mod bot;
pub mod contract;
pub mod fleet;
pub mod relay;
pub mod server;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

pub const PLUGIN_NAME: &str = "discord";

/// Registers the `discord` server and worker facets in `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register(PLUGIN_NAME, FacetKind::Server, server::server_facet()?)?;
    registry.register(PLUGIN_NAME, FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
