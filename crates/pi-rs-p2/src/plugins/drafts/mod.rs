//! `drafts` built-in plugin: stores withdrawn inputs per `<conversation>:<authorId>` for the
//! worker's lifetime so the author's first claiming client recovers them atomically once.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    watch::WatchCall,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup},
    runtime::principals::OwnerAccess,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub kind: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct AddArgs {
    conversation: u64,
    items: Vec<AddItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct AddItem {
    author: Author,
    text: String,
}

#[derive(Clone, Debug, Deserialize)]
struct StoreClaimArgs {
    conversation: u64,
    #[serde(rename = "authorId")]
    author_id: String,
    ids: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct RemoteClaimArgs {
    ids: Vec<u64>,
}

#[derive(Default)]
struct StoreState {
    drafts: IndexMap<String, Vec<Draft>>,
    next: u64,
}

impl StoreState {
    fn new() -> Self {
        Self {
            drafts: IndexMap::new(),
            next: 1,
        }
    }

    fn to_json(&self) -> Result<JsonValue> {
        Ok(JsonValue::from_serde(serde_json::to_value(&self.drafts)?))
    }
}

pub fn draft_key(conversation: u64, author_id: &str) -> String {
    format!("{conversation}:{author_id}")
}

fn author_id_of(principal: &Principal) -> Option<String> {
    match principal {
        Principal::Owner {
            access: OwnerAccess::Local | OwnerAccess::Relay | OwnerAccess::Device { .. },
        } => Some("owner".into()),
        Principal::Guest { id, .. } => Some(id.clone()),
        _ => None,
    }
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let draft_store_contract = Arc::new(lookup("p2.draft-store")?);
    let drafts_contract = Arc::new(lookup("p2.drafts")?);

    registry.register(
        "drafts",
        FacetKind::Worker,
        define_facet(move |host| {
            let store_for_remote = draft_store_contract.clone();
            host.serve(
                draft_store_contract.clone(),
                move |_, _, object| {
                    let state = Arc::new(Mutex::new(StoreState::new()));
                    let drafts_source = object.writable(JsonValue::from_serde(json!({})))?;
                    let mut members = Members::new();
                    members.state("drafts", drafts_source.clone())?;

                    let add_state = state.clone();
                    let add_source = drafts_source.clone();
                    members.method("add", move |args, ctx| {
                        let add_state = add_state.clone();
                        let add_source = add_source.clone();
                        Box::pin(async move {
                            let args: AddArgs = serde_json::from_value(args.to_serde()?)?;
                            if args.items.is_empty() {
                                return Ok(JsonValue::Null);
                            }
                            let snapshot = {
                                let mut guard = add_state
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner());
                                let first = guard.next;
                                guard.next = guard.next.saturating_add(args.items.len() as u64);
                                for (index, item) in args.items.into_iter().enumerate() {
                                    let key = draft_key(args.conversation, &item.author.id);
                                    guard.drafts.entry(key).or_default().push(Draft {
                                        id: first.saturating_add(index as u64),
                                        text: item.text,
                                    });
                                }
                                guard.to_json()?
                            };
                            add_source.set(snapshot, &ctx.context)?;
                            Ok(JsonValue::Null)
                        })
                    })?;

                    let claim_state = state.clone();
                    let claim_source = drafts_source.clone();
                    members.method("claim", move |args, ctx| {
                        let claim_state = claim_state.clone();
                        let claim_source = claim_source.clone();
                        Box::pin(async move {
                            let args: StoreClaimArgs = serde_json::from_value(args.to_serde()?)?;
                            let key = draft_key(args.conversation, &args.author_id);
                            let wanted: BTreeSet<u64> = args.ids.into_iter().collect();
                            let (texts, changed) = {
                                let mut guard = claim_state
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner());
                                let Some(items) = guard.drafts.get(&key).cloned() else {
                                    return Ok(JsonValue::from_serde(json!({ "texts": [] })));
                                };
                                let claimed: Vec<String> = items
                                    .iter()
                                    .filter(|item| wanted.contains(&item.id))
                                    .map(|item| item.text.clone())
                                    .collect();
                                if claimed.is_empty() {
                                    (claimed, None)
                                } else {
                                    let rest: Vec<Draft> = items
                                        .into_iter()
                                        .filter(|item| !wanted.contains(&item.id))
                                        .collect();
                                    if rest.is_empty() {
                                        guard.drafts.shift_remove(&key);
                                    } else {
                                        guard.drafts.insert(key, rest);
                                    }
                                    (claimed, Some(guard.to_json()?))
                                }
                            };
                            if let Some(snapshot) = changed {
                                claim_source.set(snapshot, &ctx.context)?;
                            }
                            Ok(JsonValue::from_serde(json!({ "texts": texts })))
                        })
                    })?;

                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            host.serve(
                drafts_contract.clone(),
                move |params, principal, object| {
                    let params = params.to_serde()?;
                    let session = params["session"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing session param".into()))?;
                    let conversation = params["conversation"]
                        .as_u64()
                        .ok_or_else(|| Error::Invalid("missing conversation param".into()))?;
                    let parsed = principal
                        .and_then(|value| Principal::deserialize(value).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "participants only"))?;
                    if !parsed.can_send(session) {
                        return Err(ChordError::new("forbidden", "participants only").into());
                    }
                    let Some(author_id) = author_id_of(&parsed) else {
                        return Err(ChordError::new("forbidden", "people only").into());
                    };
                    let store =
                        object.get(store_for_remote.clone(), JsonValue::from_serde(json!({})))?;
                    let key = draft_key(conversation, &author_id);

                    let store_for_follow = store.clone();
                    let key_for_follow = key.clone();
                    let derived = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let source = store_for_follow.member("drafts")?;
                            let extract = {
                                let key = key_for_follow.clone();
                                move |value: JsonValue| -> Result<JsonValue> {
                                    let map = value.to_serde()?;
                                    let items =
                                        map.get(&key).cloned().unwrap_or(Value::Array(Vec::new()));
                                    Ok(JsonValue::from_serde(items))
                                }
                            };
                            let initial = extract(source.value()?.to_json())?;
                            target.set(initial, &ctx.context)?;
                            let changed = target.clone();
                            let ended = target.clone();
                            let subscription = source.subscribe(
                                move |call, context| match call {
                                    WatchCall::Value { value, .. } => {
                                        changed.set(extract(value)?, context)
                                    }
                                    WatchCall::Failed { error } => changed.fail(error, context),
                                },
                                move |_| ended.detach("ended"),
                            );
                            Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let mut members = Members::variant("author");
                    members.state("drafts", derived)?;
                    let claim_store = store.clone();
                    let claim_author = author_id.clone();
                    members.method("claim", move |args, ctx| {
                        let claim_store = claim_store.clone();
                        let claim_author = claim_author.clone();
                        Box::pin(async move {
                            let args: RemoteClaimArgs = serde_json::from_value(args.to_serde()?)?;
                            claim_store
                                .call(
                                    "claim",
                                    JsonValue::from_serde(json!({
                                        "conversation": conversation,
                                        "authorId": claim_author,
                                        "ids": args.ids,
                                    })),
                                    &ctx.context,
                                )
                                .await
                        })
                    })?;

                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
