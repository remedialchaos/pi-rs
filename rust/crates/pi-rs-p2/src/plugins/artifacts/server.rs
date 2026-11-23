//! Artifacts (server facet): `p2.artifact-store`, `p2.artifacts`, `p2.artifact-page`, and
//! `/artifact-frame` (GET/HEAD) serving sandboxed html pages with CSP.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::Body;
use http::{HeaderValue, Method, Response, StatusCode, header};
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{Contract, Member},
    data::ChordError,
    host::{
        Ctx, EventChannel, Facet, FacetHost, Host, Members, ObjectSelf, ServeOptions, WeakHost,
    },
    json::JsonValue,
    node::http::{HttpError, HttpRequest, HttpResponse, HttpResult, RouteOptions, text_response},
    state::Source,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::v1::{
        Principal,
        p2_contracts::lookup,
        server::{install_service, server_of, service_of},
    },
    runtime::server::ServerServices,
};

use super::{
    frame::{FRAME_CSP, frame_page},
    pages::{ErrorMark, EvalResult, PageRegistry},
    store::{
        ARTIFACT_STORE_OWNER, ArtifactEdit, ArtifactKind, ArtifactStore, WriteInput,
        artifacts_state_definition, is_valid_artifact_id, states_state_definition, summaries_of,
        summary_of, version_rows_table,
    },
};

/// Native service key exposing the live `ArtifactStore` on the server host.
pub const ARTIFACT_STORE_SERVICE: &str = "p2.artifact-store.service";
/// Native service key exposing the live `PageRegistry` on the server host.
pub const ARTIFACT_PAGES_SERVICE: &str = "p2.artifact-pages";

struct StoreBundle {
    store: Arc<ArtifactStore>,
    pages: Arc<PageRegistry>,
}

fn store_contract() -> Result<Arc<Contract>> {
    let mut members = IndexMap::new();
    let any = JsonValue::from_serde(json!({}));
    let null = JsonValue::from_serde(json!({"type": "null"}));
    for (name, ret) in [
        ("read", any.clone()),
        ("write", any.clone()),
        ("edit", any.clone()),
        ("remove", null.clone()),
        ("copy", null.clone()),
        ("readState", any.clone()),
        ("setState", null),
    ] {
        members.insert(
            name.into(),
            Member::Method {
                args: any.clone(),
                result: ret,
            },
        );
    }
    Ok(Arc::new(Contract::local(
        ARTIFACT_STORE_OWNER,
        1,
        Vec::new(),
        members,
    )?))
}

/// Copies all artifacts, versions, and html page states from session `from` to session `to`.
pub async fn copy_session_artifacts(
    host: &Host,
    from: &str,
    to: &str,
    context: &Context,
) -> Result<()> {
    let held = host.hold(store_contract()?, JsonValue::from_serde(json!({})), context)?;
    held.wait(context).await?;
    held.call(
        "copy",
        JsonValue::from_serde(json!({ "from": from, "to": to })),
        context,
    )
    .await?;
    Ok(())
}

