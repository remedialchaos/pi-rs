//! Views plugin: shared side panes, annotations, `view_*` code-mode tools, and send router.

pub mod contract;
pub mod doc;
pub mod layout;
pub mod tool;

use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    delta::Revision,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    watch::WatchCall,
};
use pi_rs_durable::{
    documents::resolve_address,
    session::{Error as DurableError, Observer},
};
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup, worker::worker_of},
    plugins::chat_mode::{SendRouterInput, get_or_install_send_routers},
    runtime::principals::Principal,
};

use contract::{
    AnnotationInput, MAX_ANNOTATIONS, SelectInlineInput, Selection, VIEW_INPUT, ViewsState,
    get_or_install_message_contexts, is_valid_view_id, is_valid_view_kind, live_views,
    views_contract,
};
use doc::{
    ViewSpec, add_annotation, free_view_id, mark_sent, open_view, read_views, remove_view,
    select_inline, update_views_doc, view_context, views_doc,
};
use tool::create_views_extension;

const MAX_SELECTION_DATA: usize = 16 * 1024;
const DEFAULT_OWNER_NAME: &str = "Owner";

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn parse_principal(principal: Option<&Value>) -> Option<Principal> {
    principal.and_then(|v| serde_json::from_value::<Principal>(v.clone()).ok())
}

fn author_of(principal: &Principal, owner_name: &str) -> Value {
    match principal {
        Principal::Owner { .. } => json!({
            "kind": "owner",
            "id": "owner",
            "name": owner_name,
        }),
        Principal::Guest { id, name, .. } => json!({
            "kind": "guest",
            "id": id,
            "name": name,
        }),
        _ => json!({
            "kind": "owner",
            "id": "owner",
            "name": owner_name,
        }),
    }
}

fn check_selection_size(selection: Option<&Selection>) -> Result<()> {
    let data = selection.and_then(|s| s.data.as_ref());
    let len = serde_json::to_string(&data)?.len();
    if len > MAX_SELECTION_DATA {
        return Err(ChordError::invalid("args", "Selection data is too large", None).into());
    }
    Ok(())
}

