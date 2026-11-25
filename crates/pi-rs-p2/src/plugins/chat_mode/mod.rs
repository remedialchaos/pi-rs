//! Chat mode plugin: per-conversation switch, agent name, waiting count, and send router.

pub mod contract;
pub mod digest;
pub mod display;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    host::{Facet, Host, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    sync::SyncFuture,
    watch::WatchCall,
};
use pi_rs_durable::{
    ConversationId, DocDefinition, DocToken, EntryQuery, EntryRecord, INBOX_DOC, ScanOrder,
    documents::{DocumentSemantics, InitialDocument, LatestFork, resolve_address},
    harness::{Harness, InboxState},
    session::Observer,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{
        FacetKind, PluginRegistry,
        p2_contracts::lookup,
        server::{install_service, service_of},
        worker::worker_of,
    },
    runtime::{main_conversation::agent_conversation, principals::Principal},
};

use contract::{
    Attachment, AttachmentRef, Author, CHAT_ENTRY, ChatMessage, DEFAULT_AGENT_NAME,
    MAX_ATTACHMENTS, chat_mode_contract, chat_mode_setting_contract, is_valid_agent_name,
    mentions_agent,
};
use digest::{
    ComposeMentionInput, compose_mention, digest_files, quote_of, read_enough, unmentioned,
};

pub const SEND_ROUTERS_SERVICE: &str = "p2.send-routers";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendRouterInput {
    pub conversation: u64,
    pub author: Author,
    pub text: String,
    pub request_id: String,
    pub when_busy: Option<String>,
    pub attachments: Vec<String>,
    pub reply_to: Option<u64>,
}

pub type SendRouterFn = Arc<
    dyn Fn(SendRouterInput, pi_rs_chord::host::Ctx) -> SyncFuture<Option<JsonValue>> + Send + Sync,
>;

#[derive(Default)]
pub struct SendRoutersService {
    routers: Mutex<IndexMap<String, SendRouterFn>>,
}

impl SendRoutersService {
    pub fn contribute(&self, name: impl Into<String>, router: SendRouterFn) {
        self.routers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into(), router);
    }

    pub fn get(&self, name: &str) -> Option<SendRouterFn> {
        self.routers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    pub fn list(&self) -> Vec<(String, SendRouterFn)> {
        self.routers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

pub fn get_or_install_send_routers(host: &Host) -> Result<Arc<SendRoutersService>> {
    match service_of::<SendRoutersService>(host, SEND_ROUTERS_SERVICE) {
        Ok(existing) => Ok(existing),
        Err(_) => {
            let service = Arc::new(SendRoutersService::default());
            install_service(host, SEND_ROUTERS_SERVICE, &service)?;
            Ok(service)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatModeSettingsState {
    pub enabled: bool,
    pub name: String,
}

impl Default for ChatModeSettingsState {
    fn default() -> Self {
        Self {
            enabled: false,
            name: DEFAULT_AGENT_NAME.to_string(),
        }
    }
}

/// Durable document storing chat mode switch and agent name per agent conversation.
pub fn chat_mode_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "chat-mode.settings".into(),
        version: 2,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("enabled".into(), json!(false)),
                ("name".into(), json!(DEFAULT_AGENT_NAME)),
            ]))
        })),
        migrate: Some(Arc::new(|value, _version| {
            let enabled = value.get("enabled").and_then(Value::as_bool) == Some(true);
            Ok(Map::from_iter([
                ("enabled".into(), json!(enabled)),
                ("name".into(), json!(DEFAULT_AGENT_NAME)),
            ]))
        })),
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}

/// A conversation's newest entries, newest first, until `unmentioned` has what it needs.
pub async fn recent_entries(
    durable: &Harness,
    id: ConversationId,
    ctx: &Context,
    limit: usize,
) -> Result<Vec<EntryRecord>> {
    let Some(conversation) = durable
        .conversation(id, ctx)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
    else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    let mut before: Option<u64> = None;
    while entries.len() < limit && !read_enough(&entries) {
        let max_entry_id = match before {
            Some(b) if b > 1 => Some(b - 1),
            Some(_) => break,
            None => None,
        };
        let page = conversation
            .entries(
                EntryQuery {
                    conversation_id: id,
                    min_entry_id: None,
                    max_entry_id,
                    order: Some(ScanOrder::Descending),
                },
                256,
                None,
                ctx,
            )
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        for entry in page.items {
            before = Some(entry.id);
            entries.push(entry);
        }
        if page.next.is_none() {
            break;
        }
    }
    Ok(entries)
}

