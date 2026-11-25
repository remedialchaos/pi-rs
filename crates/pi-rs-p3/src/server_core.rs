//! The server's base pieces that a test topology shares with the real server: the base's facet (the
//! registries it serves) and the admission of the route to session workers.

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    host::{Facet, PluginFacet, define_facet},
};

use crate::{api::v1::contribute::PluginLives, registry::ServerRegistries, server::ServerRuntime};

/// The base's facet: it serves `ServerRuntime` (the process values), which plugins read.
pub fn facet(
    runtime: Arc<ServerRuntime>,
    registries: ServerRegistries,
    lives: &Arc<PluginLives>,
) -> PluginFacet {
    let lives = lives.clone();
    PluginFacet::new("p3.server", define_server_facet(runtime, registries, lives))
}

fn define_server_facet(
    runtime: Arc<ServerRuntime>,
    registries: ServerRegistries,
    lives: Arc<PluginLives>,
) -> Facet {
    define_facet(move |_host| -> Result<()> {
        let live = lives.open("p3.server");
        live.contribute(&registries.server_runtime, runtime.clone());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use pi_rs_chord::context::Context;

    use super::*;
    use crate::install::WorkerSpec;

    #[tokio::test]
    async fn the_base_serves_the_servers_process_values() {
        let root = tempfile::tempdir().expect("temp dir");
        let options = crate::server::ServerOptions {
            data_dir: root.path().to_path_buf(),
            port: 0,
            worker: WorkerSpec::at("/bin/true"),
            worker_idle_ms: 100,
            version: "test".into(),
            on_stop: Arc::new(|| {}),
            on_error: Arc::new(|_| {}),
        };
        let server = crate::server::start_server(options, &Context::background())
            .await
            .expect("server");
        let registries = server.runtime.registries().clone();
        let served = registries.runtime().expect("runtime");
        let real = std::fs::canonicalize(root.path()).expect("real path");
        assert_eq!(served.data_dir(), real);
        assert!(server.runtime.listening().is_some());
        assert!(
            registries.runner().is_some(),
            "the runner plugin contributed"
        );
        server.close(&Context::background()).await.expect("close");
        assert!(crate::records::running(&real).is_none());
    }
}
