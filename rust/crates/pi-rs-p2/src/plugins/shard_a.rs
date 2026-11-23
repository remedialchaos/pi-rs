#[path = "conversations/mod.rs"]
mod conversations;
#[path = "sessions/mod.rs"]
mod sessions;
#[path = "sharing/mod.rs"]
mod sharing;

pub use sharing::PeopleUrl;

use crate::api::v1::PluginRegistry;

pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    sharing::register(registry)?;
    sessions::register(registry)?;
    conversations::register(registry)
}
