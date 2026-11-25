//! Native `history` plugin: worker facet providing `history_search`, `history_read`, and `history_handoff`.

pub mod tools;

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    host::{Facet, define_facet},
    json::JsonValue,
};
use pi_rs_durable::session::Error as DurableError;

use crate::api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup, worker::worker_of};

use self::tools::{HistoryHits, HistorySearch, LabelFn, create_history_extension};

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("history", FacetKind::Worker, worker_facet())
}

pub fn worker_facet() -> Facet {
    define_facet(|host| {
        let chord_host = host.host();
        let worker = worker_of(&chord_host)?;
        let search_feed = Arc::new(lookup("p2.search-feed")?);
        let session_id = worker.init.session()?.id;

        let search_host = chord_host.clone();
        let search_worker = worker.clone();
        let search: HistorySearch = Arc::new(move |query, context| {
            let host = search_host.clone();
            let worker = search_worker.clone();
            let contract = search_feed.clone();
            Box::pin(async move {
                let value = worker
                    .call(async {
                        let root = host.root();
                        let feed = root.hold(contract, JsonValue::Object(Default::default()))?;
                        feed.wait(&context).await?;
                        let args = JsonValue::from_serde(serde_json::to_value(&query)?);
                        feed.call("history", args, &context).await
                    })
                    .await
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                let hits: HistoryHits = serde_json::from_value(value.to_serde()?)
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                Ok(hits)
            })
        });

        let label: LabelFn = Arc::new(move |conversation| format!("{session_id}#{conversation}"));
        let extension = create_history_extension(search, label);
        worker.registries.extensions.install(extension)?;
        Ok(())
    })
}
