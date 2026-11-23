use std::sync::Arc;

use pi_rs_ai::{Message, simple_options::estimate_context_tokens};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    delta::Revision,
    host::{Members, ObjectSelf},
    state::Source,
    watch::WatchCall,
};
use pi_rs_durable::{
    EntryRecord,
    harness::{ConversationView, ConversationWindow, Harness},
    session::Observer,
};
use serde_json::{Value, json};

use crate::api::v1::worker::WorkerProcess;

pub(super) fn states(
    object: &mut ObjectSelf,
    members: &mut Members,
    worker: Arc<WorkerProcess>,
    id: u64,
) -> Result<()> {
    let session = worker.init.session()?.id;
    let watching_worker = worker.clone();
    let view = object.follow(move |target, ctx| {
        Box::pin(async move {
            let harness = watching_worker.harness()?;
            let receiving = target.clone();
            let received_session = session.clone();
            let observed = harness
                .durable
                .observe_conversation(
                    id,
                    ConversationWindow::default(),
                    Observer::new(
                        move |change: &WatchCall<ConversationView>, context| {
                            if let WatchCall::Value { value, .. } = change {
                                receiving.set(
                                    view_wire(value, &received_session, id)?.into(),
                                    context,
                                )?;
                            }
                            Ok(())
                        },
                        |_| {},
                    ),
                    &ctx.context,
                )
                .await
                .map_err(invalid)?;
            target.set(
                view_wire(&observed.value, &session, id)?.into(),
                &ctx.context,
            )?;
            Ok(Box::new(move || drop(observed.subscription)) as Box<dyn FnOnce() + Send>)
        })
    })?;
    let model = derive_model(object, view.clone(), worker.clone());
    members.state("view", view)?;
    members.state("model", model)?;
    let stats = object.follow(move |target, ctx| {
        Box::pin(async move {
            let harness = worker.harness()?;
            let (tx, mut rx) = tokio::sync::watch::channel(None::<u64>);
            let observation = harness
                .durable
                .observe_conversation(
                    id,
                    ConversationWindow::default(),
                    Observer::new(
                        move |change: &WatchCall<ConversationView>, _| {
                            if let WatchCall::Value { value, .. } = change {
                                let latest = value.entries.last().map(|entry| entry.id);
                                tx.send_if_modified(|current| {
                                    if *current == latest {
                                        false
                                    } else {
                                        *current = latest;
                                        true
                                    }
                                });
                            }
                            Ok(())
                        },
                        |_| {},
                    ),
                    &ctx.context,
                )
                .await
                .map_err(invalid)?;
            target.set(
                active_stats(&harness.durable, id, &ctx.context)
                    .await?
                    .into(),
                &ctx.context,
            )?;
            let task = tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let next = active_stats(&harness.durable, id, &ctx.context).await;
                    if let Ok(next) = next {
                        let _ = target.set(next.into(), &ctx.context);
                    }
                }
            });
            Ok(Box::new(move || {
                task.abort();
                drop(observation.subscription);
            }) as Box<dyn FnOnce() + Send>)
        })
    })?;
    members.state("stats", stats)?;
    Ok(())
}

fn derive_model(object: &ObjectSelf, view: Source, worker: Arc<WorkerProcess>) -> Source {
    Source::derive(
        object.batch(),
        vec![view],
        move |inputs| {
            let input: Value = inputs[0].to_json().to_serde()?;
            let agent = &input["docs"]["pi.agent"];
            let provider = agent["model"]["provider"].as_str();
            let model_id = agent["model"]["modelId"].as_str();
            if provider.is_none() || model_id.is_none() {
                return Ok(Revision::from_json(Value::Null.into())?);
            }
            let models = worker.harness()?.durable.models();
            let model = provider
                .zip(model_id)
                .and_then(|(provider, model_id)| models.get_model(provider, model_id));
            let metadata = model.map(|model| {
                json!({
                    "provider":model.provider, "modelId":model.id, "name":model.name,
                    "contextWindow":model.context_window, "reasoning":model.reasoning,
                    "thinkingLevels":model.supported_thinking_levels().collect::<Vec<_>>(),
                })
            });
            Ok(Revision::from_json(metadata.unwrap_or(Value::Null).into())?)
        },
        |_| {},
    )
}

async fn active_stats(harness: &Harness, id: u64, context: &Context) -> Result<Value> {
    let conversation = harness
        .conversation(id, context)
        .await
        .map_err(invalid)?
        .ok_or_else(|| Error::Invalid(format!("Unknown conversation {id}")))?;
    let active = conversation.context(None, context).await.map_err(invalid)?;
    let usage = active
        .entries
        .iter()
        .rev()
        .filter_map(|entry| entry.model.as_ref()?.first())
        .find(|message| message["role"] == "assistant")
        .and_then(|message| message.get("usage"));
    let messages = active
        .messages
        .into_iter()
        .map(serde_json::from_value::<Message>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let estimate = estimate_context_tokens(&messages);
    let mut result = json!({"contextTokens":estimate.tokens});
    if let Some(usage) = usage {
        result["latestAssistantUsage"] = usage.clone();
    }
    Ok(result)
}

pub(super) fn response_costs(entries: &[EntryRecord]) -> Value {
    let responses = entries.iter().filter_map(|entry| {
        let message = entry.model.as_ref()?.first()?;
        if message["role"] != "assistant" {
            return None;
        }
        let usage = &message["usage"];
        let numeric = |name| usage[name].as_f64().unwrap_or_default();
        let prompt = numeric("input") + numeric("cacheRead") + numeric("cacheWrite");
        let mut cost = json!({
            "entryId":entry.id.to_string(),"timestamp":message["timestamp"],"cost":usage["cost"]["total"],
            "provider":message["provider"],"model":message["model"],"stopReason":message["stopReason"],
        });
        if prompt > 0. {
            cost["cacheHit"] = json!(numeric("cacheRead") / prompt);
        }
        Some(cost)
    }).collect::<Vec<_>>();
    json!(responses)
}

pub(super) fn entry_wire(entry: &EntryRecord, session: &str, conversation: u64) -> Result<Value> {
    let mut wire = serde_json::to_value(entry)?;
    if let Some(messages) = wire.get_mut("model").and_then(Value::as_array_mut) {
        for (message_index, message) in messages.iter_mut().enumerate() {
            if message["role"] == "assistant" {
                continue;
            }
            let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for (part_index, part) in parts.iter_mut().enumerate() {
                if part["type"] != "image" {
                    continue;
                }
                let query = form_urlencoded::Serializer::new(String::new())
                    .append_pair("session", session)
                    .append_pair("conversation", &conversation.to_string())
                    .append_pair("entry", &entry.id.to_string())
                    .append_pair("message", &message_index.to_string())
                    .append_pair("part", &part_index.to_string())
                    .finish();
                let size = part["data"].as_str().and_then(super::image_size::size);
                let mut image = json!({"type":"image","mimeType":part["mimeType"],"src":format!("/entry-image?{query}")});
                if let Some((width, height)) = size {
                    image["width"] = json!(width);
                    image["height"] = json!(height);
                }
                *part = image;
            }
        }
    }
    Ok(wire)
}

fn view_wire(view: &ConversationView, session: &str, conversation: u64) -> Result<Value> {
    let entries = view
        .entries
        .iter()
        .map(|entry| entry_wire(entry, session, conversation))
        .collect::<Result<Vec<_>>>()?;
    let mut wire = json!({"conversation":view.conversation,"docs":view.docs,"entries":entries});
    if let Some(before) = view.before {
        wire["before"] = json!(before);
    }
    Ok(wire)
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
