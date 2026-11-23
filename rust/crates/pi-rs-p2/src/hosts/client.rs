//! Native web and terminal facets on a host connected to p2's Chord endpoint.

use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Host, HostOptions, PluginFacet, create_host},
    link::Link,
    remote::{RemoteOptions, RemotePeer},
};
use serde_json::json;
use std::sync::Arc;

use crate::api::v1::{FacetKind, PluginRegistry};

pub struct ClientHost {
    pub host: Host,
    pub server: RemotePeer,
}

impl ClientHost {
    pub async fn close(self, context: &Context) -> Result<()> {
        self.server.close();
        self.host.close(context).await
    }
}

/// Connect a caller-supplied link (for example, a browser WebSocket or the terminal launcher)
/// and activate the facets for that surface.
pub async fn start_client_host(
    link: Arc<dyn Link>,
    registry: &PluginRegistry,
    kind: FacetKind,
    requested: Option<&[String]>,
    mut core: Vec<PluginFacet>,
    context: &Context,
) -> Result<ClientHost> {
    if !matches!(kind, FacetKind::Web | FacetKind::Tui) {
        return Err(Error::Invalid("Expected a web or tui host kind".into()));
    }
    let selected = registry.select(requested)?;
    core.extend(registry.facets(kind, &selected));
    let host = create_host(HostOptions::default())?;
    let server = host.connect(link, RemoteOptions::new(json!({"kind":"server"})))?;
    let client = ClientHost { host, server };
    let result = client.host.load(core, context).await.and_then(|report| {
        if let Some(failure) = report.failed.first() {
            return Err(Error::Invalid(format!(
                "p2 plugin {}: {}",
                failure.plugin, failure.error
            )));
        }
        Ok(())
    });
    if let Err(error) = result {
        let _ = client.close(&Context::background()).await;
        return Err(error);
    }
    Ok(client)
}
