//! Worker facet for the `discord` plugin (`p2.discord-ingress`, `p2.discord-link`, `discord` prompt section).

use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Facet, Held, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    ConversationId, ConversationOwnership, DocDefinition, DocToken, EntryId, EntryQuery,
    EntryRecord, ROOT_CONVERSATION_ID, SubmissionKind, SubmissionStatus,
    documents::{DocumentSemantics, InitialDocument, resolve_address},
    harness::{AGENT_DOC, Extension, Harness, INBOX_DOC, LIVE_DOC, PromptSection},
    session::Error as DurableError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{
        Principal,
        p2_contracts::lookup,
        worker::{WorkerProcess, worker_of},
    },
    plugins::{
        send_file::offered_by,
        subagents::contract_worker::{Subagent, SubagentState, subagents_doc},
    },
    runtime::{
        files::safe_file_name,
        main_conversation::{is_top_level, main_conversation},
        session_files::{DOWNLOADS, FILE_CHUNK, UPLOADS, display_name},
        session_lock::{SessionLock, set_session_lock},
    },
};

use super::contract::{
    Command, DISCORD_BATCH, DiscordFileWire, OfferedFile, TurnState, discord_ingress_contract,
};

const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSummary {
    pub channel: String,
    pub guild: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordLinkState {
    pub link: Option<LinkSummary>,
    #[serde(default)]
    pub threads: BTreeMap<String, String>,
}

/// Durable document `p2.discord-link` (v1, session-scoped).
pub fn discord_link_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.discord-link".into(),
        version: 1,
        semantics: DocumentSemantics::Session,
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("link".into(), Value::Null),
                ("threads".into(), json!({})),
            ]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}

/// Builds the `discord` system prompt section for a linked channel or thread.
pub fn discord_prompt(where_str: &str) -> String {
    format!(
        "Your input comes from {where_str}; your final answer of each turn is posted there as the reply, with the files you offer with send_file during the turn attached. Nothing else is posted (not intermediate text, thinking or tool output), so make the final answer complete on its own. Each line names its author with their mention, as in `Ann (<@1234>): text`; write `<@1234>` to mention them."
    )
}