/// What a reply quotes, read from the entry itself in the same conversation.
pub async fn reply_target(
    durable: &Harness,
    id: ConversationId,
    entry_id: u64,
    ctx: &Context,
) -> Result<contract::ReplyTo> {
    let conversation = durable
        .conversation(id, ctx)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let quote = if let Some(conv) = conversation {
        let page = conv
            .entries(
                EntryQuery {
                    conversation_id: id,
                    min_entry_id: Some(entry_id),
                    max_entry_id: Some(entry_id),
                    order: None,
                },
                1,
                None,
                ctx,
            )
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        page.items
            .first()
            .filter(|e| e.id == entry_id)
            .and_then(quote_of)
    } else {
        None
    };
    quote.ok_or_else(|| {
        ChordError::invalid("args", "That message cannot be replied to", None).into()
    })
}

async fn resolve_attachments(
    ctx: &pi_rs_chord::host::Ctx,
    attachments_contract: &Arc<Contract>,
    ids: &[String],
    conversation: u64,
) -> Result<Vec<Attachment>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let held = ctx.get(
        attachments_contract.clone(),
        JsonValue::Object(Default::default()),
    )?;
    let mut files = Vec::new();
    for chunk in ids.chunks(MAX_ATTACHMENTS) {
        let res = held
            .call(
                "resolve",
                JsonValue::from_serde(json!({
                    "ids": chunk,
                    "conversation": conversation,
                })),
                &ctx.context,
            )
            .await?;
        let resolved: Vec<Attachment> = serde_json::from_value(res.to_serde()?)?;
        files.extend(resolved);
    }
    Ok(files)
}

fn parse_settings(val: &JsonValue) -> ChatModeSettingsState {
    val.to_serde()
        .ok()
        .and_then(|v| serde_json::from_value::<ChatModeSettingsState>(v).ok())
        .unwrap_or_default()
}

fn can_view(principal: Option<&Value>, session: &str) -> bool {
    let Some(val) = principal else {
        return false;
    };
    serde_json::from_value::<Principal>(val.clone())
        .map(|p| p.can_view(session))
        .unwrap_or(false)
}

fn is_owner(principal: Option<&Value>) -> bool {
    let Some(val) = principal else {
        return false;
    };
    serde_json::from_value::<Principal>(val.clone())
        .map(|p| p.is_owner())
        .unwrap_or(false)
}

fn parse_conversation_id(params: &JsonValue) -> Result<u64> {
    let val = params.to_serde()?;
    val.get("conversation")
        .and_then(Value::as_u64)
        .filter(|&id| id >= 1)
        .ok_or_else(|| ChordError::invalid("params", "missing conversation", None).into())
}

