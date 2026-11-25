//! `cache-warming` built-in plugin: keeps prompt cache entries warm per conversation
//! using `pi-rs-ai::cache_warming` policy and Durable `GenerationTask` hooks.

pub mod warmer;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use indexmap::IndexMap;
use pi_rs_ai::{AssistantMessage, CacheWarmingPhase, Message, StreamOptions, Usage};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::{Members, ServeOptions, define_facet},
    sync::SyncFuture,
};
use pi_rs_durable::{
    ConversationId, DocumentAddress, DocumentScope,
    harness::{
        Extension, HookHandlers, LIVE_DOC, USAGE_DOC, UsageState, add_usage as durable_add_usage,
        define_extension, generation_task, hook,
    },
    session::Error as DurableError,
};
use serde_json::Value;

use crate::{
    api::v1::{FacetKind, PluginRegistry, worker::worker_of},
    hosts::worker::{Activity, ActivityGuard, WorkerProcess},
    runtime::main_conversation::is_top_level,
};

use warmer::{CacheWarmingMode, ConversationWarmers, WarmRequest, WarmerHost};

/// Whether `current` starts with `sent`, compared by role and timestamp.
pub fn messages_start_with(current: &[Message], sent: &[Message]) -> bool {
    fn role_and_timestamp(msg: &Message) -> (&'static str, u64) {
        match msg {
            Message::System(m) => ("system", m.timestamp),
            Message::User(m) => ("user", m.timestamp),
            Message::Assistant(m) => ("assistant", m.timestamp),
            Message::ToolResult(m) => ("toolResult", m.timestamp),
        }
    }
    current.len() >= sent.len()
        && sent
            .iter()
            .enumerate()
            .all(|(idx, msg)| role_and_timestamp(&current[idx]) == role_and_timestamp(msg))
}

struct WorkerWarmerHost {
    worker: Arc<WorkerProcess>,
    guards: Mutex<Vec<ActivityGuard>>,
}

impl WorkerWarmerHost {
    fn new(worker: Arc<WorkerProcess>) -> Self {
        Self {
            worker,
            guards: Mutex::default(),
        }
    }
}

impl WarmerHost for WorkerWarmerHost {
    fn mode(&self) -> CacheWarmingMode {
        let settings = self.worker.registries.settings.borrow();
        CacheWarmingMode::from_setting(settings.get("cacheWarming").and_then(Value::as_str))
    }

    fn phase(
        &self,
        conversation_id: ConversationId,
        request: WarmRequest,
    ) -> SyncFuture<Option<CacheWarmingPhase>> {
        let worker = self.worker.clone();
        Box::pin(async move {
            let parts = worker.harness()?;
            let durable = &parts.durable;
            let ctx = Context::background();
            let live_address = DocumentAddress {
                kind: LIVE_DOC.kind.clone(),
                scope: DocumentScope::Conversation { conversation_id },
                key: None,
            };
            let live = durable
                .snapshot(&LIVE_DOC, &live_address, &ctx)
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if live
                .as_ref()
                .and_then(|v| v.get("generation"))
                .is_some_and(|v| !v.is_null())
            {
                return Ok(None);
            }
            let running = live
                .as_ref()
                .and_then(|v| v.get("run"))
                .is_some_and(|v| !v.is_null());
            if !running
                && !is_top_level(&durable.session, conversation_id, &ctx)
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?
            {
                return Ok(None);
            }
            let Some(conversation) = durable
                .conversation(conversation_id, &ctx)
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?
            else {
                return Ok(None);
            };
            let agent = conversation
                .agent(&ctx)
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let model_provider = agent
                .state
                .get("model")
                .and_then(|m| m.get("provider"))
                .and_then(Value::as_str);
            let model_id = agent
                .state
                .get("model")
                .and_then(|m| m.get("modelId"))
                .and_then(Value::as_str);
            if model_provider != Some(request.model.provider.as_str())
                || model_id != Some(request.model.id.as_str())
            {
                return Ok(None);
            }
            let raw_current = conversation
                .context(None, &ctx)
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?
                .messages;
            let current: Vec<Message> = raw_current
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect();
            if !messages_start_with(&current, &request.messages) {
                return Ok(None);
            }
            Ok(Some(if running {
                CacheWarmingPhase::Streaming
            } else {
                CacheWarmingPhase::Idle
            }))
        })
    }

