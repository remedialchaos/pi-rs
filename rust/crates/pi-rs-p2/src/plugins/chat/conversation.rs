use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Ctx, Members, ObjectSelf},
    json::JsonValue,
};
use pi_rs_durable::{
    EntryDraft, EntryQuery, ScanOrder, SubmissionStatus,
    documents::resolve_address,
    harness::{
        AbortSubmissionResult, AbortTaskResult, Conversation, Harness, INBOX_DOC, InputEntry,
        LIVE_DOC, SubmissionBody, SubmissionDraft,
    },
};
use serde_json::{Value, json};

use crate::api::v1::{p2_contracts::lookup, worker::WorkerProcess};

use super::{attachments, images, view};

#[derive(Clone)]
struct Chat {
    worker: Arc<WorkerProcess>,
    id: u64,
    session: String,
}

pub(super) fn members(
    object: &mut ObjectSelf,
    worker: Arc<WorkerProcess>,
    id: u64,
) -> Result<Members> {
    let chat = Chat {
        id,
        session: worker.init.session()?.id,
        worker,
    };
    let mut members = Members::new();
    view::states(object, &mut members, chat.worker.clone(), id)?;
    for method in [
        "costs",
        "submit",
        "write",
        "stop",
        "dequeue",
        "entries",
        "image",
        "configure",
        "compact",
        "reset",
        "abortCompaction",
        "dequeueOwn",
    ] {
        let chat = chat.clone();
        members.method(method, move |args, ctx| {
            let chat = chat.clone();
            Box::pin(async move { Ok(chat.call(method, args.to_serde()?, &ctx).await?.into()) })
        })?;
    }
    Ok(members)
}