/// Builds the server facet for the `artifacts` plugin.
pub fn server_facet() -> Result<Facet> {
    let store_c = store_contract()?;
    let artifacts_c = Arc::new(lookup("p2.artifacts")?);
    let page_c = Arc::new(lookup("p2.artifact-page")?);
    let host_slot: Arc<Mutex<Option<WeakHost>>> = Arc::new(Mutex::new(None));

    let mut services = ServerServices::default();
    let get_slot = Arc::clone(&host_slot);
    let mut get_opts = RouteOptions::new(move |req| {
        let slot = Arc::clone(&get_slot);
        Box::pin(async move { handle_artifact_frame(req, slot).await })
    });
    get_opts.public = true;
    let head_slot = Arc::clone(&host_slot);
    let mut head_opts = RouteOptions::new(move |req| {
        let slot = Arc::clone(&head_slot);
        Box::pin(async move { handle_artifact_frame(req, slot).await })
    });
    head_opts.public = true;
    services
        .route("GET /artifact-frame", get_opts)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    services
        .route("HEAD /artifact-frame", head_opts)
        .map_err(|e| Error::Invalid(e.to_string()))?;

    Ok(services.with_facet(move |host: &mut FacetHost| {
        *host_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(host.host().downgrade());
        let store_c = Arc::clone(&store_c);
        let artifacts_c = Arc::clone(&artifacts_c);
        let page_c = Arc::clone(&page_c);

        host.serve(
            Arc::clone(&store_c),
            |_params, _principal, object| store_host_members(object),
            ServeOptions::default(),
        )?;

        let store_dep = Arc::clone(&store_c);
        host.serve(
            artifacts_c,
            move |params, principal, object| {
                artifacts_members(params, principal, object, Arc::clone(&store_dep))
            },
            ServeOptions::default(),
        )?;

        let store_dep = Arc::clone(&store_c);
        host.serve(
            page_c,
            move |params, principal, object| {
                artifact_page_members(params, principal, object, Arc::clone(&store_dep))
            },
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}

fn store_host_members(object: &mut ObjectSelf) -> Result<Members> {
    let artifacts_doc = object.state(artifacts_state_definition()?)?;
    let states_doc = object.state(states_state_definition()?)?;
    let table_def = version_rows_table()?;
    object.table(table_def.clone())?;
    let host_handle = object.host();
    let engine = object
        .store()
        .cloned()
        .ok_or_else(|| Error::Invalid("artifacts requires a persistent server store".into()))?;

    let bundle = object.open(
        move |_ctx: Ctx| {
            Box::pin(async move {
                let store = Arc::new(ArtifactStore::new(
                    artifacts_doc,
                    states_doc,
                    table_def,
                    engine,
                )?);
                let pages = Arc::new(PageRegistry::new());
                install_service(&host_handle, ARTIFACT_STORE_SERVICE, &store)?;
                install_service(&host_handle, ARTIFACT_PAGES_SERVICE, &pages)?;
                Ok(StoreBundle { store, pages })
            })
        },
        |_bundle: Arc<StoreBundle>, _reason: String| Box::pin(async { Ok(()) }),
    );

    let mut members = Members::new();
    {
        let bundle = bundle.clone();
        members.method("read", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    session: String,
                    id: String,
                    #[serde(default)]
                    version: Option<i64>,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let read = bundle
                    .value()?
                    .store
                    .read(&parsed.session, &parsed.id, parsed.version, &ctx.context)
                    .await?;
                Ok(JsonValue::from_serde(serde_json::to_value(read)?))
            })
        })?;
    }
    {
        let bundle = bundle.clone();
        members.method("write", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    session: String,
                    #[serde(flatten)]
                    input: WriteInput,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let (id, version) = bundle
                    .value()?
                    .store
                    .write(&parsed.session, parsed.input, now_ms(), &ctx.context)
                    .await?;
                Ok(JsonValue::from_serde(
                    json!({ "id": id, "version": version }),
                ))
            })
        })?;
    }
    {
        let bundle = bundle.clone();
        members.method("edit", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    session: String,
                    id: String,
                    edits: Vec<ArtifactEdit>,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let version = bundle
                    .value()?
                    .store
                    .edit(
                        &parsed.session,
                        &parsed.id,
                        &parsed.edits,
                        now_ms(),
                        &ctx.context,
                    )
                    .await?;
                Ok(JsonValue::from_serde(json!({ "version": version })))
            })
        })?;
    }
    {
        let bundle = bundle.clone();
        members.method("remove", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    session: String,
                    id: String,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let inner = bundle.value()?;
                inner
                    .store
                    .remove(&parsed.session, &parsed.id, &ctx.context)
                    .await?;
                inner.pages.forget(&parsed.session, &parsed.id);
                Ok(JsonValue::Null)
            })
        })?;
    }
    {
        let bundle = bundle.clone();
        members.method("copy", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    from: String,
                    to: String,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                bundle
                    .value()?
                    .store
                    .copy(&parsed.from, &parsed.to, &ctx.context)
                    .await?;
                Ok(JsonValue::Null)
            })
        })?;
    }
    {
        let bundle = bundle.clone();
        members.method("readState", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    session: String,
                    id: String,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let state = bundle
                    .value()?
                    .store
                    .state(&parsed.session, &parsed.id, &ctx.context)
                    .await?;
                Ok(JsonValue::from_serde(json!({ "state": state })))
            })
        })?;
    }
    {
        members.method("setState", move |args, ctx| {
            let bundle = bundle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    session: String,
                    id: String,
                    state: Value,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                bundle
                    .value()?
                    .store
                    .set_state(&parsed.session, &parsed.id, parsed.state, &ctx.context)
                    .await?;
                Ok(JsonValue::Null)
            })
        })?;
    }

    Ok(members)
}

