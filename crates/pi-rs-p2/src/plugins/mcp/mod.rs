//! The MCP plugin: the owner's MCP servers, the leases the session workers hold on them, and the
//! sign-ins the owner's browser walks through.

mod codemode;
pub mod config;
pub mod config_file;
pub mod credentials;
pub mod environments;
pub mod instances;
pub mod leases;
mod pages;
pub mod placement;
pub mod probe;
pub mod project;
mod routes;
mod server;
mod sign_in;
mod tools;
pub mod transports;
mod windows_command;
mod worker;

pub fn register(registry: &mut crate::api::v1::PluginRegistry) -> pi_rs_chord::Result<()> {
    registry.register("mcp", crate::api::v1::FacetKind::Server, server::facet()?)?;
    registry.register("mcp", crate::api::v1::FacetKind::Worker, worker::facet()?)
}

#[cfg(test)]
mod tests;
