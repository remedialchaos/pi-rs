use std::{path::PathBuf, sync::Arc};

use pi_rs_ai::{
    Message, StringOrBlocks,
    faux::{
        FauxModelDefinition, FauxOptions, FauxStep, faux_assistant_message, faux_provider,
        faux_text,
    },
    registry::Models,
    transcript::input_text,
};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{HostOptions, PluginFacet},
    node::{
        app::{FilesCaps, WorkerApp, WorkerAppOptions, start_worker_with},
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

// These paths keep the worker's native API together with its entry point.
#[path = "../runtime/durable.rs"]
pub mod durable;
#[path = "../runtime/worker_core.rs"]
mod worker_core;
pub use worker_core::*;

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
) -> Result<SessionWorker> {
    start(
        worker,
        registry,
        requested,
        None,
        |_| Ok(Vec::new()),
        |_, _| Ok(None),
    )
    .await
}

/// Existing embedders can supply a lifecycle boundary of their own.
pub async fn start_session_worker_with(
    worker: WorkerSession,
    registry: &PluginRegistry,
    requested: Option<&[String]>,
    boundary: impl FnOnce(&WorkerInit, &SessionInit) -> Result<Option<Arc<WorkerBoundary>>>,
) -> Result<SessionWorker> {
    start(
        worker,
        registry,
        requested,
        None,
        |_| Ok(Vec::new()),
        boundary,
    )
    .await
}

/// Supply registered models and construct native facets with access to the shared worker process.
pub async fn start_session_worker_configured(
    worker: WorkerSession,
    registry: &PluginRegistry,
    requested: Option<&[String]>,
    models: Models,
    facets: impl FnOnce(&Arc<WorkerProcess>) -> Result<Vec<PluginFacet>>,
) -> Result<SessionWorker> {
    start(worker, registry, requested, Some(models), facets, |_, _| {
        Ok(None)
    })
    .await
}

async fn start(
    worker: WorkerSession,
    registry: &PluginRegistry,
    requested: Option<&[String]>,
    models: Option<Models>,
    facets: impl FnOnce(&Arc<WorkerProcess>) -> Result<Vec<PluginFacet>>,
    boundary: impl FnOnce(&WorkerInit, &SessionInit) -> Result<Option<Arc<WorkerBoundary>>>,
) -> Result<SessionWorker> {
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
    let boundary = boundary(&init, &session)?;
    let models = match models {
        Some(models) => models,
        None if init.fake => fake_models(),
        None => Models::builtin(Default::default()).map_err(durable::durable_error)?,
    };
    let process = Arc::new(WorkerProcess::new(init, &session, models));
    let boundary = boundary.unwrap_or_else(|| {
        Arc::new(WorkerBoundary::new(
            process.location.value(),
            Arc::new(BoundaryBackend(Arc::downgrade(&process))),
        ))
    });
    let mut core = registry.facets(FacetKind::Worker, &selected);
    core.extend(facets(&process)?);
    core.push(PluginFacet::new("@p2/core", boundary.facet(id.clone())?));
    let opening = process.clone();
    let app = start_worker_with(
        worker,
        WorkerAppOptions {
            directory,
            principal: json!({"kind":"host"}),
            server_principal: json!({"kind":"server"}),
            core,
            files: FilesCaps::default(),
        },
        HostOptions {
            principal_policy: Some(crate::runtime::principals::Principal::POLICY),
            ..Default::default()
        },
        async move |store| {
            opening
                .open(store, &session, boundary, &Context::background())
                .await?;
            Ok(opening.liveness().idle())
        },
    )
    .await;
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            let _ = process.close().await;
            return Err(error);
        }
    };
    Ok(SessionWorker { app, process })
}

pub struct SessionWorker {
    app: WorkerApp,
    pub process: Arc<WorkerProcess>,
}

impl std::ops::Deref for SessionWorker {
    type Target = WorkerApp;

    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

impl SessionWorker {
    pub async fn run(mut self) -> Result<()> {
        let result = self
            .app
            .serve_with_idle(|| self.process.liveness().idle())
            .await;
        result.and(self.close().await)
    }

    pub async fn close(self) -> Result<()> {
        self.process.stop();
        let host = self.app.host.close(&Context::background()).await;
        let durable = self.process.close().await;
        let store = self.app.store.database().close().await;
        durable.and(host).and(store)
    }
}

fn fake_models() -> Models {
    let models = Models::default();
    for (provider, id) in [("faux", "faux-1"), ("fake", "fake")] {
        let faux = Arc::new(faux_provider(FauxOptions {
            api: Some("p2-faux".into()),
            provider: Some(provider.into()),
            models: vec![FauxModelDefinition {
                id: id.into(),
                ..Default::default()
            }],
            tokens_per_second: Some(200.0),
            ..Default::default()
        }));
        let mut provider = faux.provider.clone();
        if let Some(chat) = provider.chat.take() {
            let queue = std::sync::Mutex::new(());
            provider.chat = Some(Arc::new(move |model, context, options, simple| {
                let text = context
                    .messages()
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::User(message) => Some(match &message.content {
                            StringOrBlocks::String(text) => text.clone(),
                            StringOrBlocks::Blocks(content) => input_text(content, "\n"),
                        }),
                        _ => None,
                    })
                    .unwrap_or_default();
                let response = text
                    .strip_prefix("json ")
                    .map_or_else(|| format!("Echo: {text}"), str::to_owned);
                let _guard = queue.lock().unwrap_or_else(|err| err.into_inner());
                faux.append_responses([FauxStep::from(faux_assistant_message(vec![faux_text(
                    response,
                )]))]);
                chat(model, context, options, simple)
            }));
        }
        models.set_provider(provider);
    }
    models
}