fn discord_extension(worker_ref: Weak<WorkerProcess>) -> Extension {
    Extension {
        name: "p2-discord".into(),
        sections: vec![PromptSection {
            key: "discord".into(),
            tag: true,
            render: Arc::new(move |input, context| {
                let worker_ref = worker_ref.clone();
                Box::pin(async move {
                    let Some(id) = input["conversationId"].as_u64() else {
                        return Ok(None);
                    };
                    let worker = worker_ref
                        .upgrade()
                        .ok_or_else(|| DurableError::invalid("Worker closed"))?;
                    let parts = worker
                        .harness()
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let doc = discord_link_doc();
                    let addr = resolve_address(&doc, None, None)
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let Some(raw) = parts.durable.session.snapshot(&doc, &addr, context).await?
                    else {
                        return Ok(None);
                    };
                    let state: DiscordLinkState =
                        serde_json::from_value(Value::Object((*raw).clone()))
                            .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let Some(link) = state.link else {
                        return Ok(None);
                    };
                    let channel = format!("the Discord channel #{} ({})", link.channel, link.guild);
                    if let Some(thread) = state.threads.get(&id.to_string()) {
                        return Ok(Some(discord_prompt(&format!(
                            "the thread \"{thread}\" of {channel}"
                        ))));
                    }
                    let top = is_top_level(&parts.durable.session, id, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    Ok(top.then(|| discord_prompt(&channel)))
                })
            }),
        }],
        ..Default::default()
    }
}

fn durable_error(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

fn image_mime(path: &str) -> &'static str {
    match path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

async fn download_attachment(
    files: &Held,
    client: &reqwest::Client,
    file: &DiscordFileWire,
    ctx: &Context,
) -> Option<String> {
    let existing = files
        .call(
            "byKey",
            JsonValue::from_serde(json!({ "space": UPLOADS, "key": file.key })),
            ctx,
        )
        .await
        .ok()
        .and_then(|v| v.to_serde().ok())
        .filter(|v| !v.is_null());
    if existing.is_some() {
        return Some(file.key.clone());
    }
    if file.size > MAX_UPLOAD_BYTES {
        return None;
    }
    let path = format!(
        "{}-{}",
        file.key.replace(':', "-"),
        safe_file_name(&file.name)
    );
    let begun = files
        .call(
            "begin",
            JsonValue::from_serde(json!({
                "space": UPLOADS,
                "path": path,
                "mime": image_mime(&path),
                "source": "discord",
                "key": file.key,
                "ref": file.ref_id,
                "meta": { "name": file.name },
            })),
            ctx,
        )
        .await
        .ok()?
        .to_serde()
        .ok()?;
    if begun.get("kind").and_then(Value::as_str) == Some("exists") {
        return Some(file.key.clone());
    }
    let upload_id = begun.get("upload").and_then(Value::as_str)?.to_owned();

    let res: Result<()> = async {
        let resp = ctx
            .await_with(
                client
                    .get(&file.url)
                    .timeout(std::time::Duration::from_secs(30))
                    .send(),
            )
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Error::Invalid(format!(
                "Download failed: {}",
                resp.status().as_u16()
            )));
        }
        let bytes = ctx
            .await_with(resp.bytes())
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if bytes.len() as u64 > MAX_UPLOAD_BYTES {
            return Err(Error::Invalid("Attachment too large".into()));
        }
        let mut offset = 0usize;
        for chunk in bytes.chunks(FILE_CHUNK) {
            files
                .call(
                    "append",
                    JsonValue::from_serde(json!({
                        "upload": upload_id,
                        "offset": offset,
                        "bytes": chunk,
                    })),
                    ctx,
                )
                .await?;
            offset += chunk.len();
        }
        files
            .call(
                "finish",
                JsonValue::from_serde(json!({ "upload": upload_id })),
                ctx,
            )
            .await?;
        Ok(())
    }
    .await;

    match res {
        Ok(()) => Some(file.key.clone()),
        Err(err) => {
            let _ = files
                .call(
                    "cancel",
                    JsonValue::from_serde(json!({ "upload": upload_id })),
                    ctx,
                )
                .await;
            eprintln!("p2 discord: could not download {}: {err}", file.name);
            None
        }
    }
}

async fn offered_files(
    files: &Held,
    conversation: u64,
    start: u64,
    end: u64,
    ctx: &Context,
) -> Result<Vec<OfferedFile>> {
    let mut found = Vec::new();
    let mut after: Option<String> = None;
    let ref_str = offered_by(conversation);
    loop {
        let mut args = Map::new();
        args.insert("space".into(), json!(DOWNLOADS));
        args.insert("ref".into(), json!(ref_str));
        if let Some(ref a) = after {
            args.insert("after".into(), json!(a));
        }
        let page = files
            .call("byRef", JsonValue::from_serde(Value::Object(args)), ctx)
            .await?
            .to_serde()?;
        if let Some(items) = page.get("items").and_then(Value::as_array) {
            for item in items {
                let info: crate::runtime::session_files::FileInfo =
                    serde_json::from_value(item.clone())?;
                if let Some(ref key) = info.key
                    && info.created >= start
                    && info.created <= end
                {
                    found.push(OfferedFile {
                        id: key.clone(),
                        name: display_name(&info).to_owned(),
                        size: info.size,
                    });
                }
            }
        }
        after = page.get("next").and_then(Value::as_str).map(str::to_owned);
        if after.is_none() {
            break;
        }
    }
    Ok(found)
}