pub fn worker_facet() -> Result<Facet> {
    let views_c = views_contract()?;
    let conv_c = Arc::new(lookup("p2.conversation")?);
    let att_c = Arc::new(lookup("p2.attachments")?);

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let worker = worker_of(&host_handle)?;
        worker
            .registries
            .extensions
            .install(create_views_extension())?;

        let contexts = get_or_install_message_contexts(&host_handle)?;
        let routers = get_or_install_send_routers(&host_handle)?;

        let host_r = host_handle.clone();
        let conv_r = conv_c.clone();
        let att_r = att_c.clone();
        let contexts_r = contexts.clone();
        routers.contribute(
            "views",
            Arc::new(move |input: SendRouterInput, ctx| {
                let host_r = host_r.clone();
                let conv_r = conv_r.clone();
                let att_r = att_r.clone();
                let contexts_r = contexts_r.clone();
                Box::pin(async move {
                    let worker = worker_of(&host_r)?;
                    let durable = worker.harness()?.durable.clone();
                    let state = read_views(&durable.session, &ctx.context).await?;
                    let carried = view_context(&state);

                    let mut others = Vec::new();
                    for provider in contexts_r.providers() {
                        if let Some(added) = provider(input.clone(), ctx.clone()).await? {
                            others.push(added);
                        }
                    }

                    if carried.is_none() && others.is_empty() {
                        return Ok(None);
                    }

                    let files: Vec<Value> = if input.attachments.is_empty() {
                        Vec::new()
                    } else {
                        let held = ctx.get(att_r, JsonValue::Object(Default::default()))?;
                        let mut out = Vec::new();
                        for chunk in input.attachments.chunks(20) {
                            let res = held
                                .call(
                                    "resolve",
                                    JsonValue::from_serde(json!({
                                        "ids": chunk,
                                        "conversation": input.conversation,
                                    })),
                                    &ctx.context,
                                )
                                .await?;
                            let resolved: Vec<Value> = serde_json::from_value(res.to_serde()?)?;
                            out.extend(resolved);
                        }
                        out
                    };

                    let att_refs: Vec<Value> = files
                        .iter()
                        .map(|f| json!({ "id": f["id"], "name": f["name"] }))
                        .collect();
                    let selections_count = carried
                        .as_ref()
                        .map_or(0, |c| c.selections.len() + c.inline.len());
                    let annotations_count = carried.as_ref().map_or(0, |c| c.annotations.len());
                    let mut views_meta = json!({
                        "selections": selections_count,
                        "annotations": annotations_count,
                    });
                    if !others.is_empty() {
                        views_meta["notes"] =
                            json!(others.iter().map(|o| o.note.as_str()).collect::<Vec<_>>());
                    }
                    let data = json!({
                        "author": input.author,
                        "text": input.text,
                        "attachments": att_refs,
                        "views": views_meta,
                    });

                    let mut blocks = Vec::new();
                    if let Some(c) = &carried {
                        blocks.push(c.text.as_str());
                    }
                    for o in &others {
                        blocks.push(o.text.as_str());
                    }
                    let blocks_str = blocks.join("\n\n");
                    let full_text = if input.text.trim().is_empty() {
                        blocks_str
                    } else {
                        format!("{}\n\n{blocks_str}", input.text)
                    };

                    let chat = ctx.get(
                        conv_r,
                        JsonValue::from_serde(json!({ "conversation": input.conversation })),
                    )?;
                    let mut submit_args = json!({
                        "text": full_text,
                        "requestId": input.request_id,
                        "author": input.author,
                        "files": files,
                        "entry": { "kind": VIEW_INPUT, "data": data },
                    });
                    if let Some(wb) = input.when_busy {
                        submit_args["whenBusy"] = json!(wb);
                    }
                    let submitted = chat
                        .call("submit", JsonValue::from_serde(submit_args), &ctx.context)
                        .await?;

                    if let Some(carried) = carried {
                        durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        mark_sent(draft, &carried);
                                        Ok(())
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                    }
                    for other in others {
                        (other.carried)(ctx.clone()).await?;
                    }
                    Ok(Some(submitted))
                })
            }),
        );

        let contexts_keepalive = contexts.clone();
        let routers_keepalive = routers.clone();
        host.serve(
            views_c.clone(),
            move |params, principal, object| {
                let _k1 = &contexts_keepalive;
                let _k2 = &routers_keepalive;
                let p = params.to_serde()?;
                let session = p
                    .get("session")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ChordError::invalid("params", "missing session", None))?;
                let parsed = parse_principal(principal);
                let Some(person) = parsed.filter(|pr| pr.can_view(session)) else {
                    return Err(ChordError::new("forbidden", "viewers only").into());
                };

                let host_follow = object.host();
                let index = object.follow(move |target, ctx| {
                    Box::pin(async move {
                        let worker = worker_of(&host_follow)?;
                        let durable = worker.harness()?.durable.clone();
                        let doc = views_doc();
                        let addr = resolve_address(&doc, None, None)
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
                                        Error::Invalid("failed to observe views doc".into())
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

                let derive_state = |f: fn(&ViewsState) -> Value| {
                    Source::derive(
                        object.batch(),
                        vec![index.clone()],
                        move |vals| {
                            let state = vals
                                .first()
                                .and_then(|r| r.to_json().to_serde().ok())
                                .and_then(|v| serde_json::from_value::<ViewsState>(v).ok())
                                .unwrap_or_default();
                            Revision::from_json(JsonValue::from_serde(f(&state)))
                                .map_err(|e| Error::Invalid(e.to_string()))
                        },
                        |_| {},
                    )
                };

                let views_state =
                    derive_state(|s| serde_json::to_value(live_views(s)).unwrap_or(json!([])));
                let annotations_state =
                    derive_state(|s| serde_json::to_value(&s.annotations).unwrap_or(json!([])));
                let arrangement_state =
                    derive_state(|s| serde_json::to_value(&s.arrangement).unwrap_or(Value::Null));
                let inline_state = derive_state(|s| {
                    serde_json::to_value(s.inline.as_deref().unwrap_or(&[])).unwrap_or(json!([]))
                });

                let can_edit = person.can_send(session)
                    && matches!(person, Principal::Owner { .. } | Principal::Guest { .. });

                if !can_edit {
                    let mut members = Members::variant("reader");
                    members.state("views", views_state)?;
                    members.state("annotations", annotations_state)?;
                    members.state("arrangement", arrangement_state)?;
                    members.state("inline", inline_state)?;
                    return Ok(members);
                }

                let mut members = Members::variant("editor");
                members.state("views", views_state)?;
                members.state("annotations", annotations_state)?;
                members.state("arrangement", arrangement_state)?;
                members.state("inline", inline_state)?;

                let host_open = object.host();
                members.method("open", move |args, ctx| {
                    let host_open = host_open.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let kind = v
                            .get("kind")
                            .and_then(Value::as_str)
                            .filter(|k| is_valid_view_kind(k))
                            .ok_or_else(|| ChordError::invalid("args", "invalid kind", None))?
                            .to_string();
                        let item = v.get("item").and_then(Value::as_str).map(str::to_string);
                        let given = v.get("title").and_then(Value::as_str).map(str::to_string);
                        if kind == "markdown" && item.is_none() {
                            return Err(ChordError::invalid(
                                "args",
                                "A markdown view needs a file",
                                None,
                            )
                            .into());
                        }
                        let raw_title = given.unwrap_or_else(|| {
                            if kind == "changes" {
                                match &item {
                                    Some(it) => format!("Changes in {it}"),
                                    None => "Changes".to_string(),
                                }
                            } else {
                                item.clone().unwrap_or_else(|| kind.clone())
                            }
                        });
                        let title: String = raw_title.chars().take(120).collect();

                        let worker = worker_of(&host_open)?;
                        let durable = worker.harness()?.durable.clone();
                        let state = read_views(&durable.session, &ctx.context).await?;
                        let id = free_view_id(&state, &title, &kind);
                        let now = now_millis();

                        let view = durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        open_view(
                                            draft,
                                            ViewSpec {
                                                id,
                                                kind,
                                                title,
                                                item,
                                            },
                                            now,
                                        )
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::from_serde(json!({ "id": view.id })))
                    })
                })?;

                let host_close = object.host();
                members.method("close", move |args, ctx| {
                    let host_close = host_close.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let id = v
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|s| is_valid_view_id(s))
                            .ok_or_else(|| ChordError::invalid("args", "invalid id", None))?
                            .to_string();
                        let worker = worker_of(&host_close)?;
                        let durable = worker.harness()?.durable.clone();
                        durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        remove_view(draft, &id);
                                        Ok(())
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let host_select = object.host();
                members.method("select", move |args, ctx| {
                    let host_select = host_select.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let id = v
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|s| is_valid_view_id(s))
                            .ok_or_else(|| ChordError::invalid("args", "invalid id", None))?
                            .to_string();
                        let selection: Option<Selection> = serde_json::from_value(
                            v.get("selection").cloned().unwrap_or(Value::Null),
                        )?;
                        check_selection_size(selection.as_ref())?;

                        let worker = worker_of(&host_select)?;
                        let durable = worker.harness()?.durable.clone();
                        durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        let Some(view) =
                                            draft.views.iter_mut().find(|each| each.id == id)
                                        else {
                                            return Err(ChordError::invalid(
                                                "args",
                                                format!("No view \"{id}\""),
                                                None,
                                            )
                                            .into());
                                        };
                                        view.selection = selection.map(|s| Selection {
                                            summary: s.summary,
                                            data: s.data,
                                            sent: None,
                                        });
                                        Ok(())
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let host_inline = object.host();
                members.method("selectInline", move |args, ctx| {
                    let host_inline = host_inline.clone();
                    Box::pin(async move {
                        let input: SelectInlineInput = serde_json::from_value(args.to_serde()?)?;
                        check_selection_size(input.selection.as_ref())?;
                        let worker = worker_of(&host_inline)?;
                        let durable = worker.harness()?.durable.clone();
                        durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        select_inline(draft, input);
                                        Ok(())
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let host_ann = object.host();
                let person_ann = person.clone();
                members.method("annotate", move |args, ctx| {
                    let host_ann = host_ann.clone();
                    let person_ann = person_ann.clone();
                    Box::pin(async move {
                        let input: AnnotationInput = serde_json::from_value(args.to_serde()?)?;
                        let worker = worker_of(&host_ann)?;
                        let owner_name = worker
                            .registries
                            .settings
                            .borrow()
                            .get("ownerName")
                            .and_then(Value::as_str)
                            .unwrap_or(DEFAULT_OWNER_NAME)
                            .to_string();
                        let author = author_of(&person_ann, &owner_name);
                        let author_name = author["name"]
                            .as_str()
                            .unwrap_or(DEFAULT_OWNER_NAME)
                            .to_string();
                        let now = now_millis();
                        let durable = worker.harness()?.durable.clone();
                        let id = durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        add_annotation(draft, input, &author_name, now)
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::from_serde(json!({ "id": id })))
                    })
                })?;

                let host_unann = object.host();
                members.method("unannotate", move |args, ctx| {
                    let host_unann = host_unann.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let ids: Option<HashSet<String>> =
                            v.get("ids").and_then(Value::as_array).map(|arr| {
                                arr.iter()
                                    .take(MAX_ANNOTATIONS)
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            });
                        let worker = worker_of(&host_unann)?;
                        let durable = worker.harness()?.durable.clone();
                        durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        match ids {
                                            Some(remove) => {
                                                draft
                                                    .annotations
                                                    .retain(|each| !remove.contains(&each.id));
                                            }
                                            None => draft.annotations.clear(),
                                        }
                                        Ok(())
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
                                },
                                &ctx.context,
                            )
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let host_discard = object.host();
                members.method("discard", move |_, ctx| {
                    let host_discard = host_discard.clone();
                    Box::pin(async move {
                        let worker = worker_of(&host_discard)?;
                        let durable = worker.harness()?.durable.clone();
                        durable
                            .session
                            .commit(
                                async move |tx| {
                                    update_views_doc(tx, |draft| {
                                        draft.annotations.retain(|each| each.sent == Some(true));
                                        for view in &mut draft.views {
                                            if let Some(sel) = &mut view.selection {
                                                sel.sent = Some(true);
                                            }
                                        }
                                        if let Some(inline_list) = &mut draft.inline {
                                            for each in inline_list {
                                                each.selection.sent = Some(true);
                                            }
                                        }
                                        Ok(())
                                    })
                                    .await
                                    .map_err(|e| DurableError::invalid(e.to_string()))
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
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("views", FacetKind::Worker, worker_facet()?)
}
