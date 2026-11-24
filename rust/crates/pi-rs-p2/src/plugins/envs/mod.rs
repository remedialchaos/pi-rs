//! `envs` plugin (`p2.env-service`, `p2.envs`, `p2.sandbox-secrets`, `p2.env-managers`).

pub mod config;
pub mod manager;
pub mod secrets;
pub mod server;
pub mod store;
pub mod worker;

use pi_rs_chord::Result;

use crate::api::v1::{FacetKind, PluginRegistry};

pub use crate::plugins::locations::{ENV_MANAGERS_SERVICE, EnvManagers};

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("envs", FacetKind::Server, server::server_facet()?)?;
    registry.register("envs", FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
