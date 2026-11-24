//! Session-scoped terminal windows shared by the owner's panel and the terminal tool.

mod server;
mod service;
mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

/// Optional server service containing an `Arc<dyn EnvironmentProvider>` for named environments.
pub const ENVIRONMENT_PROVIDER_SERVICE: &str = "p2.terminal-env-provider";

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("terminals", FacetKind::Server, server::facet()?)?;
    registry.register("terminals", FacetKind::Worker, worker::facet()?)
}
