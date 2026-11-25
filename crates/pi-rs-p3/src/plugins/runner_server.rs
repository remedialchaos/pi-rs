//! The runner plugin on the server: contributes the local runner to `WorkerRunners`, and stops every
//! worker when its facet ends (a reload of this plugin, the server's close).

use std::sync::Arc;

use pi_rs_chord::host::{PluginFacet, define_facet};

use crate::{api::v1::contribute::PluginLives, registry::ServerRegistries, runner::WorkerRunner};

pub fn facet(
    runner: Arc<dyn WorkerRunner>,
    registries: ServerRegistries,
    lives: &Arc<PluginLives>,
) -> PluginFacet {
    let lives = lives.clone();
    PluginFacet::new(
        "p3-runner",
        define_facet(move |_host| -> pi_rs_chord::Result<()> {
            let live = lives.open("p3-runner");
            live.contribute(&registries.worker_runners, runner.clone());
            Ok(())
        }),
    )
}
