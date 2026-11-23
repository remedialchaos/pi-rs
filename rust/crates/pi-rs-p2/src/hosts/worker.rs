use std::{path::PathBuf, sync::Arc};

use pi_rs_chord::{
    Error, Result,
    host::PluginFacet,
    node::{
        app::{FilesCaps, WorkerApp, WorkerAppOptions, start_worker},
        workers::WorkerSession,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{FacetKind, PluginRegistry},
    runtime::{
        host_dirs::session_directory,
        session_worker::{SessionInit, WorkerBoundary},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerInit {
    pub app: PathBuf,
    pub data_dir: PathBuf,
    pub fake: bool,
    pub telemetry: bool,
    pub generation: u64,
    pub values: Map<String, Value>,
}

impl WorkerInit {
    pub fn session(&self) -> Result<SessionInit> {
        let session = self.values.get("session").ok_or_else(|| {
            Error::Invalid("Session parameters unavailable; refusing worker start".into())
        })?;
        Ok(serde_json::from_value(session.clone())?)
    }
}

pub use crate::runtime::address::session_address;

pub async fn start_session_worker(
    worker: WorkerSession,
    registry: &PluginRegistry,
    requested: Option<&[String]>,
) -> Result<WorkerApp> {
    start_session_worker_with(worker, registry, requested, |_, _| Ok(None)).await
}

/// Assemble the native session runtime after validating the parent handshake.
/// The factory can share its boundary with the plugin facets that admit work.
pub async fn start_session_worker_with(
    worker: WorkerSession,
    registry: &PluginRegistry,
    requested: Option<&[String]>,
    boundary: impl FnOnce(&WorkerInit, &SessionInit) -> Result<Option<Arc<WorkerBoundary>>>,
) -> Result<WorkerApp> {
    let init: WorkerInit = serde_json::from_value(worker.start.clone())?;
    let session = init.session()?;
    let id = &session.id;
    if worker.address != session_address(id) {
        return Err(Error::Invalid(
            "Session parameters do not match worker address".into(),
        ));
    }
    let selected = registry.select(requested)?;
    let directory = session_directory(&init.data_dir, id)?;
    let mut core = registry.facets(FacetKind::Worker, &selected);
    if let Some(boundary) = boundary(&init, &session)? {
        core.push(PluginFacet::new("@p2/core", boundary.facet(id.clone())?));
    }
    start_worker(
        worker,
        WorkerAppOptions {
            directory,
            principal: json!({"kind":"host"}),
            server_principal: json!({"kind":"server"}),
            core,
            files: FilesCaps::default(),
        },
    )
    .await
}
