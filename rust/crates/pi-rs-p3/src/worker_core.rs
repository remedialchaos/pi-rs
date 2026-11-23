//! The worker's base pieces that a test topology shares with the real worker: the base's facets (the
//! registries it serves).

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    host::{PluginFacet, define_facet},
};

use crate::{api::v1::contribute::PluginLives, registry::WorkerRegistries, worker::WorkerRuntime};

/// The base's facets on a worker: `WorkerRuntime` (the process values) and `WorkerBusy` (the sum of
/// the counts, composed by [`WorkerRegistries::busy`]).
pub fn facets(
    runtime: &Arc<WorkerRuntime>,
    registries: &WorkerRegistries,
    lives: &Arc<PluginLives>,
) -> Vec<PluginFacet> {
    let runtime = runtime.clone();
    let registries = registries.clone();
    let lives = lives.clone();
    vec![PluginFacet::new(
        "p3.worker",
        define_facet(move |_host| -> Result<()> {
            let live = lives.open("p3.worker");
            live.contribute(&registries.worker_runtime, runtime.clone());
            Ok(())
        }),
    )]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pi_rs_chord::{context::Context, host::create_host};

    use super::*;

    #[tokio::test]
    async fn the_base_serves_the_workers_process_values() {
        let runtime = WorkerRuntime::new("s1", PathBuf::from("/tmp/worker"), 100);
        let registries = WorkerRegistries::default();
        let lives = PluginLives::new();
        let host = create_host(Default::default()).expect("host");
        host.load(
            facets(&runtime, &registries, &lives),
            &Context::background(),
        )
        .await
        .expect("load");
        let served = registries.runtime().expect("runtime");
        assert!(Arc::ptr_eq(&served, &runtime));
        assert_eq!(served.session(), "s1");
        assert_eq!(served.idle_ms(), 100);
        host.close(&Context::background()).await.expect("close");
    }
}
