//! Built-in `artifacts` plugin: session html pages and Markdown documents with version history,
//! sandboxed `/artifact-frame` serving, live `p2.artifact-page` eval/diagnostics, and the agent's
//! 12 `artifact_*` code-mode tools.

pub mod bridge;
pub mod frame;
pub mod pages;
pub mod server;
pub mod store;
pub mod tool;
pub mod worker;

#[cfg(test)]
mod tests;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

/// Registers the `artifacts` plugin (server and worker facets) into `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("artifacts", FacetKind::Server, server::server_facet()?)?;
    registry.register("artifacts", FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