pub fn worker_facet() -> Result<Facet> {
    let setting_contract = chat_mode_setting_contract()?;
    let mode_contract = chat_mode_contract()?;
    let conv_contract = Arc::new(lookup("p2.conversation")?);
    let attachments_contract = Arc::new(lookup("p2.attachments")?);

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let routers = get_or_install_send_routers(&host_handle)?;
        let attachments_for_router = attachments_contract.clone();
        let conv_for_router = conv_contract.clone();
        let host_for_router = host_handle.clone();
        routers.contribute(
            "chat-mode",
            Arc::new(move |input: SendRouterInput, ctx| {
                let attachments_contract = attachments_for_router.clone();
                let conv_contract = conv_for_router.clone();
                let host_handle = host_for_router.clone();
                Box::pin(async move {
                    let worker = worker_of(&host_handle)?;
                    let durable = worker.harness()?.durable.clone();
                    let id: ConversationId = input.conversation;
                    let holder = agent_conversation(&durable.session, id, &ctx.context)
                        .await
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    let doc = chat_mode_doc();
                    let addr = resolve_address(&doc, Some(holder), None)
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    let settings = durable
                        .session
                        .snapshot(&doc, &addr, &ctx.context)
                        .await
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .map(|obj| {
                            serde_json::from_value::<ChatModeSettingsState>(Value::Object(
                                (*obj).clone(),
                            ))
                            .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    if !settings.enabled {
                        return Ok(None);
                    }

                    let chat = ctx.get(
                        conv_contract,
                        JsonValue::from_serde(json!({ "conversation": input.conversation })),
                    )?;
                    let answering = match input.reply_to {
                        Some(reply_id) => {
                            Some(reply_target(&durable, id, reply_id, &ctx.context).await?)
                        }
                        None => None,
                    };

                    if !mentions_agent(&input.text, &settings.name) {
                        let files = resolve_attachments(
                            &ctx,
                            &attachments_contract,
                            &input.attachments,
                            input.conversation,
                        )
                        .await?;
                        let data = ChatMessage {
                            author: input.author.clone(),
                            text: input.text,
                            attachments: files
                                .into_iter()
                                .map(|f| AttachmentRef {
                                    id: f.id,
                                    name: f.name,
                                })
                                .collect(),
                            reply_to: answering,
                        };
                        let res = chat
                            .call(
                                "write",
                                JsonValue::from_serde(json!({
                                    "kind": CHAT_ENTRY,
                                    "data": data,
                                    "requestId": input.request_id,
                                    "author": input.author,
                                })),
                                &ctx.context,
                            )
                            .await?;
                        return Ok(Some(res));
                    }

                    let inbox_addr = resolve_address(&INBOX_DOC, Some(id), None)
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    let inbox: Option<InboxState> = durable
                        .session
                        .snapshot(&INBOX_DOC, &inbox_addr, &ctx.context)
                        .await
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .and_then(|obj| serde_json::from_value(Value::Object((*obj).clone())).ok());
                    let entries = recent_entries(&durable, id, &ctx.context, 2000).await?;
                    let earlier = unmentioned(&entries, inbox.as_ref());
                    let all_ids = digest_files(&earlier, &input.attachments);
                    let files = resolve_attachments(
                        &ctx,
                        &attachments_contract,
                        &all_ids,
                        input.conversation,
                    )
                    .await?;
                    let own: Vec<AttachmentRef> = files
                        .iter()
                        .filter(|f| input.attachments.contains(&f.id))
                        .map(|f| AttachmentRef {
                            id: f.id.clone(),
                            name: f.name.clone(),
                        })
                        .collect();
                    let submit = compose_mention(ComposeMentionInput {
                        author: input.author,
                        text: input.text,
                        chat: &earlier,
                        reply_to: answering,
                        attachments: own,
                        files,
                        request_id: input.request_id,
                        when_busy: input.when_busy,
                    });
                    let res = chat
                        .call(
                            "submit",
                            JsonValue::from_serde(serde_json::to_value(submit)?),
                            &ctx.context,
                        )
                        .await?;
                    Ok(Some(res))
                })
            }),
        );

        let conv_for_setting = conv_contract.clone();
        let routers_keepalive = routers.clone();
        host.serve(
            setting_contract.clone(),
            move |params, _, object| {
                let _keepalive = &routers_keepalive;
                let conversation = parse_conversation_id(params)?;
                let id: ConversationId = conversation;
                let host_handle = object.host();
                let chat = object.get(
                    conv_for_setting.clone(),
                    JsonValue::from_serde(json!({ "conversation": conversation })),
                )?;
                let chat_view = chat.member("view")?;

                let holder = Arc::new(Mutex::new(id));
                let holder_follow = holder.clone();
                let host_follow = host_handle.clone();
                let settings = object.follow(move |target, ctx| {
                    Box::pin(async move {
                        let worker = worker_of(&host_follow)?;
                        let durable = worker.harness()?.durable.clone();
                        let h = agent_conversation(&durable.session, id, &ctx.context)
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        *holder_follow.lock().unwrap_or_else(|e| e.into_inner()) = h;

                        let doc = chat_mode_doc();
                        let addr = resolve_address(&doc, Some(h), None)
                            .map_err(|e| Error::Invalid(e.to_string()))?;

                        let make_observer = |t: Source| {
                            let ended = t.clone();
                            Observer::new(
                                move |call: &WatchCall<Arc<Map<String, Value>>>, c| {
                                    match call {
                                        WatchCall::Value { value, .. } => {
                                            t.set(
                                                JsonValue::from_serde(Value::Object(
                                                    (**value).clone(),
                                                )),
                                                c,
                                            )?;
                                        }
                                        WatchCall::Failed { error } => {
                                            t.fail(error.clone(), c)?;
                                        }
                                    }
                                    Ok(())
                                },
                                move |_| ended.detach("ended"),
                            )
                        };

                        let obs = match durable
                            .session
                            .observe_doc(&doc, &addr, make_observer(target.clone()), &ctx.context)
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?
                        {
                            Some(obs) => obs,
                            None => {
                                durable
                                    .session
                                    .commit(
                                        async |tx| {
                                            tx.doc(&doc, &addr, None).await?;
                                            Ok(())
                                        },
                                        &ctx.context,
                                    )
                                    .await
                                    .map_err(|e| Error::Invalid(e.to_string()))?;
                                durable
                                    .session
                                    .observe_doc(
                                        &doc,
                                        &addr,
                                        make_observer(target.clone()),
                                        &ctx.context,
                                    )
                                    .await
                                    .map_err(|e| Error::Invalid(e.to_string()))?
                                    .ok_or_else(|| {
                                        Error::Invalid("failed to observe chat-mode doc".into())
                                    })?
                            }
                        };

                        target.set(
                            JsonValue::from_serde(Value::Object((*obs.value).clone())),
                            &ctx.context,
                        )?;
                        let sub = obs.subscription;
                        Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let waiting = object.writable(JsonValue::from_serde(json!(0)))?;

                // Recompute waiting count whenever settings or chat.view changes.
                let settings_sub = settings.clone();
                let view_sub = chat_view.clone();
                let waiting_update = waiting.clone();
                let host_watch = host_handle.clone();
                object.on_live(move |ctx| {
                    let settings_sub = settings_sub.clone();
                    let view_sub = view_sub.clone();
                    let waiting_update = waiting_update.clone();
                    let host_watch = host_watch.clone();
                    Box::pin(async move {
                        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
                        let tx1 = tx.clone();
                        let _s1 = settings_sub.subscribe(
                            move |_, _| {
                                let _ = tx1.send(());
                                Ok(())
                            },
                            |_| {},
                        );
                        let tx2 = tx.clone();
                        let _s2 = view_sub.subscribe(
                            move |_, _| {
                                let _ = tx2.send(());
                                Ok(())
                            },
                            |_| {},
                        );
                        let _ = tx.send(());

                        loop {
                            let recv = ctx
                                .context
                                .await_with(rx.recv())
                                .await
                                .map_err(|e| ChordError::new("aborted", e.to_string()))?;
                            if recv.is_none() {
                                break;
                            }
                            let enabled = settings_sub
                                .value()
                                .ok()
                                .map(|r| parse_settings(&r.to_json()).enabled)
                                .unwrap_or(false);
                            let count = if enabled {
                                let worker = worker_of(&host_watch)?;
                                let durable = worker.harness()?.durable.clone();
                                let view_val = view_sub.value().ok().map(|r| r.to_json());
                                let inbox: Option<InboxState> = view_val.and_then(|jv| {
                                    let v = jv.to_serde().ok()?;
                                    let inbox_val = v.get("docs")?.get(INBOX_DOC.kind.as_str())?;
                                    serde_json::from_value(inbox_val.clone()).ok()
                                });
                                let entries =
                                    recent_entries(&durable, id, &ctx.context, 2000).await?;
                                unmentioned(&entries, inbox.as_ref()).len()
                            } else {
                                0
                            };
                            let prev = waiting_update
                                .value()
                                .ok()
                                .and_then(|r| r.to_json().to_serde().ok())
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            if prev != count as u64 {
                                waiting_update
                                    .set(JsonValue::from_serde(json!(count)), &ctx.context)?;
                            }
                        }
                        Ok(())
                    })
                });

                let enabled_state = Source::derive(
                    object.batch(),
                    vec![settings.clone()],
                    |vals| {
                        let s = vals
                            .first()
                            .map(|r| parse_settings(&r.to_json()).enabled)
                            .unwrap_or(false);
                        pi_rs_chord::delta::Revision::from_json(JsonValue::from_serde(json!(s)))
                            .map_err(|e| Error::Invalid(e.to_string()))
                    },
                    |_| {},
                );
                let name_state = Source::derive(
                    object.batch(),
                    vec![settings.clone()],
                    |vals| {
                        let n = vals
                            .first()
                            .map(|r| parse_settings(&r.to_json()).name)
                            .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string());
                        pi_rs_chord::delta::Revision::from_json(JsonValue::from_serde(json!(n)))
                            .map_err(|e| Error::Invalid(e.to_string()))
                    },
                    |_| {},
                );

                let mut members = Members::new();
                members.state("enabled", enabled_state)?;
                members.state("name", name_state)?;
                members.state("waiting", waiting)?;

                let host_set = host_handle.clone();
                let holder_set = holder.clone();
                members.method("set", move |args, ctx| {
                    let host_set = host_set.clone();
                    let holder_set = holder_set.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let on = v
                            .get("enabled")
                            .and_then(Value::as_bool)
                            .ok_or_else(|| ChordError::invalid("args", "missing enabled", None))?;
                        let worker = worker_of(&host_set)?;
                        let durable = worker.harness()?.durable.clone();
                        let conv = durable
                            .conversation(id, &ctx.context)
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        if conv.is_none() {
                            return Err(ChordError::invalid(
                                "args",
                                format!("Unknown conversation {conversation}"),
                                None,
                            )
                            .into());
                        }
                        let h = *holder_set.lock().unwrap_or_else(|e| e.into_inner());
                        let doc = chat_mode_doc();
                        let addr = resolve_address(&doc, Some(h), None)
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        durable
                            .session
                            .commit(
                                async |tx| {
                                    use pi_rs_chord::delta::Segment;
                                    let change = tx.doc(&doc, &addr, None).await?;
                                    change
                                        .set(&[Segment::Key("enabled".into())], json!(on).into())?;
                                    Ok(())
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let host_rename = host_handle.clone();
                let holder_rename = holder.clone();
                members.method("rename", move |args, ctx| {
                    let host_rename = host_rename.clone();
                    let holder_rename = holder_rename.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let next = v
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|n| is_valid_agent_name(n))
                            .ok_or_else(|| ChordError::invalid("args", "invalid agent name", None))?
                            .to_string();
                        let worker = worker_of(&host_rename)?;
                        let durable = worker.harness()?.durable.clone();
                        let conv = durable
                            .conversation(id, &ctx.context)
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        if conv.is_none() {
                            return Err(ChordError::invalid(
                                "args",
                                format!("Unknown conversation {conversation}"),
                                None,
                            )
                            .into());
                        }
                        let h = *holder_rename.lock().unwrap_or_else(|e| e.into_inner());
                        let doc = chat_mode_doc();
                        let addr = resolve_address(&doc, Some(h), None)
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        durable
                            .session
                            .commit(
                                async |tx| {
                                    use pi_rs_chord::delta::Segment;
                                    let change = tx.doc(&doc, &addr, None).await?;
                                    change
                                        .set(&[Segment::Key("name".into())], json!(next).into())?;
                                    Ok(())
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::Null)
                    })
                })?;

                Ok(members)
            },
            ServeOptions {
                linger: Duration::from_secs(30),
                ..Default::default()
            },
        )?;

        let setting_for_remote = setting_contract.clone();
        host.serve(
            mode_contract.clone(),
            move |params, principal, object| {
                let p = params.to_serde()?;
                let session = p
                    .get("session")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ChordError::invalid("params", "missing session", None))?;
                let conversation = parse_conversation_id(params)?;
                if !can_view(principal, session) {
                    return Err(ChordError::new("forbidden", "viewers only").into());
                }
                let setting = object.get(
                    setting_for_remote.clone(),
                    JsonValue::from_serde(json!({ "conversation": conversation })),
                )?;
                let owner = is_owner(principal);
                let mut members = Members::variant(if owner { "owner" } else { "reader" });
                members.state("enabled", setting.member("enabled")?)?;
                members.state("waiting", setting.member("waiting")?)?;
                members.state("name", setting.member("name")?)?;
                if owner {
                    let s_set = setting.clone();
                    members.method("set", move |args, ctx| {
                        let s_set = s_set.clone();
                        Box::pin(async move { s_set.call("set", args, &ctx.context).await })
                    })?;
                    let s_rename = setting.clone();
                    members.method("rename", move |args, ctx| {
                        let s_rename = s_rename.clone();
                        Box::pin(async move { s_rename.call("rename", args, &ctx.context).await })
                    })?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("chat-mode", FacetKind::Worker, worker_facet()?)
}
