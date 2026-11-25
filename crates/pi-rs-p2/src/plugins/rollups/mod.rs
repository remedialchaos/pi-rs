//! Native `rollups` plugin: code-defined sidebar rollups (server + Familiar worker tool).

pub mod runner;
pub mod server;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

/// The Familiar's tool for rollups, named in its tool lists.
pub const ROLLUPS_TOOL: &str = "sidebar_rollups";

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("rollups", FacetKind::Server, server::server_facet())?;
    registry.register("rollups", FacetKind::Worker, worker::worker_facet())?;
    Ok(())
}