    fn record(&self, conversation_id: ConversationId, message: AssistantMessage) -> SyncFuture<()> {
        let worker = self.worker.clone();
        Box::pin(async move {
            let parts = worker.harness()?;
            let ctx = Context::background();
            let key = format!("{}/{}", message.provider, message.model);
            let usage: Usage = message.usage;
            parts
                .durable
                .commit(
                    async move |tx| {
                        let address = DocumentAddress {
                            kind: USAGE_DOC.kind.clone(),
                            scope: DocumentScope::Conversation { conversation_id },
                            key: None,
                        };
                        let doc = tx.doc(&USAGE_DOC, &address, None).await?;
                        let mut state: UsageState = serde_json::from_value(
                            doc.value()?
                                .to_json()
                                .to_serde()
                                .map_err(|e| DurableError::invalid(e.to_string()))?,
                        )?;
                        let entry = state.models.entry(key.clone()).or_default();
                        durable_add_usage(entry, &usage);
                        doc.set(&[], serde_json::to_value(state)?.into())?;
                        Ok(())
                    },
                    &ctx,
                )
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
            Ok(())
        })
    }

    fn keep_alive(&self, delta: isize) {
        let mut guards = self.guards.lock().unwrap_or_else(|p| p.into_inner());
        if delta > 0 {
            for _ in 0..delta {
                guards.push(self.worker.hold_activity(Activity::CacheWarming));
            }
        } else if delta < 0 {
            for _ in 0..(-delta) {
                let _ = guards.pop();
            }
        }
    }
}

/// Builds the Durable `cache-warming` extension bound to `warmers`.
pub fn create_cache_warming_extension(warmers: ConversationWarmers) -> Extension {
    let last_messages: Arc<Mutex<HashMap<ConversationId, Vec<Message>>>> = Arc::default();
    let mut handlers = HookHandlers::new();

    let capture_msgs = last_messages.clone();
    handlers.insert(
        "beforeRequest".into(),
        Arc::new(move |payload, invocation, _| {
            let capture_msgs = capture_msgs.clone();
            let conversation_id = invocation.record.conversation_id;
            Box::pin(async move {
                if let Some(arr) = payload.get("messages").and_then(Value::as_array)
                    && let Ok(msgs) = arr
                        .iter()
                        .cloned()
                        .map(serde_json::from_value::<Message>)
                        .collect::<std::result::Result<Vec<_>, _>>()
                {
                    capture_msgs
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(conversation_id, msgs);
                }
                Ok(None)
            })
        }),
    );

    let warm_msgs = last_messages;
    handlers.insert(
        "afterResponse".into(),
        Arc::new(move |payload, invocation, _| {
            let warmers = warmers.clone();
            let warm_msgs = warm_msgs.clone();
            let conversation_id = invocation.record.conversation_id;
            let models = invocation.harness().models();
            Box::pin(async move {
                let message: AssistantMessage = serde_json::from_value(payload)?;
                let Some(messages) = warm_msgs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&conversation_id)
                else {
                    return Ok(None);
                };
                let Some(model) = models.get_model(&message.provider, &message.model) else {
                    return Ok(None);
                };
                warmers.start(
                    conversation_id,
                    &message,
                    WarmRequest {
                        model,
                        messages,
                        options: StreamOptions::default(),
                    },
                    models,
                );
                Ok(None)
            })
        }),
    );

    define_extension(Extension {
        name: "cache-warming".into(),
        hooks: vec![hook(&generation_task(), handlers)],
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let runtime_contract = Arc::new(Contract::local(
        "p2.cache-warming-runtime",
        1,
        Vec::new(),
        IndexMap::new(),
    )?);
    registry.register(
        "cache-warming",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                runtime_contract.clone(),
                move |_, _, object| {
                    if let Ok(worker) = worker_of(&object.host()) {
                        let warmers = ConversationWarmers::new(Arc::new(WorkerWarmerHost::new(
                            worker.clone(),
                        )));
                        let extension = create_cache_warming_extension(warmers.clone());
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let initial_enabled = registries
                            .settings
                            .borrow()
                            .get("cacheWarming")
                            .and_then(Value::as_str)
                            != Some("off");
                        let _ = registries
                            .extensions
                            .switch(&ext_name, Some(initial_enabled));

                        let settings_rx = registries.settings.subscribe();
                        let switch_regs = registries.clone();
                        let switch_name = ext_name.clone();
                        object.on_live(move |ctx| {
                            let switch_regs = switch_regs.clone();
                            let switch_name = switch_name.clone();
                            let mut rx = settings_rx.clone();
                            Box::pin(async move {
                                while matches!(
                                    ctx.context.await_with(rx.changed()).await,
                                    Ok(Ok(()))
                                ) {
                                    let enabled = rx
                                        .borrow_and_update()
                                        .get("cacheWarming")
                                        .and_then(Value::as_str)
                                        != Some("off");
                                    let _ =
                                        switch_regs.extensions.switch(&switch_name, Some(enabled));
                                }
                                Ok(())
                            })
                        });

                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let warmers = warmers.clone();
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    warmers.close();
                                    let _ = registries.extensions.switch(&ext_name, None);
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}
