//! Built-in `voice` plugin: speech service proxy, room WebSocket bridge, and worker voice mode.

pub mod choice;
pub mod contract;
pub mod policy;
pub mod protocol;
pub mod server;
pub mod speech_text;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

pub const PLUGIN_NAME: &str = "voice";

/// Registers the `voice` server and worker facets in `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register(PLUGIN_NAME, FacetKind::Server, server::server_facet()?)?;
    registry.register(PLUGIN_NAME, FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
