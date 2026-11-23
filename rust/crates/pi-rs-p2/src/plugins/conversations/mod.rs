mod server;
#[cfg(test)]
mod tests;
mod tool;
mod worker;

use crate::api::v1::{FacetKind, PluginRegistry};
use pi_rs_chord::Result;

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("conversations", FacetKind::Server, server::facet()?)?;
    registry.register("conversations", FacetKind::Worker, worker::facet()?)
}
