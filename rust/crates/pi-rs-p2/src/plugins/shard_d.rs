//! Wave 2 Plugin Shard D registration.

use crate::api::v1::PluginRegistry;

use super::{devices, drafts, file_index, logs, proxy_marker};

pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    proxy_marker::register(registry)?;
    drafts::register(registry)?;
    logs::register(registry)?;
    file_index::register(registry)?;
    devices::register(registry)?;
    Ok(())
}
