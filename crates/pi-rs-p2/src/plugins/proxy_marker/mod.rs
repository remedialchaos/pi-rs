//! `proxy-marker` built-in plugin: exposes the `p2.proxy-marker` local contract without
//! contributing any authenticators (marker classification lives in host origin checks).

use std::sync::Arc;

use crate::api::v1::{
    FacetKind, Members, PluginRegistry, ServeOptions, define_facet, p2_contracts::lookup,
};

pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    let contract = Arc::new(lookup("p2.proxy-marker")?);
    registry.register(
        "proxy-marker",
        FacetKind::Server,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                |_, _, _| Ok(Members::new()),
                ServeOptions::default(),
            )
        }),
    )
}
