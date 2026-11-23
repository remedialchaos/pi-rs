//! `web-shell` built-in plugin: web app shell metadata & facet registration.

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    contracts::Contract,
    host::{Members, ServeOptions, define_facet},
};

use crate::api::v1::{FacetKind, PluginRegistry};

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let contract = Arc::new(Contract::local(
        "p2.web-shell",
        1,
        Vec::new(),
        indexmap::IndexMap::new(),
    )?);
    registry.register(
        "web-shell",
        FacetKind::Web,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                |_, _, _| Ok(Members::new()),
                ServeOptions::default(),
            )
        }),
    )
}
