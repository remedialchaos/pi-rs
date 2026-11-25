mod attachments;
mod conversation;
mod image_size;
mod images;
mod public;
mod shell;
#[cfg(test)]
mod tests;
mod view;

pub use public::{SendRouter, SendRouters};

use crate::api::v1::{FacetKind, PluginRegistry};
use pi_rs_chord::Result;

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("chat", FacetKind::Server, images::server()?)?;
    registry.register("chat", FacetKind::Worker, public::worker()?)
}
