//! The Radius plugin: the brain that connects this server to a Radius space, and the relay access
//! that tells its loopback requests apart from the device's own.

mod brain;
pub mod login;
pub mod relay;
mod server;
mod tunnel;

pub use brain::{RadiusBrain, Status, SyncAccess};
pub use server::{
    BACKUP_TARGET, CONNECTIONS, FORWARD_RESOLVER, RadiusBackup, RadiusBackupTarget,
    RadiusConnection,
};
pub use tunnel::{Forward, ResolveForward};

pub fn register(registry: &mut crate::api::v1::PluginRegistry) -> pi_rs_chord::Result<()> {
    registry.register(
        "radius",
        crate::api::v1::FacetKind::Server,
        server::facet()?,
    )
}

#[cfg(test)]
mod tests;
