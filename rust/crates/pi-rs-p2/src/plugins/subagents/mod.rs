//! Built-in `subagents` plugin: persistent background subagents and team messaging.

pub mod children;
pub mod contract;
pub mod contract_worker;
pub mod model_list;
pub mod server;
pub mod tools;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

pub const PLUGIN_NAME: &str = "subagents";

/// Registers the `subagents` server and worker facets in `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register(PLUGIN_NAME, FacetKind::Server, server::server_facet()?)?;
    registry.register(PLUGIN_NAME, FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
