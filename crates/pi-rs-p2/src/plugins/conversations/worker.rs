use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    ASSISTANT_ENTRY, EntryQuery,
    documents::resolve_address,
    harness::{Conversation, Harness, InputEntry, LIVE_DOC, SubmissionBody, SubmissionDraft},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{Principal, p2_contracts::lookup, worker::worker_of},
    runtime::main_conversation::main_conversation,
};

const PEER_MESSAGE: &str = "p2.peer-message";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Send {
    source: String,
    title: String,
    request_id: String,
    message: String,
    follow_up: Option<bool>,
}

pub(super) fn facet() -> Result<Facet> {
    let peer = Arc::new(lookup("p2.peer-conversation")?);
    let status = Arc::new(lookup("p2.status-report")?);
    let chat = Arc::new(lookup("p2.conversation")?);
    let drafts = Arc::new(lookup("p2.draft-store")?);
    Ok(define_facet(move |host| {
        let worker = worker_of(&host.host())?;
        super::tool::install(&worker, host.host().downgrade())?;
        let (status, chat, drafts) = (status.clone(), chat.clone(), drafts.clone());
        host.serve(
            peer.clone(),
            move |_, principal, _| {
                if principal.map(Principal::deserialize).transpose()? != Some(Principal::Server) {
                    return Err(ChordError::new("forbidden", "server broker only").into());
                }
                let mut members = Members::variant("broker");
                let sending_worker = worker.clone();
                let status = status.clone();
                members.method("send", move |args, ctx| {
                    let (worker, status) = (sending_worker.clone(), status.clone());
                    Box::pin(async move {
                        let args = serde_json::from_value(args.to_serde()?)?;
                        worker.call(async {
                            worker.registries.wait_for_input(&ctx.context).await?;
                            let harness = worker.harness()?;
                            let main = main(&harness.durable, &ctx.context).await?;
                            let id = submit(&main, args, &ctx.context).await?;
                            ctx.get(status, json!({}).into())?
                                .call("push", JsonValue::Null, &ctx.context)
                                .await?;
                            Ok(json!({"submissionId":id}).into())
                        }).await
                    })
                })?;
                let answering_worker = worker.clone();
                members.method("answer", move |_, ctx| {
                    let worker = answering_worker.clone();
                    Box::pin(async move {
                        worker.call(async {
                            let harness = worker.harness()?;
                            let main = main(&harness.durable, &ctx.context).await?;
                            answer(&harness.durable, &main, &ctx.context).await
                        }).await
                    })
                })?;
                let stopping_worker = worker.clone();
                let (chat, drafts) = (chat.clone(), drafts.clone());
                members.method("stop", move |_, ctx| {
                    let (worker, chat, drafts) = (stopping_worker.clone(), chat.clone(), drafts.clone());
                    Box::pin(async move {
                        worker.call(async {
                            let harness = worker.harness()?;
                            let main = main(&harness.durable, &ctx.context).await?;
                            let stopped = busy(&harness.durable, &main, &ctx.context).await?;
                            let result = ctx.get(chat, json!({"conversation":main.id}).into())?
                                .call("stop", JsonValue::Null, &ctx.context).await?.to_serde()?;
                            if result["restored"].as_array().is_some_and(|items| !items.is_empty()) {
                                ctx.get(drafts, json!({}).into())?
                                    .call("add", json!({"conversation":main.id,"items":result["restored"]}).into(), &ctx.context)
                                    .await?;
                            }
                            Ok(json!({"stopped":stopped}).into())
                        }).await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}

async fn main(harness: &Harness, context: &Context) -> Result<Conversation> {
    let id = main_conversation(harness, context).await.map_err(invalid)?;
    harness
        .conversation(id, context)
        .await
        .map_err(invalid)?
        .ok_or_else(|| Error::Invalid("No main conversation".into()))
}

async fn busy(harness: &Harness, conversation: &Conversation, context: &Context) -> Result<bool> {
    let address = resolve_address(&LIVE_DOC, Some(conversation.id), None).map_err(invalid)?;
    let state = harness
        .snapshot(&LIVE_DOC, &address, context)
        .await
        .map_err(invalid)?;
    Ok(state.is_some_and(|state| state.contains_key("run")))
}

async fn submit(main: &Conversation, args: Send, context: &Context) -> Result<u64> {
    let sender = format!(
        "<sender kind=\"session\" id=\"{}\" title=\"{}\"/>",
        escape_attribute(&args.source),
        escape_attribute(&args.title),
    );
    let draft = SubmissionDraft {
        request_id: Some(format!("conversation:{}:{}", args.source, args.request_id)),
        notify: None,
        body: SubmissionBody::Input {
            content: json!([{"type":"text","text":sender},{"type":"text","text":args.message}]),
            entry: Some(InputEntry {
                kind: PEER_MESSAGE.into(),
                data: Some(
                    json!({"from":{"id":args.source,"title":args.title},"text":args.message}),
                ),
            }),
            when_busy: Some(
                if args.follow_up == Some(true) {
                    "followUp"
                } else {
                    "steer"
                }
                .into(),
            ),
            when_idle: None,
            on_abort: Some("keep".into()),
        },
    };
    Ok(main.submit(draft, context).await.map_err(invalid)?.id)
}

async fn answer(harness: &Harness, main: &Conversation, context: &Context) -> Result<JsonValue> {
    let mut cursor = None;
    let mut reply = Map::new();
    let mut limited = false;
    for _ in 0..20 {
        let page = main
            .entries(EntryQuery::default(), 100, cursor.as_ref(), context)
            .await
            .map_err(invalid)?;
        let text = page.items.iter().find_map(|entry| {
            if entry.kind != ASSISTANT_ENTRY.kind() {
                return None;
            }
            let message = entry.model.as_ref()?.first()?;
            if message["stopReason"] != "stop" {
                return None;
            }
            let text: String = message["content"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect();
            (!text.trim().is_empty()).then_some(text)
        });
        if let Some(text) = text {
            let utf16: Vec<_> = text.encode_utf16().take(65_537).collect();
            let truncated = utf16.len() > 65_536;
            reply.insert(
                "text".into(),
                json!(if truncated {
                    String::from_utf16_lossy(&utf16[..65_536])
                } else {
                    text
                }),
            );
            reply.insert("truncated".into(), json!(truncated));
            limited = false;
            break;
        }
        cursor = page.next;
        limited = cursor.is_some();
        if cursor.is_none() {
            break;
        }
    }
    reply.entry("truncated").or_insert(Value::Bool(false));
    reply.insert("busy".into(), json!(busy(harness, main, context).await?));
    reply.insert("limited".into(), json!(limited));
    Ok(Value::Object(reply).into())
}

fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
