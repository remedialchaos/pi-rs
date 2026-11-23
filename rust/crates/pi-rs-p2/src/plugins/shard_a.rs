#[path = "sharing/mod.rs"]
mod sharing;

pub use sharing::PeopleUrl;

use crate::api::v1::PluginRegistry;

pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    sharing::register(registry)
}
