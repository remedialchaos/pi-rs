//! `fit-images` built-in plugin: Durable `afterTool` extension that fits tool-result images
//! to the conversation model's limits before committing them to the transcript.

use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::Member,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    DocumentAddress, DocumentScope,
    harness::{AGENT_DOC, Extension, HookHandlers, define_extension, hook, tool_task},
    images::{FIT_IMAGES_EXTENSION_NAME, ImageProcessor, fit_tool_result},
    session::Error as DurableError,
    tools::{ToolError, ToolResult},
};
use serde_json::{Value, json};

use crate::api::v1::{FacetKind, PluginRegistry, contracts::Contract, worker::worker_of};

/// Builds the Durable `fit-images` extension backed by `processor`.
pub fn create_fit_images_extension(processor: Arc<dyn ImageProcessor>) -> Extension {
    let mut handlers = HookHandlers::new();
    handlers.insert(
        "afterTool".into(),
        Arc::new(move |payload, invocation, context| {
            let processor = processor.clone();
            Box::pin(async move {
                let nested = invocation.record.owner.is_some();
                let result_val = payload
                    .get("result")
                    .cloned()
                    .ok_or_else(|| DurableError::invalid("afterTool payload missing result"))?;
                let mut result: ToolResult<Value> = serde_json::from_value(result_val)?;
                let conversation_id = invocation.record.conversation_id;
                let harness = invocation.harness();
                let address = DocumentAddress {
                    kind: AGENT_DOC.kind.clone(),
                    scope: DocumentScope::Conversation { conversation_id },
                    key: None,
                };
                let changed = fit_tool_result(nested, &mut result, processor.as_ref(), || async {
                    let agent = harness
                        .snapshot(&AGENT_DOC, &address, context)
                        .await
                        .map_err(|e| ToolError::Message(e.to_string()))?;
                    let model = agent
                        .as_ref()
                        .and_then(|doc| doc.get("model"))
                        .and_then(|m| {
                            m.get("provider")
                                .and_then(Value::as_str)
                                .zip(m.get("modelId").and_then(Value::as_str))
                        })
                        .and_then(|(provider, model_id)| {
                            harness.models().get_model(provider, model_id)
                        });
                    Ok(model)
                })
                .await
                .map_err(|e| DurableError::invalid(e.to_string()))?;
                if changed {
                    Ok(Some(serde_json::to_value(result)?))
                } else {
                    Ok(None)
                }
            })
        }),
    );

    define_extension(Extension {
        name: FIT_IMAGES_EXTENSION_NAME.into(),
        hooks: vec![hook(&tool_task(), handlers)],
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let mut member_defs = IndexMap::new();
    member_defs.insert(
        "active".into(),
        Member::State(JsonValue::from_serde(json!({ "type": "boolean" }))),
    );
    let local_contract = Arc::new(Contract::local(
        "p2.fit-images",
        1,
        Vec::new(),
        member_defs,
    )?);
    registry.register(
        "fit-images",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                local_contract.clone(),
                move |_, _, object| {
                    if let Ok(worker) = worker_of(&object.host()) {
                        let processor: Arc<dyn ImageProcessor> = Arc::new(worker.images);
                        let extension = create_fit_images_extension(processor);
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    let mut members = Members::new();
                    members.state("active", object.writable(JsonValue::Bool(true))?)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
