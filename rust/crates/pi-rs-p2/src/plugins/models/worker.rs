//! Worker facet for `models`: serves `p2.worker-models` and updates worker settings.

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde_json::json;

use crate::{
    api::v1::{p2_contracts::lookup, worker::worker_of},
    plugins::models::resources::ModelResources,
};

pub fn worker_facet() -> Result<Facet> {
    let worker_models = Arc::new(lookup("p2.worker-models")?);

    Ok(define_facet(move |facet_host| {
        let host = facet_host.host();
        let worker = worker_of(&host)?;

        facet_host.serve(
            worker_models.clone(),
            move |_, _, object| {
                let worker = worker.clone();
                let settings_holder = object.writable(JsonValue::from_serde(json!({})))?;
                let settings_open = settings_holder.clone();

                object.open(
                    move |ctx| {
                        let worker = worker.clone();
                        let settings_open = settings_open.clone();
                        Box::pin(async move {
                            let res = ModelResources::open(
                                worker.init.data_dir.clone(),
                                worker.init.fake,
                            )
                            .await?;
                            let initial = res.settings();
                            let _ = worker.registries.settings.send(initial.clone());
                            settings_open.set(JsonValue::from_serde(initial), &ctx.context)?;

                            let mut rx = res.subscribe();
                            let res_watch = res.clone();
                            let settings_watch = settings_open.clone();
                            let task = tokio::spawn(async move {
                                let bg = pi_rs_chord::context::Context::background();
                                while rx.changed().await.is_ok() {
                                    let next = res_watch.settings();
                                    let _ = worker.registries.settings.send(next.clone());
                                    let _ = settings_watch.set(JsonValue::from_serde(next), &bg);
                                }
                            });
                            Ok((task.abort_handle(), res))
                        })
                    },
                    |state, _| {
                        Box::pin(async move {
                            state.0.abort();
                            Ok(())
                        })
                    },
                );

                let mut members = Members::new();
                members.state("settings", settings_holder)?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}
