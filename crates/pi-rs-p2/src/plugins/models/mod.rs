//! `models` plugin (`p2.models`, `p2.model-config`, `p2.worker-models`, `p2.model-resources`).

pub mod resources;
pub mod scope;
pub mod server;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

pub use resources::{MODEL_RESOURCES_SERVICE, ModelResources};

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("models", FacetKind::Server, server::server_facet()?)?;
    registry.register("models", FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