async fn entry_of(
    durable: &Harness,
    conversation_id: ConversationId,
    id: EntryId,
    ctx: &Context,
) -> Result<Option<EntryRecord>> {
    let page = durable
        .session
        .scan_entries(
            &EntryQuery {
                conversation_id,
                min_entry_id: Some(id),
                max_entry_id: Some(id),
                ..Default::default()
            },
            1,
            None,
            ctx,
        )
        .await
        .map_err(durable_error)?;
    Ok(page.items.into_iter().next())
}

fn finished_timestamp(entry: &EntryRecord) -> Option<u64> {
    let msg = entry.model.as_deref()?.first()?;
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let stop = msg.get("stopReason").and_then(Value::as_str)?;
    if stop == "stop" || stop == "length" {
        msg.get("timestamp").and_then(Value::as_u64)
    } else {
        None
    }
}

/// Formats a subagent slug from a Discord thread name (`discord-<slug>`).
pub fn thread_subagent_base(label: &str) -> String {
    let safe = safe_file_name(label).to_ascii_lowercase();
    let mut slug = String::with_capacity(safe.len());
    let mut last_dash = false;
    for ch in safe.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let truncated: String = trimmed.chars().take(40).collect();
    let final_slug = if truncated.is_empty() {
        "thread"
    } else {
        truncated.as_str()
    };
    format!("discord-{final_slug}")
}

