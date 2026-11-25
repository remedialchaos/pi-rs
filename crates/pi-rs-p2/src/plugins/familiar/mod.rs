//! Native `familiar` plugin: server and worker facets for the owner's Familiar session.

pub mod cold;
pub mod server;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

/// The Familiar's name for the owner: its session's title when made, its sidebar row's label.
pub const FAMILIAR_TITLE: &str = "Ask Pi";

/// `WorkerInit.values` key: whether the starting worker's session is the Familiar (a boolean).
pub const FAMILIAR_INIT: &str = "familiar";

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("familiar", FacetKind::Server, server::server_facet())?;
    registry.register("familiar", FacetKind::Worker, worker::worker_facet())?;
    Ok(())
}
