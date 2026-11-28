use std::{
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_ai::{
    Message, StringOrBlocks,
    faux::{
        FauxModelDefinition, FauxOptions, FauxStep, faux_assistant_message, faux_provider,
        faux_text, faux_tool_call,
    },
    registry::Models,
    transcript::input_text,
};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::{Host, HostOptions, PluginFacet, define_facet},
    json::JsonValue,
    node::{
        app::{FilesCaps, WorkerApp, WorkerAppOptions, start_worker_with_setup},
        workers::WorkerSession,
    },
    remote::RemotePeer,
    sync::SyncFuture,
    telemetry::{Recorder, SendKind, TelemetrySender, TelemetrySink},
    telemetry_ingest::TelemetryBatch,
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

const WORKER_PROCESS_SERVICE: &str = "p2.worker-process";

/// Retrieve the active `WorkerProcess` from a worker Chord `Host`.
pub fn worker_of(host: &Host) -> Result<Arc<WorkerProcess>> {
    crate::runtime::server::service_of::<WorkerProcess>(host, WORKER_PROCESS_SERVICE)
}

struct HeldWorkerServices {
    _parent: Arc<pi_rs_chord::remote::RemotePeer>,
    _files: Arc<pi_rs_chord::node::app::Files>,
    sender: Option<TelemetrySender>,
}

struct WorkerTelemetrySink {
    parent: Weak<RemotePeer>,
    contract: Arc<Contract>,
}

impl TelemetrySink for WorkerTelemetrySink {
    fn send(&self, batch: &TelemetryBatch) -> Option<SyncFuture<()>> {
        let parent = self.parent.upgrade()?;
        let contract = self.contract.clone();
        let Ok(payload) = serde_json::to_value(batch) else {
            return None;
        };
        Some(Box::pin(async move {
            let ctx = Context::background();
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                let held = parent.hold(contract, JsonValue::Object(IndexMap::new()), &ctx)?;
                held.wait(&ctx).await?;
                let _ = held
                    .call("ingest", JsonValue::from_serde(payload), &ctx)
                    .await;
                held.release();
                Ok::<_, Error>(())
            })
            .await;
            Ok(())
        }))
    }

    fn release(&self) {}
}

type HeldWorkerServicesSlot = Arc<std::sync::Mutex<Option<HeldWorkerServices>>>;

async fn start(
    mut worker: WorkerSession,
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
    let reserved = registry.select(None)?;
    worker.scripts.retain(|script| {
        if reserved.contains(&script.name) {
            eprintln!("p2 plugins: plugin {} is already discovered", script.name);
            false
        } else {
            true
        }
    });
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
    let mut core = Vec::new();
    let installed = process.clone();
    core.push(PluginFacet::new(
        "@p2/worker-process",
        define_facet(move |host| {
            crate::runtime::server::install_service(
                &host.host(),
                WORKER_PROCESS_SERVICE,
                &installed,
            )
        }),
    ));
    core.extend(registry.facets(FacetKind::Worker, &selected));
    core.extend(facets(&process)?);
    core.push(PluginFacet::new("@p2/core", boundary.facet(id.clone())?));
    let delivered: Vec<_> = worker
        .scripts
        .iter()
        .map(|s| (s.name.clone(), s.digest.clone()))
        .collect();
    let telemetry_enabled = process.init.telemetry;
    let session_id = id.clone();
    let opening = process.clone();
    let held_services: HeldWorkerServicesSlot = Arc::default();
    let setup_services = held_services.clone();
    let app = start_worker_with_setup(
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
        move |host, parent, files| {
            let parent_arc = Arc::new(parent.clone());
            let files_arc = Arc::new(files.clone());
            crate::runtime::server::install_service(host, "p2.server-peer", &parent_arc)?;
            crate::runtime::server::install_service(
                host,
                crate::plugins::send_file::WORKER_FILES_SERVICE,
                &files_arc,
            )?;
            let sender = if telemetry_enabled {
                let scope_session = session_id.clone();
                let recorder = Recorder::new(move || format!("worker:{scope_session}"), 10_000);
                let contract = Arc::new(crate::plugins::telemetry::worker_ingest_contract()?);
                let sink = Arc::new(WorkerTelemetrySink {
                    parent: Arc::downgrade(&parent_arc),
                    contract,
                });
                let sender = TelemetrySender::new(recorder, sink, SendKind::Worker);
                let _ = sender.start_timer(Duration::from_secs(5));
                Some(sender)
            } else {
                None
            };
            *setup_services
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(HeldWorkerServices {
                _parent: parent_arc,
                _files: files_arc,
                sender,
            });
            Ok(())
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
    for (name, digest) in delivered {
        if app.plugins.digest(&name).as_ref() != Some(&digest) {
            eprintln!("p2 worker: optional component {name} was not registered");
        }
    }
    Ok(SessionWorker {
        app,
        process,
        _services: held_services,
    })
}

pub struct SessionWorker {
    app: WorkerApp,
    pub process: Arc<WorkerProcess>,
    _services: HeldWorkerServicesSlot,
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
        let sender = self
            ._services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .and_then(|s| s.sender.take());
        if let Some(sender) = sender {
            let _ = sender.send(true).await;
            sender.stop();
        }
        let host = self.app.host.close(&Context::background()).await;
        let durable = self.process.close().await;
        let store = self.app.store.database().close().await;
        durable.and(host).and(store)
    }
}

pub(crate) fn fake_models() -> Models {
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
                let step = if let Some(raw_args) = text.strip_prefix("send_file ")
                    && let Ok(args) = serde_json::from_str::<pi_rs_ai::types::JsonObject>(raw_args)
                {
                    FauxStep::from(faux_assistant_message(vec![faux_tool_call(
                        "send_file",
                        args,
                        None,
                    )]))
                } else {
                    let response = text
                        .strip_prefix("json ")
                        .map_or_else(|| format!("Echo: {text}"), str::to_owned);
                    FauxStep::from(faux_assistant_message(vec![faux_text(response)]))
                };
                let _guard = queue.lock().unwrap_or_else(|err| err.into_inner());
                faux.append_responses([step]);
                chat(model, context, options, simple)
            }));
        }
        models.set_provider(provider);
    }
    models
}

#[cfg(test)]
mod tests {
    use super::fake_models;
    use pi_rs_ai::{
        AssistantContent, Context, Message, StreamOptions, StringOrBlocks,
        transcript::normalize_context,
        types::{UserMessage, UserRole},
    };

    #[tokio::test]
    async fn fake_models_emit_send_file_tool_call_when_prompt_requests_it()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let models = fake_models();
        let model = models
            .get_model("faux", "faux-1")
            .ok_or("missing faux-1 model")?;
        let provider = models.get_provider("faux").ok_or("missing faux provider")?;
        let chat = provider.chat.ok_or("missing chat handler")?;
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: StringOrBlocks::String(r#"send_file {"path":"shot.png"}"#.into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let stream = chat(
            model,
            normalize_context(ctx),
            StreamOptions::default(),
            false,
        );
        let msg = stream.result().await?;
        assert!(matches!(
            msg.content.as_slice(),
            [AssistantContent::ToolCall(call)] if call.name == "send_file"
                && call.arguments.get("path").and_then(|v| v.as_str()) == Some("shot.png")
        ));
        Ok(())
    }
}