fn parse_principal(principal: Option<&Value>, refusal: &str) -> Result<Principal> {
    let value = principal.ok_or_else(|| Error::from(ChordError::new("forbidden", refusal)))?;
    serde_json::from_value(value.clone())
        .map_err(|_| Error::from(ChordError::new("forbidden", refusal)))
}

fn artifacts_members(
    params: &JsonValue,
    principal: Option<&Value>,
    object: &mut ObjectSelf,
    store_c: Arc<Contract>,
) -> Result<Members> {
    let params = params.to_serde()?;
    let session = params["session"]
        .as_str()
        .ok_or_else(|| Error::Invalid("missing session param".into()))?
        .to_owned();
    let parsed = parse_principal(principal, "the session's viewers and worker only")?;
    let is_worker = matches!(&parsed, Principal::Worker { session: s, .. } if s == &session);
    if !is_worker && !parsed.can_view(&session) {
        return Err(ChordError::new("forbidden", "the session's viewers and worker only").into());
    }

    let store_held = object.get(store_c, JsonValue::from_serde(json!({})))?;
    let host_handle = object.host();

    let artifacts_source = follow_session_summaries(object, host_handle.clone(), session.clone())?;

    if is_worker {
        let mut members = Members::variant("worker");
        {
            let host_handle = host_handle.clone();
            let session = session.clone();
            members.method("list", move |_args, ctx| {
                let host_handle = host_handle.clone();
                let session = session.clone();
                Box::pin(async move {
                    let store = service_of::<ArtifactStore>(&host_handle, ARTIFACT_STORE_SERVICE)?;
                    let doc = store.artifacts_doc(&session, &ctx.context).await?;
                    Ok(JsonValue::from_serde(serde_json::to_value(summaries_of(
                        &doc,
                    ))?))
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let session = session.clone();
            members.method("read", move |args, ctx| {
                let store_held = store_held.clone();
                let session = session.clone();
                Box::pin(async move {
                    let mut obj = args.to_serde()?;
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("session".into(), json!(session));
                    }
                    ctx.call(&store_held, "read", JsonValue::from_serde(obj))
                        .await
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let session = session.clone();
            members.method("write", move |args, ctx| {
                let store_held = store_held.clone();
                let session = session.clone();
                Box::pin(async move {
                    let mut obj = args.to_serde()?;
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("session".into(), json!(session));
                    }
                    ctx.call(&store_held, "write", JsonValue::from_serde(obj))
                        .await
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let session = session.clone();
            members.method("edit", move |args, ctx| {
                let store_held = store_held.clone();
                let session = session.clone();
                Box::pin(async move {
                    let mut obj = args.to_serde()?;
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("session".into(), json!(session));
                    }
                    ctx.call(&store_held, "edit", JsonValue::from_serde(obj))
                        .await
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let session = session.clone();
            members.method("remove", move |args, ctx| {
                let store_held = store_held.clone();
                let session = session.clone();
                Box::pin(async move {
                    let mut obj = args.to_serde()?;
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("session".into(), json!(session));
                    }
                    ctx.call(&store_held, "remove", JsonValue::from_serde(obj))
                        .await
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let session = session.clone();
            members.method("readState", move |args, ctx| {
                let store_held = store_held.clone();
                let session = session.clone();
                Box::pin(async move {
                    let mut obj = args.to_serde()?;
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("session".into(), json!(session));
                    }
                    ctx.call(&store_held, "readState", JsonValue::from_serde(obj))
                        .await
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let session = session.clone();
            members.method("setState", move |args, ctx| {
                let store_held = store_held.clone();
                let session = session.clone();
                Box::pin(async move {
                    let mut obj = args.to_serde()?;
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("session".into(), json!(session));
                    }
                    ctx.call(&store_held, "setState", JsonValue::from_serde(obj))
                        .await
                })
            })?;
        }
        {
            let host_handle = host_handle.clone();
            let session = session.clone();
            members.method("errors", move |args, _ctx| {
                let host_handle = host_handle.clone();
                let session = session.clone();
                Box::pin(async move {
                    let args = args.to_serde()?;
                    let id = args["id"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing artifact id".into()))?;
                    let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                    Ok(JsonValue::from_serde(serde_json::to_value(
                        pages.errors(&session, id),
                    )?))
                })
            })?;
        }
        {
            let host_handle = host_handle.clone();
            let session = session.clone();
            members.method("logs", move |args, _ctx| {
                let host_handle = host_handle.clone();
                let session = session.clone();
                Box::pin(async move {
                    let args = args.to_serde()?;
                    let id = args["id"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing artifact id".into()))?;
                    let since = args.get("since").and_then(Value::as_i64).unwrap_or(0);
                    let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                    let (logs, next) = pages.logs(&session, id, since);
                    Ok(JsonValue::from_serde(json!({ "logs": logs, "next": next })))
                })
            })?;
        }
        {
            let store_held = store_held.clone();
            let host_handle = host_handle.clone();
            let session = session.clone();
            members.method("eval", move |args, ctx| {
                let store_held = store_held.clone();
                let host_handle = host_handle.clone();
                let session = session.clone();
                Box::pin(async move {
                    let args = args.to_serde()?;
                    let id = args["id"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing artifact id".into()))?
                        .to_owned();
                    let code = args["code"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing code".into()))?
                        .to_owned();
                    // Verify artifact exists before dispatching to open pages.
                    ctx.call(
                        &store_held,
                        "read",
                        JsonValue::from_serde(json!({ "session": session, "id": id })),
                    )
                    .await?;
                    let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                    let result = pages.run(&session, &id, &code, &ctx.context).await;
                    Ok(JsonValue::from_serde(serde_json::to_value(result)?))
                })
            })?;
        }
        {
            let host_handle = host_handle.clone();
            let session = session.clone();
            members.method("newErrors", move |_args, ctx| {
                let host_handle = host_handle.clone();
                let session = session.clone();
                Box::pin(async move {
                    let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                    let store = service_of::<ArtifactStore>(&host_handle, ARTIFACT_STORE_SERVICE)?;
                    let doc = store.artifacts_doc(&session, &ctx.context).await?;
                    let (errors, marks) = pages.uncarried(&session);
                    let enriched: Vec<Value> = errors
                        .into_iter()
                        .map(|err| {
                            let title = doc
                                .artifacts
                                .get(&err.id)
                                .map_or(err.id.as_str(), |s| s.title.as_str());
                            json!({ "id": err.id, "title": title, "text": err.text })
                        })
                        .collect();
                    Ok(JsonValue::from_serde(json!({
                        "errors": enriched,
                        "marks": marks,
                    })))
                })
            })?;
        }
        {
            members.method("carried", move |args, _ctx| {
                let host_handle = host_handle.clone();
                let session = session.clone();
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        marks: Vec<ErrorMark>,
                    }
                    let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                    let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                    pages.carried(&session, &parsed.marks);
                    Ok(JsonValue::Null)
                })
            })?;
        }
        return Ok(members);
    }

    let can_edit = parsed.can_send(&session)
        && matches!(parsed, Principal::Owner { .. } | Principal::Guest { .. });
    let mut members = Members::variant(if can_edit { "editor" } else { "reader" });
    members.state("artifacts", artifacts_source)?;
    {
        let store_held = store_held.clone();
        let session = session.clone();
        members.method("read", move |args, ctx| {
            let store_held = store_held.clone();
            let session = session.clone();
            Box::pin(async move {
                let mut obj = args.to_serde()?;
                if let Some(map) = obj.as_object_mut() {
                    map.insert("session".into(), json!(session));
                }
                ctx.call(&store_held, "read", JsonValue::from_serde(obj))
                    .await
            })
        })?;
    }
    if can_edit {
        members.method("remove", move |args, ctx| {
            let store_held = store_held.clone();
            let session = session.clone();
            Box::pin(async move {
                let mut obj = args.to_serde()?;
                if let Some(map) = obj.as_object_mut() {
                    map.insert("session".into(), json!(session));
                }
                ctx.call(&store_held, "remove", JsonValue::from_serde(obj))
                    .await
            })
        })?;
    }
    Ok(members)
}

fn artifact_page_members(
    params: &JsonValue,
    principal: Option<&Value>,
    object: &mut ObjectSelf,
    store_c: Arc<Contract>,
) -> Result<Members> {
    let params = params.to_serde()?;
    let session = params["session"]
        .as_str()
        .ok_or_else(|| Error::Invalid("missing session param".into()))?
        .to_owned();
    let id = params["id"]
        .as_str()
        .ok_or_else(|| Error::Invalid("missing id param".into()))?
        .to_owned();
    let parsed = parse_principal(principal, "the session's viewers only")?;
    if !parsed.can_view(&session) {
        return Err(ChordError::new("forbidden", "the session's viewers only").into());
    }
    if !is_valid_artifact_id(&id) {
        return Err(ChordError::new("forbidden", "no such artifact").into());
    }

    let store_held = object.get(store_c, JsonValue::from_serde(json!({})))?;
    let host_handle = object.host();
    let summary_source =
        follow_artifact_summary(object, host_handle.clone(), session.clone(), id.clone())?;
    let value_source =
        follow_artifact_state(object, host_handle.clone(), session.clone(), id.clone())?;
    let request_channel = EventChannel::new();

    // Open while some browser holds it live: the agent's requests go to it.
    {
        let host_handle = host_handle.clone();
        let session = session.clone();
        let id = id.clone();
        let request_channel = request_channel.clone();
        object.on_live(move |ctx: Ctx| {
            let host_handle = host_handle.clone();
            let session = session.clone();
            let id = id.clone();
            let request_channel = request_channel.clone();
            Box::pin(async move {
                let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                let _guard = pages.open(
                    &session,
                    &id,
                    Arc::new(move |asked| {
                        if let Ok(value) = serde_json::to_value(asked) {
                            request_channel.emit(JsonValue::from_serde(value));
                        }
                    }),
                );
                let _ = ctx.context.await_with(std::future::pending::<()>()).await;
                Ok(())
            })
        });
    }

    let can_edit = parsed.can_send(&session)
        && matches!(parsed, Principal::Owner { .. } | Principal::Guest { .. });
    let mut members = Members::variant(if can_edit { "editor" } else { "viewer" });
    members.state("summary", summary_source)?;
    members.state("value", value_source)?;
    members.event("request", request_channel)?;

    {
        let host_handle = host_handle.clone();
        let session = session.clone();
        let id = id.clone();
        members.method("report", move |args, _ctx| {
            let host_handle = host_handle.clone();
            let session = session.clone();
            let id = id.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct LogInput {
                    level: String,
                    text: String,
                }
                #[derive(Deserialize)]
                struct Args {
                    errors: Vec<String>,
                    logs: Vec<LogInput>,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                let logs: Vec<(String, String)> =
                    parsed.logs.into_iter().map(|l| (l.level, l.text)).collect();
                pages.report(&session, &id, &parsed.errors, &logs);
                Ok(JsonValue::Null)
            })
        })?;
    }
    {
        members.method("answer", move |args, _ctx| {
            let host_handle = host_handle.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    request: String,
                    result: EvalResult,
                }
                let parsed: Args = serde_json::from_value(args.to_serde()?)?;
                let pages = service_of::<PageRegistry>(&host_handle, ARTIFACT_PAGES_SERVICE)?;
                pages.answer(&parsed.request, parsed.result);
                Ok(JsonValue::Null)
            })
        })?;
    }
    if can_edit {
        members.method("setState", move |args, ctx| {
            let store_held = store_held.clone();
            let session = session.clone();
            let id = id.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                ctx.call(
                    &store_held,
                    "setState",
                    JsonValue::from_serde(json!({
                        "session": session,
                        "id": id,
                        "state": args["state"].clone(),
                    })),
                )
                .await
            })
        })?;
    }
    Ok(members)
}

fn follow_session_summaries(
    object: &mut ObjectSelf,
    host: Host,
    session: String,
) -> Result<Source> {
    object.follow(move |target, ctx| {
        Box::pin(async move {
            let store = service_of::<ArtifactStore>(&host, ARTIFACT_STORE_SERVICE)?;
            let mut rx = store.subscribe();
            let initial = summaries_of(&store.artifacts_doc(&session, &ctx.context).await?);
            target.set(
                JsonValue::from_serde(serde_json::to_value(initial)?),
                &ctx.context,
            )?;
            let task = tokio::spawn(async move {
                let bg = Context::background();
                loop {
                    match rx.recv().await {
                        Ok(changed_session) if changed_session == session => {
                            if let Ok(doc) = store.artifacts_doc(&session, &bg).await
                                && let Ok(val) = serde_json::to_value(summaries_of(&doc))
                            {
                                let _ = target.set(JsonValue::from_serde(val), &bg);
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Ok(doc) = store.artifacts_doc(&session, &bg).await
                                && let Ok(val) = serde_json::to_value(summaries_of(&doc))
                            {
                                let _ = target.set(JsonValue::from_serde(val), &bg);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
        })
    })
}

fn follow_artifact_summary(
    object: &mut ObjectSelf,
    host: Host,
    session: String,
    id: String,
) -> Result<Source> {
    object.follow(move |target, ctx| {
        Box::pin(async move {
            let store = service_of::<ArtifactStore>(&host, ARTIFACT_STORE_SERVICE)?;
            let mut rx = store.subscribe();
            let doc = store.artifacts_doc(&session, &ctx.context).await?;
            let initial = doc.artifacts.get(&id).map(|stored| summary_of(&id, stored));
            target.set(
                JsonValue::from_serde(serde_json::to_value(initial)?),
                &ctx.context,
            )?;
            let task = tokio::spawn(async move {
                let bg = Context::background();
                loop {
                    match rx.recv().await {
                        Ok(changed_session) if changed_session == session => {
                            if let Ok(doc) = store.artifacts_doc(&session, &bg).await {
                                let next =
                                    doc.artifacts.get(&id).map(|stored| summary_of(&id, stored));
                                if let Ok(val) = serde_json::to_value(next) {
                                    let _ = target.set(JsonValue::from_serde(val), &bg);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Ok(doc) = store.artifacts_doc(&session, &bg).await {
                                let next =
                                    doc.artifacts.get(&id).map(|stored| summary_of(&id, stored));
                                if let Ok(val) = serde_json::to_value(next) {
                                    let _ = target.set(JsonValue::from_serde(val), &bg);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
        })
    })
}

fn follow_artifact_state(
    object: &mut ObjectSelf,
    host: Host,
    session: String,
    id: String,
) -> Result<Source> {
    object.follow(move |target, ctx| {
        Box::pin(async move {
            let store = service_of::<ArtifactStore>(&host, ARTIFACT_STORE_SERVICE)?;
            let mut rx = store.subscribe();
            let doc = store.states_doc(&session, &ctx.context).await?;
            let initial = doc.states.get(&id).cloned().unwrap_or(Value::Null);
            target.set(JsonValue::from_serde(initial), &ctx.context)?;
            let task = tokio::spawn(async move {
                let bg = Context::background();
                loop {
                    match rx.recv().await {
                        Ok(changed_session) if changed_session == session => {
                            if let Ok(doc) = store.states_doc(&session, &bg).await {
                                let next = doc.states.get(&id).cloned().unwrap_or(Value::Null);
                                let _ = target.set(JsonValue::from_serde(next), &bg);
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Ok(doc) = store.states_doc(&session, &bg).await {
                                let next = doc.states.get(&id).cloned().unwrap_or(Value::Null);
                                let _ = target.set(JsonValue::from_serde(next), &bg);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
        })
    })
}

async fn handle_artifact_frame(
    request: HttpRequest,
    host_slot: Arc<Mutex<Option<WeakHost>>>,
) -> HttpResult<HttpResponse> {
    let query: HashMap<String, String> = form_urlencoded::parse(request.query.as_bytes())
        .into_owned()
        .collect();
    let session = query.get("session").map(String::as_str).unwrap_or("");
    let principal: Option<Principal> = request
        .principal
        .clone()
        .and_then(|v| serde_json::from_value(v).ok());
    let sec_fetch_site = request.header("sec-fetch-site");
    if !principal.as_ref().is_some_and(|p| p.can_view(session))
        || sec_fetch_site == Some("cross-site")
    {
        return text_response(403, "Forbidden");
    }

    let id = query.get("id").map(String::as_str).unwrap_or("");
    let version: i64 = match query.get("version").and_then(|v| v.parse::<i64>().ok()) {
        Some(v) if v >= 1 => v,
        _ => return text_response(404, "Artifact not found"),
    };
    if !is_valid_artifact_id(id) {
        return text_response(404, "Artifact not found");
    }

    let Some(host) = host_slot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(WeakHost::upgrade)
    else {
        return text_response(404, "Artifact not found");
    };
    let natives = server_of(&host).ok();
    let relay_shell_origin = natives
        .as_ref()
        .and_then(|n| n.request_access(&request).ok())
        .and_then(|access| access.origin.relay_shell_origin);

    let ctx = Context::background();
    let root = host.root();

    // If the sessions catalog is served, ensure the session exists.
    if host.served_contract("p2.catalog").is_some()
        && let Ok(catalog_c) = lookup("p2.catalog").map(Arc::new)
    {
        let Ok(catalog) = root.hold(catalog_c, JsonValue::from_serde(json!({}))) else {
            return text_response(404, "Artifact not found");
        };
        if catalog.wait(&ctx).await.is_err() {
            return text_response(404, "Artifact not found");
        }
        if catalog
            .call("get", JsonValue::from_serde(json!({ "id": session })), &ctx)
            .await
            .is_err()
        {
            return text_response(404, "Artifact not found");
        }
    }

    let Ok(store_c) = store_contract() else {
        return text_response(404, "Artifact not found");
    };
    let Ok(store_held) = root.hold(store_c, JsonValue::from_serde(json!({}))) else {
        return text_response(404, "Artifact not found");
    };
    if store_held.wait(&ctx).await.is_err() {
        return text_response(404, "Artifact not found");
    }
    let Ok(read_val) = store_held
        .call(
            "read",
            JsonValue::from_serde(json!({ "session": session, "id": id, "version": version })),
            &ctx,
        )
        .await
    else {
        return text_response(404, "Artifact not found");
    };
    let Ok(read_json) = read_val.to_serde() else {
        return text_response(404, "Artifact not found");
    };
    let Ok(read) = serde_json::from_value::<super::store::ArtifactRead>(read_json) else {
        return text_response(404, "Artifact not found");
    };
    if read.summary.kind != ArtifactKind::Html {
        return text_response(404, "Artifact not found");
    }

    let html = frame_page(&read.content);
    let body_bytes = html.into_bytes();
    let csp = match relay_shell_origin {
        Some(origin) => format!("{FRAME_CSP}; frame-ancestors 'self' {origin}"),
        None => format!("{FRAME_CSP}; frame-ancestors 'self'"),
    };
    let body = if request.method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(body_bytes.clone())
    };
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_str(&csp)?,
        )
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )
        .header(header::CONTENT_LENGTH, body_bytes.len().to_string())
        .header(
            "Cross-Origin-Resource-Policy",
            HeaderValue::from_static("same-origin"),
        )
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))
        .body(body)
        .map_err(|e| HttpError::new(500, e.to_string()))?;
    Ok(response)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