async fn fork_thread(
    durable: &Harness,
    label: &str,
    before: u64,
    ctx: &Context,
) -> Result<ConversationId> {
    let main = main_conversation(&durable.session, ctx)
        .await
        .map_err(durable_error)?;
    if durable
        .conversation(main, ctx)
        .await
        .map_err(durable_error)?
        .is_none()
    {
        return Err(Error::Invalid("No main conversation".into()));
    }
    let mut cursor = None;
    let mut point_id = None;
    loop {
        let page = durable
            .session
            .scan_entries(
                &EntryQuery {
                    conversation_id: main,
                    ..Default::default()
                },
                200,
                cursor.as_ref(),
                ctx,
            )
            .await
            .map_err(durable_error)?;
        if let Some(found) = page
            .items
            .iter()
            .find(|e| finished_timestamp(e).unwrap_or(u64::MAX) < before)
        {
            point_id = Some(found.id);
            break;
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    let base = thread_subagent_base(label);
    let sub_doc = subagents_doc();
    let sub_addr =
        resolve_address(&sub_doc, Some(ROOT_CONVERSATION_ID), None).map_err(durable_error)?;
    let main_agent_addr = resolve_address(&AGENT_DOC, Some(main), None).map_err(durable_error)?;

    durable
        .commit(
            async move |tx| {
                let mut state: SubagentState = {
                    let raw = tx.doc(&sub_doc, &sub_addr, None).await?;
                    serde_json::from_value(raw.value()?.to_json().to_serde()?)?
                };
                let mut name = base.clone();
                let mut index = 2usize;
                while state.agents.contains_key(&name) {
                    name = format!("{base}-{index}");
                    index += 1;
                }
                let agent_val = tx
                    .doc(&AGENT_DOC, &main_agent_addr, None)
                    .await?
                    .value()?
                    .to_json()
                    .to_serde()?;
                let child = match point_id {
                    Some(pid) => {
                        tx.fork_conversation(main, pid, ConversationOwnership::Ownerless)
                            .await?
                            .id
                    }
                    None => {
                        let c = tx
                            .create_conversation(ConversationOwnership::Ownerless)
                            .await?
                            .id;
                        let c_addr = resolve_address(&AGENT_DOC, Some(c), None)?;
                        tx.doc(&AGENT_DOC, &c_addr, None)
                            .await?
                            .set(&[], JsonValue::from_serde(agent_val.clone()))?;
                        c
                    }
                };
                let model = agent_val
                    .get("model")
                    .and_then(|m| {
                        let p = m.get("provider")?.as_str()?;
                        let id = m.get("modelId")?.as_str()?;
                        Some(format!("{p}/{id}"))
                    })
                    .unwrap_or_default();
                let thinking_level = agent_val
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .unwrap_or("off")
                    .to_owned();
                let tools = agent_val
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                state.agents.insert(
                    name,
                    Subagent {
                        conversation_id: child,
                        model,
                        thinking_level,
                        tools,
                        location: None,
                        unplaced_cwd: None,
                        parent: None,
                        reported: Vec::new(),
                        archived: None,
                    },
                );
                tx.doc(&sub_doc, &sub_addr, None)
                    .await?
                    .set(&[], JsonValue::from_serde(serde_json::to_value(&state)?))?;
                Ok(child)
            },
            ctx,
        )
        .await
        .map_err(durable_error)
}

fn format_dollars(value: f64) -> String {
    if value >= 0.01 || value == 0.0 {
        format!("${value:.4}")
    } else {
        format!("${value:.6}")
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Builds the worker facet for the `discord` plugin.
pub fn worker_facet() -> Result<Facet> {
    let ingress_contract = Arc::new(discord_ingress_contract()?);
    let files_contract = Arc::new(lookup("p2.session-files")?);
    let attachments_contract = Arc::new(lookup("p2.attachments")?);
    let conversation_contract = Arc::new(lookup("p2.conversation")?);
    let http_client = reqwest::Client::new();

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let process = worker_of(&host_handle)?;
        process
            .registries
            .extensions
            .install(discord_extension(Arc::downgrade(&process)))?;

        let proc = process;
        let fc = files_contract.clone();
        let ac = attachments_contract.clone();
        let cc = conversation_contract.clone();
        let client = http_client.clone();

        host.serve(
            ingress_contract.clone(),
            move |_params, principal_raw, self_| {
                let principal: Principal = match principal_raw {
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?,
                    None => return Err(ChordError::new("forbidden", "server only").into()),
                };
                if !matches!(principal, Principal::Server) {
                    return Err(ChordError::new("forbidden", "server only").into());
                }

                let files = self_.get(fc.clone(), JsonValue::Object(IndexMap::new()))?;
                let attachments = self_.get(ac.clone(), JsonValue::Object(IndexMap::new()))?;
                let mut members = Members::variant("server");

                // deliver
                let del_proc = proc.clone();
                let del_files = files.clone();
                let del_att = attachments;
                let del_client = client.clone();
                let del_host = host_handle.clone();
                let del_cc = cc.clone();
                members.method("deliver", move |args, ctx| {
                    let proc = del_proc.clone();
                    let files = del_files.clone();
                    let attachments = del_att.clone();
                    let client = del_client.clone();
                    let host = del_host.clone();
                    let conv_contract = del_cc.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let conversation = val.get("conversation").and_then(Value::as_u64);
                        let request_id = val
                            .get("requestId")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing requestId".into()))?
                            .to_owned();
                        let author = val
                            .get("author")
                            .cloned()
                            .ok_or_else(|| Error::Invalid("missing author".into()))?;
                        let author_id = author
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let author_name = author
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let text = val
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing text".into()))?
                            .to_owned();
                        let discord_ctx = val
                            .get("context")
                            .cloned()
                            .ok_or_else(|| Error::Invalid("missing context".into()))?;
                        let downloads: Vec<DiscordFileWire> = val
                            .get("files")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()?
                            .unwrap_or_default();
                        let lock: Option<SessionLock> = val
                            .get("lock")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()?;

                        let parts = proc.harness()?;
                        set_session_lock(&parts.durable.session, lock.as_ref(), &ctx.context)
                            .await
                            .map_err(durable_error)?;
                        let target = match conversation {
                            Some(c) => c,
                            None => main_conversation(&parts.durable.session, &ctx.context)
                                .await
                                .map_err(durable_error)?,
                        };
                        let channel_name = discord_ctx
                            .get("channel")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let guild_name = discord_ctx
                            .get("guild")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let thread_name = discord_ctx
                            .get("thread")
                            .and_then(Value::as_str)
                            .map(str::to_owned);

                        let link_doc = discord_link_doc();
                        let link_addr =
                            resolve_address(&link_doc, None, None).map_err(durable_error)?;
                        parts
                            .durable
                            .commit(
                                async |tx| {
                                    let raw = tx.doc(&link_doc, &link_addr, None).await?;
                                    let mut st: DiscordLinkState =
                                        serde_json::from_value(raw.value()?.to_json().to_serde()?)?;
                                    st.link = Some(LinkSummary {
                                        channel: channel_name.clone(),
                                        guild: guild_name.clone(),
                                    });
                                    if let (Some(conv), Some(th)) = (conversation, &thread_name) {
                                        st.threads.insert(conv.to_string(), th.clone());
                                    }
                                    raw.set(
                                        &[],
                                        JsonValue::from_serde(serde_json::to_value(&st)?),
                                    )?;
                                    Ok(())
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(durable_error)?;

                        let mut ids = Vec::new();
                        for file in &downloads {
                            if let Some(id) =
                                download_attachment(&files, &client, file, &ctx.context).await
                            {
                                ids.push(json!({ "id": id, "name": file.name }));
                            }
                        }
                        let resolved = if ids.is_empty() {
                            Value::Null
                        } else {
                            let id_strs: Vec<&str> = ids
                                .iter()
                                .filter_map(|i| i.get("id").and_then(Value::as_str))
                                .collect();
                            attachments
                                .call(
                                    "resolve",
                                    JsonValue::from_serde(json!({
                                        "ids": id_strs,
                                        "conversation": target,
                                    })),
                                    &ctx.context,
                                )
                                .await?
                                .to_serde()?
                        };
                        let who = json!({
                            "kind": "guest",
                            "id": format!("discord:{author_id}"),
                            "name": format!("{author_name} · Discord"),
                        });
                        let data = json!({
                            "author": who,
                            "text": text,
                            "attachments": ids,
                            "discord": discord_ctx,
                        });
                        let root = host.root();
                        let chat = root.hold(
                            conv_contract,
                            JsonValue::from_serde(json!({ "conversation": target })),
                        )?;
                        chat.wait(&ctx.context).await?;
                        let mut submit_args = Map::new();
                        submit_args.insert("text".into(), json!(text));
                        submit_args.insert("requestId".into(), json!(request_id));
                        submit_args.insert("whenBusy".into(), json!("steer"));
                        submit_args.insert("author".into(), who);
                        if let Some(arr) = resolved.as_array()
                            && !arr.is_empty()
                        {
                            submit_args.insert("files".into(), Value::Array(arr.clone()));
                        }
                        submit_args.insert(
                            "entry".into(),
                            json!({ "kind": DISCORD_BATCH, "data": data }),
                        );
                        let res = chat
                            .call(
                                "submit",
                                JsonValue::from_serde(Value::Object(submit_args)),
                                &ctx.context,
                            )
                            .await?
                            .to_serde()?;
                        let submission_id =
                            res.get("submissionId").and_then(Value::as_u64).unwrap_or(0);
                        Ok(JsonValue::from_serde(json!({
                            "conversation": target,
                            "submissionId": submission_id,
                        })))
                    })
                })?;

                // unlinked
                let un_proc = proc.clone();
                members.method("unlinked", move |_, ctx| {
                    let proc = un_proc.clone();
                    Box::pin(async move {
                        let parts = proc.harness()?;
                        let doc = discord_link_doc();
                        let addr = resolve_address(&doc, None, None).map_err(durable_error)?;
                        parts
                            .durable
                            .commit(
                                async |tx| {
                                    tx.doc(&doc, &addr, None).await?.set(
                                        &[],
                                        JsonValue::from_serde(
                                            json!({ "link": null, "threads": {} }),
                                        ),
                                    )?;
                                    Ok(())
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(durable_error)?;
                        Ok(JsonValue::Null)
                    })
                })?;

                // turn
                let turn_proc = proc.clone();
                let turn_files = files.clone();
                members.method("turn", move |args, ctx| {
                    let proc = turn_proc.clone();
                    let files = turn_files.clone();
                    Box::pin(async move {
                        let submission_id = args
                            .to_serde()?
                            .get("submissionId")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| Error::Invalid("missing submissionId".into()))?;
                        let parts = proc.harness()?;
                        let Some(sub) = parts
                            .durable
                            .submission(submission_id, &ctx.context)
                            .await
                            .map_err(durable_error)?
                        else {
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Stopped,
                            )?));
                        };
                        let rec = sub.status(&ctx.context).await.map_err(durable_error)?;
                        if rec.kind != SubmissionKind::Input {
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Unanswered {
                                    reason: "Not an input.".into(),
                                    files: Vec::new(),
                                },
                            )?));
                        }
                        if matches!(
                            rec.status,
                            SubmissionStatus::Queued | SubmissionStatus::Placed
                        ) {
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Pending,
                            )?));
                        }
                        if rec.status == SubmissionStatus::Unanswered
                            && rec.reason.as_deref() == Some("aborted")
                        {
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Stopped,
                            )?));
                        }
                        let start = match rec.entry {
                            None => 0,
                            Some(eid) => {
                                entry_of(&parts.durable, rec.conversation_id, eid, &ctx.context)
                                    .await?
                                    .and_then(|e| e.model)
                                    .and_then(|m| m.first().cloned())
                                    .and_then(|msg| msg.get("timestamp").and_then(Value::as_u64))
                                    .unwrap_or(0)
                            }
                        };
                        if rec.status == SubmissionStatus::Unanswered {
                            let sent = offered_files(
                                &files,
                                rec.conversation_id,
                                start,
                                now_ms(),
                                &ctx.context,
                            )
                            .await?;
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Unanswered {
                                    reason: rec
                                        .reason
                                        .as_ref()
                                        .filter(|r| !r.is_empty())
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            "The turn ended without an answer.".into()
                                        }),
                                    files: sent,
                                },
                            )?));
                        }
                        let Some(answer_id) = rec.answer else {
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Unanswered {
                                    reason: "The answer has no text.".into(),
                                    files: Vec::new(),
                                },
                            )?));
                        };
                        let answer =
                            entry_of(&parts.durable, rec.conversation_id, answer_id, &ctx.context)
                                .await?;
                        let msg = answer
                            .as_ref()
                            .and_then(|e| e.model.as_deref())
                            .and_then(|m| m.first());
                        if msg.and_then(|m| m.get("stopReason").and_then(Value::as_str))
                            == Some("aborted")
                        {
                            return Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Stopped,
                            )?));
                        }
                        let text: String = msg
                            .and_then(|m| m.get("content").and_then(Value::as_array))
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter(|p| {
                                        p.get("type").and_then(Value::as_str) == Some("text")
                                    })
                                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default()
                            .trim()
                            .to_owned();
                        let end = msg
                            .and_then(|m| m.get("timestamp").and_then(Value::as_u64))
                            .unwrap_or_else(now_ms);
                        let sent =
                            offered_files(&files, rec.conversation_id, start, end, &ctx.context)
                                .await?;
                        if !text.is_empty() || !sent.is_empty() {
                            Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Answered {
                                    text,
                                    files: sent,
                                    answer: answer_id,
                                },
                            )?))
                        } else {
                            let reason = if msg
                                .and_then(|m| m.get("stopReason").and_then(Value::as_str))
                                == Some("error")
                            {
                                msg.and_then(|m| m.get("errorMessage").and_then(Value::as_str))
                                    .unwrap_or("The model failed.")
                                    .to_owned()
                            } else {
                                "The answer has no text.".into()
                            };
                            Ok(JsonValue::from_serde(serde_json::to_value(
                                TurnState::Unanswered {
                                    reason,
                                    files: sent,
                                },
                            )?))
                        }
                    })
                })?;

                // read
                let read_files = files;
                members.method("read", move |args, ctx| {
                    let files = read_files.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let id = val
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing id".into()))?
                            .to_owned();
                        let offset = val.get("offset").and_then(Value::as_u64).unwrap_or(0);
                        let length = val.get("length").and_then(Value::as_u64).unwrap_or(0);
                        let dl = files
                            .call(
                                "download",
                                JsonValue::from_serde(Value::String(id)),
                                &ctx.context,
                            )
                            .await?
                            .to_serde()?;
                        let Some(file_id) =
                            dl.get("file").and_then(Value::as_str).map(str::to_owned)
                        else {
                            return Err(Error::Invalid("No such file".into()));
                        };
                        files
                            .call(
                                "read",
                                JsonValue::from_serde(json!({
                                    "file": file_id,
                                    "offset": offset,
                                    "length": length,
                                })),
                                &ctx.context,
                            )
                            .await
                    })
                })?;

                // command
                let cmd_proc = proc.clone();
                let cmd_host = host_handle.clone();
                let cmd_cc = cc.clone();
                members.method("command", move |args, ctx| {
                    let proc = cmd_proc.clone();
                    let host = cmd_host.clone();
                    let conv_contract = cmd_cc.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let conversation = val.get("conversation").and_then(Value::as_u64);
                        let command: Command = serde_json::from_value(
                            val.get("command").cloned().unwrap_or(Value::Null),
                        )?;
                        let parts = proc.harness()?;
                        let target = match conversation {
                            Some(c) => c,
                            None => main_conversation(&parts.durable.session, &ctx.context)
                                .await
                                .map_err(durable_error)?,
                        };
                        let root = host.root();
                        let chat = root.hold(
                            conv_contract,
                            JsonValue::from_serde(json!({ "conversation": target })),
                        )?;
                        chat.wait(&ctx.context).await?;

                        let live_addr = resolve_address(&LIVE_DOC, Some(target), None)
                            .map_err(durable_error)?;
                        let live = parts
                            .durable
                            .session
                            .snapshot(&LIVE_DOC, &live_addr, &ctx.context)
                            .await
                            .map_err(durable_error)?;
                        let inbox_addr = resolve_address(&INBOX_DOC, Some(target), None)
                            .map_err(durable_error)?;
                        let inbox = parts
                            .durable
                            .session
                            .snapshot(&INBOX_DOC, &inbox_addr, &ctx.context)
                            .await
                            .map_err(durable_error)?;
                        let queued = inbox
                            .as_ref()
                            .and_then(|d| d.get("items").and_then(Value::as_array))
                            .map(|arr| {
                                arr.iter()
                                    .filter(|i| {
                                        i.get("mode").and_then(Value::as_str) != Some("write")
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        let working = live
                            .as_ref()
                            .is_some_and(|l| l.get("run").is_some_and(|r| !r.is_null()));
                        let compactions: Vec<u64> = live
                            .as_ref()
                            .and_then(|l| l.get("compactions").and_then(Value::as_array))
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|c| c.get("taskId").and_then(Value::as_u64))
                                    .collect()
                            })
                            .unwrap_or_default();

                        match command {
                            Command::Stop => {
                                if !working && queued == 0 && compactions.is_empty() {
                                    return Ok(JsonValue::from_serde(
                                        json!({ "text": "Nothing is running." }),
                                    ));
                                }
                                let mut replies = Vec::new();
                                if working || queued > 0 {
                                    let res = chat
                                        .call("stop", JsonValue::Null, &ctx.context)
                                        .await?
                                        .to_serde()?;
                                    let withdrawn = res
                                        .get("restored")
                                        .and_then(Value::as_array)
                                        .map_or(0, Vec::len);
                                    if withdrawn > 0 {
                                        replies.push(format!(
                                            "Stopped; {withdrawn} queued message{} withdrawn.",
                                            if withdrawn == 1 { "" } else { "s" }
                                        ));
                                    } else {
                                        replies.push("Stopped.".into());
                                    }
                                }
                                let mut aborted = 0usize;
                                for task_id in compactions {
                                    if parts
                                        .durable
                                        .abort_task(task_id, &ctx.context)
                                        .await
                                        .is_ok()
                                    {
                                        let _ = parts
                                            .durable
                                            .wait_for_task(task_id, &ctx.context)
                                            .await;
                                        aborted += 1;
                                    }
                                }
                                if aborted > 0 {
                                    replies.push("Stopped compaction.".into());
                                }
                                let text = if replies.is_empty() {
                                    "Nothing is running.".into()
                                } else {
                                    replies.join(" ")
                                };
                                Ok(JsonValue::from_serde(json!({ "text": text })))
                            }
                            Command::Compact => {
                                chat.call(
                                    "compact",
                                    JsonValue::Object(IndexMap::new()),
                                    &ctx.context,
                                )
                                .await?;
                                Ok(JsonValue::from_serde(
                                    json!({ "text": "Compacting the context." }),
                                ))
                            }
                            Command::Status => {
                                let model_val =
                                    chat.member("model")?.value()?.to_json().to_serde()?;
                                let stats_val =
                                    chat.member("stats")?.value()?.to_json().to_serde()?;
                                let costs_val = chat
                                    .call("costs", JsonValue::Null, &ctx.context)
                                    .await?
                                    .to_serde()?;
                                let total: f64 = costs_val
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|e| e.get("cost").and_then(Value::as_f64))
                                            .sum()
                                    })
                                    .unwrap_or(0.0);
                                let model_str = match (
                                    model_val.get("name").and_then(Value::as_str),
                                    model_val.get("provider").and_then(Value::as_str),
                                    model_val.get("modelId").and_then(Value::as_str),
                                ) {
                                    (Some(n), Some(p), Some(id)) => format!("{n} ({p}/{id})"),
                                    _ => "none".into(),
                                };
                                let tokens = stats_val.get("contextTokens").and_then(Value::as_u64);
                                let window = model_val.get("contextWindow").and_then(Value::as_u64);
                                let usage = match (tokens, window) {
                                    (None, _) => "unknown".into(),
                                    (Some(t), Some(w)) if w > 0 => {
                                        let pct = ((t as f64 / w as f64) * 100.0).round() as u64;
                                        format!("{t} of {w} tokens ({pct}%)")
                                    }
                                    (Some(t), _) => format!("{t} tokens"),
                                };
                                let mut lines = vec![
                                    format!("Model: {model_str}"),
                                    format!("Working: {}", if working { "yes" } else { "no" }),
                                ];
                                if !compactions.is_empty() {
                                    lines.push("Compacting: yes".into());
                                }
                                lines.push(format!("Queued: {queued}"));
                                lines.push(format!("Context: {usage}"));
                                lines.push(format!("Cost: {} so far", format_dollars(total)));
                                Ok(JsonValue::from_serde(json!({ "text": lines.join("\n") })))
                            }
                        }
                    })
                })?;

                // thread
                let th_proc = proc.clone();
                members.method("thread", move |args, ctx| {
                    let proc = th_proc.clone();
                    Box::pin(async move {
                        let val = args.to_serde()?;
                        let name = val
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Invalid("missing name".into()))?;
                        let before = val
                            .get("before")
                            .and_then(Value::as_u64)
                            .unwrap_or_else(now_ms);
                        let parts = proc.harness()?;
                        let conversation =
                            fork_thread(&parts.durable, name, before, &ctx.context).await?;
                        Ok(JsonValue::from_serde(
                            json!({ "conversation": conversation }),
                        ))
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}