impl Chat {
    async fn call(&self, method: &str, args: Value, ctx: &Ctx) -> Result<Value> {
        let context = &ctx.context;
        let harness = self.worker.harness()?;
        let durable = &harness.durable;
        let target = durable
            .conversation(self.id, context)
            .await
            .map_err(invalid)?
            .ok_or_else(|| Error::Invalid(format!("Unknown conversation {}", self.id)))?;
        match method {
            "costs" => Ok(view::response_costs(
                &target
                    .context(None, context)
                    .await
                    .map_err(invalid)?
                    .entries,
            )),
            "entries" => self.entries(&target, &args, context).await,
            "image" => images::image_at(&target, serde_json::from_value(args)?, context).await,
            "submit" => self.submit(&target, &args, ctx).await,
            "write" => {
                self.worker.registries.wait_for_input(context).await?;
                let id = request_key(&args)?;
                let draft = SubmissionDraft {
                    request_id: Some(id),
                    notify: None,
                    body: SubmissionBody::Write {
                        entry: EntryDraft {
                            kind: text(&args, "kind")?.into(),
                            data: args.get("data").cloned(),
                            ..Default::default()
                        },
                    },
                };
                let result = target.submit(draft, context).await.map_err(invalid)?;
                Ok(json!({"submissionId":result.id}))
            }
            "stop" => {
                let before = queued(durable, self.id, context).await?;
                target.abort(false, context).await.map_err(invalid)?;
                let mut restored = Vec::new();
                for item in before.iter().filter_map(restorable) {
                    if let Some(record) =
                        durable.submission(item.0, context).await.map_err(invalid)?
                    {
                        let record = record.status(context).await.map_err(invalid)?;
                        if record.status == SubmissionStatus::Unanswered
                            && record.reason.as_deref() == Some("aborted")
                        {
                            restored.push(item.1);
                        }
                    }
                }
                push_status(ctx).await?;
                Ok(json!({"restored":restored}))
            }
            "dequeue" => {
                let id = id_at(&args, "submissionId")?;
                let item = if args["restore"] == true {
                    queued(durable, self.id, context)
                        .await?
                        .into_iter()
                        .find(|item| item["id"] == id)
                } else {
                    None
                };
                let result = durable
                    .abort_submission(id, Some(self.id), context)
                    .await
                    .map_err(invalid)?;
                if matches!(result, AbortSubmissionResult::NotFound) {
                    return Err(Error::Invalid("Unknown queued message".into()));
                }
                let restored = if matches!(result, AbortSubmissionResult::Aborted) {
                    item.as_ref().and_then(restorable).map(|(_, item)| item)
                } else {
                    None
                };
                Ok(json!({"restored":restored.into_iter().collect::<Vec<_>>()}))
            }
            "dequeueOwn" => {
                let author = text(&args, "authorId")?;
                let mut restored = Vec::new();
                let mut skipped = 0;
                for (id, item) in queued(durable, self.id, context)
                    .await?
                    .iter()
                    .filter_map(restorable)
                    .filter(|(_, item)| item["author"]["id"] == author)
                {
                    if matches!(
                        durable
                            .abort_submission(id, Some(self.id), context)
                            .await
                            .map_err(invalid)?,
                        AbortSubmissionResult::Aborted
                    ) {
                        restored.push(item);
                    } else {
                        skipped += 1;
                    }
                }
                Ok(json!({"restored":restored,"skipped":skipped}))
            }
            "configure" => {
                if let Some(model) = args.get("model")
                    && durable
                        .models()
                        .get_model(text(model, "provider")?, text(model, "modelId")?)
                        .is_none()
                {
                    return Err(Error::Invalid("Unknown model".into()));
                }
                let fields = serde_json::from_value(args)?;
                target.configure(&fields, context).await.map_err(invalid)?;
                let after: Vec<_> = self
                    .worker
                    .registries
                    .conversation_configured
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .values()
                    .cloned()
                    .collect();
                for callback in after {
                    callback(target.clone(), context.clone()).await?;
                }
                Ok(Value::Null)
            }
            "compact" => {
                let task = target
                    .compact(optional_text(&args, "instructions"), context)
                    .await
                    .map_err(invalid)?;
                push_status(ctx).await?;
                Ok(json!({"taskId":task}))
            }
            "reset" => {
                target
                    .reset(optional_text(&args, "handoff"), context)
                    .await
                    .map_err(invalid)?;
                Ok(Value::Null)
            }
            "abortCompaction" => {
                let address = resolve_address(&LIVE_DOC, Some(self.id), None).map_err(invalid)?;
                let state = durable
                    .snapshot(&LIVE_DOC, &address, context)
                    .await
                    .map_err(invalid)?;
                let tasks = state
                    .as_deref()
                    .and_then(|doc| doc.get("compactions"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten();
                let mut aborted = 0;
                for id in tasks.filter_map(|task| task["taskId"].as_u64()) {
                    if matches!(
                        durable.abort_task(id, context).await.map_err(invalid)?,
                        AbortTaskResult::Marked
                    ) {
                        durable.wait_for_task(id, context).await.map_err(invalid)?;
                        aborted += 1;
                    }
                }
                push_status(ctx).await?;
                Ok(json!({"aborted":aborted}))
            }
            name => Err(Error::Invalid(format!(
                "Unknown conversation method {name}"
            ))),
        }
    }

    async fn submit(&self, target: &Conversation, args: &Value, ctx: &Ctx) -> Result<Value> {
        let context = &ctx.context;
        self.worker.registries.wait_for_input(context).await?;
        let text = text(args, "text")?;
        let author = &args["author"];
        let files = attachments::files(args)?;
        let content = attachments::content(
            text,
            author,
            &files,
            target,
            &self.worker,
            args["maxImages"].as_u64(),
            context,
        )
        .await?;
        let data = json!({
            "author":author,"text":text,"attachments":files.iter()
                .map(|file| json!({"id":file.id,"name":file.name})).collect::<Vec<_>>(),
        });
        let entry = if let Some(entry) = args.get("entry") {
            serde_json::from_value(entry.clone())?
        } else {
            InputEntry {
                kind: "p2.input".into(),
                data: Some(data),
            }
        };
        let draft = SubmissionDraft {
            request_id: Some(request_key(args)?),
            notify: None,
            body: SubmissionBody::Input {
                content,
                entry: Some(entry),
                when_idle: None,
                on_abort: None,
                when_busy: Some(args["whenBusy"].as_str().unwrap_or("steer").into()),
            },
        };
        let id = target.submit(draft, context).await.map_err(invalid)?.id;
        push_status(ctx).await?;
        Ok(json!({"submissionId":id}))
    }

    async fn entries(
        &self,
        target: &Conversation,
        args: &Value,
        context: &Context,
    ) -> Result<Value> {
        let order = match args["order"].as_str() {
            Some("ascending") => Some(ScanOrder::Ascending),
            Some("descending") => Some(ScanOrder::Descending),
            _ => None,
        };
        let limit = usize::try_from(id_at(args, "limit")?).map_err(invalid)?;
        let page = target
            .entries(
                EntryQuery {
                    conversation_id: self.id,
                    min_entry_id: args["minEntryId"].as_u64(),
                    max_entry_id: args["maxEntryId"].as_u64(),
                    order,
                },
                limit,
                None,
                context,
            )
            .await
            .map_err(invalid)?;
        let mut items = Vec::new();
        let mut size = 0;
        for entry in &page.items {
            if !items.is_empty() && size >= 1_000_000 {
                break;
            }
            let entry = view::entry_wire(entry, &self.session, self.id)?;
            size += entry.to_string().encode_utf16().count();
            items.push(entry);
        }
        let more = items.len() < page.items.len() || page.next.is_some();
        Ok(json!({"items":items,"more":more}))
    }
}

async fn queued(harness: &Harness, id: u64, context: &Context) -> Result<Vec<Value>> {
    let address = resolve_address(&INBOX_DOC, Some(id), None).map_err(invalid)?;
    let doc = harness
        .snapshot(&INBOX_DOC, &address, context)
        .await
        .map_err(invalid)?;
    let mut items = doc
        .as_deref()
        .and_then(|doc| doc.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.sort_by_key(|item| item["id"].as_u64());
    Ok(items)
}

fn restorable(item: &Value) -> Option<(u64, Value)> {
    if item["mode"] == "write" {
        return None;
    }
    let data = &item["entry"]["data"];
    let text = data["text"]
        .as_str()
        .filter(|text| !text.trim().is_empty())?;
    let author = data.get("author")?;
    author["id"].as_str()?;
    author["name"].as_str()?;
    if !matches!(author["kind"].as_str(), Some("owner" | "guest")) {
        return None;
    }
    Some((item["id"].as_u64()?, json!({"author":author,"text":text})))
}

async fn push_status(ctx: &Ctx) -> Result<()> {
    ctx.get(Arc::new(lookup("p2.status-report")?), json!({}).into())?
        .call("push", JsonValue::Null, &ctx.context)
        .await?;
    Ok(())
}

fn request_key(args: &Value) -> Result<String> {
    Ok(format!(
        "{}:{}",
        text(&args["author"], "id")?,
        text(args, "requestId")?
    ))
}

fn optional_text(args: &Value, key: &str) -> Option<String> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

pub(super) fn id_at(args: &Value, key: &str) -> Result<u64> {
    args[key]
        .as_u64()
        .ok_or_else(|| Error::Invalid(format!("Invalid {key}")))
}

pub(super) fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("Invalid {key}")))
}

pub(super) fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
