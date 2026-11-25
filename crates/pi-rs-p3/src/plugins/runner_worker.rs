//! The runner plugin on a session's worker: the worker stops itself once it was idle for its idle
//! time (no work counted by `WorkerBusy`, no work on its host). Any work in between starts the wait
//! again. How long, and whether to stop at all, is the runner's choice; the base only closes
//! (`WorkerRuntime::close`).

use std::{sync::Arc, time::Duration};

use pi_rs_chord::{
    Result,
    host::{Facet, define_facet},
};

use crate::{
    api::v1::contribute::PluginLives, api::v1::control::CloseReason, registry::WorkerRegistries,
    worker::WorkerRuntime,
};

/// How often the idle rule looks at the worker's work: a quarter of its idle time, from 10 ms to a
/// quarter second.
fn tick(idle_ms: u64) -> Duration {
    Duration::from_millis((idle_ms / 4).clamp(10, 250))
}

pub fn facet(
    runtime: Arc<WorkerRuntime>,
    registries: WorkerRegistries,
    lives: &Arc<PluginLives>,
) -> Facet {
    let lives = lives.clone();
    define_facet(move |_host| -> Result<()> {
        let live = lives.open("p3-runner");
        let runtime = runtime.clone();
        let registries = registries.clone();
        live.spawn(async move {
            let idle = Duration::from_millis(runtime.idle_ms());
            let mut quiet_since = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tick(runtime.idle_ms())) => {
                        if registries.busy() > 0 {
                            quiet_since = std::time::Instant::now();
                        } else if quiet_since.elapsed() >= idle {
                            runtime.close(CloseReason::Idle);
                            return;
                        }
                    }
                    _ = runtime.wait_closed() => return,
                }
            }
        });
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pi_rs_chord::{context::Context, host::create_host};

    use super::*;

    async fn run(idle_ms: u64, busy: Option<(u64, u64)>) -> Arc<WorkerRuntime> {
        let runtime = WorkerRuntime::new("s1", PathBuf::from("/tmp/worker"), idle_ms);
        let registries = WorkerRegistries::default();
        let lives = PluginLives::new();
        let host = create_host(Default::default()).expect("host");
        host.load(
            vec![pi_rs_chord::host::PluginFacet::new(
                "p3-runner",
                facet(runtime.clone(), registries.clone(), &lives),
            )],
            &Context::background(),
        )
        .await
        .expect("load");
        if let Some((count, ms)) = busy {
            registries.worker_busy.set("p3-test", count);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            registries.worker_busy.remove("p3-test");
        }
        let closed = tokio::time::timeout(
            std::time::Duration::from_millis(idle_ms * 8 + 1000),
            runtime.wait_closed(),
        )
        .await
        .is_ok();
        assert!(closed, "the worker closed itself");
        let reason = runtime.wait_closed().await;
        assert_eq!(reason, CloseReason::Idle);
        lives.end_all();
        host.close(&Context::background()).await.expect("close");
        runtime
    }

    #[tokio::test]
    async fn a_worker_with_no_work_closes_itself_after_its_idle_time() {
        run(60, None).await;
    }

    #[tokio::test]
    async fn work_in_worker_busy_keeps_the_worker_alive_until_it_ends() {
        run(60, Some((1, 400))).await;
    }
}
