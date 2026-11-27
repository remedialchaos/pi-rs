use std::{sync::Arc, time::Duration};

use indexmap::IndexMap;
use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    host::{Facet, Held, Members, ServeOptions, define_facet},
    json::JsonValue,
    remote::{RemotePeer, RemoteReference},
    state::Source,
    watch::WatchCall,
};
use pi_rs_durable::{
    EntryDraft, EntryHead, EntryQuery, ROOT_CONVERSATION_ID,
    harness::{Extension, Harness, ToolRegistration, define_extension},
    session::CommitOptions,
    tools::ToolResult,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::v1::{
        Principal,
        node::{host_files, session_directory},
        p2_contracts::lookup,
        server::service_of,
        worker::{
            durable::{LockedTool, ToolAccess},
            worker_of,
        },
    },
    runtime::session_lock::{SessionLock, session_lock, set_session_lock},
};

use super::{arg, clean_title, copy, wire};

enum ServerContractRef {
    Local(Held),
    Remote(RemoteReference),
}

impl ServerContractRef {
    fn hold(
        host: &pi_rs_chord::host::Host,
        contract: Arc<Contract>,
        params: JsonValue,
        ctx: &Context,
    ) -> Result<Option<Self>> {
        if let Ok(peer) = service_of::<RemotePeer>(host, "p2.server-peer") {
            return Ok(Some(Self::Remote(peer.hold(contract, params, ctx)?)));
        }
        if host.served_contract(contract.name()).is_some() {
            return Ok(Some(Self::Local(host.hold(contract, params, ctx)?)));
        }
        Ok(None)
    }

    async fn wait(&self, ctx: &Context) -> Result<()> {
        match self {
            Self::Local(held) => held.wait(ctx).await,
            Self::Remote(remote) => remote.wait(ctx).await,
        }
    }

    fn member(&self, name: &str) -> Result<Source> {
        match self {
            Self::Local(held) => held.member(name),
            Self::Remote(remote) => remote.member(name),
        }
    }

    async fn call(&self, method: &str, args: JsonValue, ctx: &Context) -> Result<JsonValue> {
        match self {
            Self::Local(held) => held.call(method, args, ctx).await,
            Self::Remote(remote) => remote.call(method, args, ctx).await,
        }
    }

    fn release(self) {
        match self {
            Self::Local(held) => held.release(),
            Self::Remote(remote) => remote.release(),
        }
    }
}

pub const MAX_TITLE: usize = 100;

/// Commits `lock` to durable `p2.session-lock` unless it already matches.
pub async fn mirror_lock(
    durable: &Harness,
    lock: Option<&SessionLock>,
    context: &Context,
) -> Result<()> {
    let current = session_lock(&durable.session, context)
        .await
        .map_err(durable_error)?;
    if current.as_ref() == lock {
        return Ok(());
    }
    set_session_lock(&durable.session, lock, context)
        .await
        .map_err(durable_error)
}

fn create_session_title_extension(
    host: pi_rs_chord::host::Host,
    sessions_contract: Arc<Contract>,
) -> Extension {
    let tool = ToolRegistration {
        declaration: Tool {
            name: "rename_session".into(),
            description: format!(
                "Rename the current session: its title in the session list, which is how the user finds it again.\n\
Keep the title naming the overall task of the session. Call this whenever that overall task changes: the user moves on to a different piece of work, or the work turns out to be something other than what the title says. Also call it when the current title does not describe the task, for example when it is just the start of the first message.\n\
Do not rename for sub-steps, follow-up questions, fixes or progress within the same task. Example: \"Migrate pi-mobile to the monorepo\" stays while you fix its build and push it; it becomes \"Fix the terminal on Android\" when the user turns to the terminal.\n\
The title: a short noun or verb phrase of 3 to 8 words, at most {MAX_TITLE} characters, in the user's language, no trailing period, no quotes."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "minLength": 1, "maxLength": MAX_TITLE }
                },
                "required": ["title"],
                "additionalProperties": true
            }),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: None,
        deferred: false,
        execute: Arc::new(move |args, _, ctx| {
            let host = host.clone();
            let sessions_contract = sessions_contract.clone();
            Box::pin(async move {
                #[derive(Deserialize)]
                struct Args {
                    title: String,
                }
                let parsed: Args = serde_json::from_value(args)?;
                let title = clean_title(&parsed.title);
                if title.is_empty() {
                    return Ok(ToolResult {
                        output: Some(vec![InputContent::Text("The title is empty.".into())]),
                        is_error: Some(true),
                        ..Default::default()
                    }
                    .into());
                }
                let run = async {
                    let Some(held) = ServerContractRef::hold(
                        &host,
                        sessions_contract,
                        JsonValue::from_serde(json!({})),
                        ctx,
                    )
                    .map_err(durable_error)?
                    else {
                        return Err(pi_rs_durable::session::Error::Invalid(
                            "p2.sessions server view is unavailable".into(),
                        ));
                    };
                    let waited = held.wait(ctx).await.map_err(durable_error);
                    let res = match waited {
                        Ok(()) => held
                            .call(
                                "renameSelf",
                                JsonValue::from_serde(json!({ "title": title })),
                                ctx,
                            )
                            .await
                            .map_err(durable_error),
                        Err(err) => Err(err),
                    };
                    held.release();
                    let res = res?.to_serde().map_err(durable_error)?;
                    let previous = res["previous"].as_str().unwrap_or_default().to_owned();
                    let msg = if previous == title {
                        format!(
                            "Session title: {}.",
                            serde_json::to_string(&title).unwrap_or_else(|_| title.clone())
                        )
                    } else {
                        format!(
                            "Renamed the session from {} to {}.",
                            serde_json::to_string(&previous).unwrap_or(previous),
                            serde_json::to_string(&title).unwrap_or_else(|_| title.clone())
                        )
                    };
                    Ok::<_, pi_rs_durable::session::Error>(msg)
                }
                .await;
                match run {
                    Ok(text) => Ok(ToolResult {
                        output: Some(vec![InputContent::Text(text.into())]),
                        ..Default::default()
                    }
                    .into()),
                    Err(err) => {
                        if ctx.is_aborted() {
                            return Err(err);
                        }
                        Ok(ToolResult {
                            output: Some(vec![InputContent::Text(err.to_string().into())]),
                            is_error: Some(true),
                            ..Default::default()
                        }
                        .into())
                    }
                }
            })
        }),
    };

    define_extension(Extension {
        name: "p2-session-title".into(),
        tools: vec![tool],
        ..Default::default()
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    conversation: u64,
    before: u64,
}

pub(super) fn facet() -> Result<Facet> {
    let transfer_contract = Arc::new(lookup("p2.session-transfer")?);
    let sessions_contract = Arc::new(lookup("p2.sessions")?);
    let session_lock_contract = Arc::new(lookup("p2.session-lock")?);
    let lock_mirror_contract = Arc::new(Contract::local(
        "p2.session-lock-mirror",
        1,
        Vec::new(),
        IndexMap::new(),
    )?);
    let session_tools_contract = Arc::new(Contract::local(
        "p2.session-tools",
        1,
        Vec::new(),
        IndexMap::new(),
    )?);
    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let worker = worker_of(&host_handle)?;
        let id = worker.init.session()?.id;

        // Serve lock mirror: follows server `p2.session-lock` and mirrors into durable `SessionLockDoc`.
        {
            let lock_c = session_lock_contract.clone();
            let mirror_worker = worker.clone();
            host.serve(
                lock_mirror_contract.clone(),
                move |_, _, object| {
                    let lock_c = lock_c.clone();
                    let worker = mirror_worker.clone();
                    let host_handle = object.host();
                    object.on_live(move |ctx| {
                        let lock_c = lock_c.clone();
                        let worker = worker.clone();
                        let host_handle = host_handle.clone();
                        Box::pin(async move {
                            let ready = async {
                                loop {
                                    if let Ok(parts) = worker.harness() {
                                        return parts;
                                    }
                                    tokio::time::sleep(Duration::from_millis(25)).await;
                                }
                            };
                            let Ok(parts) = ctx.context.await_with(ready).await else {
                                return Ok(());
                            };
                            let Some(view) = ServerContractRef::hold(
                                &host_handle,
                                lock_c,
                                JsonValue::from_serde(json!({})),
                                &ctx.context,
                            )?
                            else {
                                return Ok(());
                            };
                            if view.wait(&ctx.context).await.is_err() {
                                view.release();
                                return Ok(());
                            }
                            let Ok(lock_source) = view.member("lock") else {
                                view.release();
                                return Ok(());
                            };
                            let (tx, mut rx) = tokio::sync::watch::channel::<Option<SessionLock>>(
                                lock_source
                                    .value()
                                    .ok()
                                    .and_then(|v| {
                                        serde_json::from_value(v.to_json().to_serde().ok()?).ok()
                                    })
                                    .unwrap_or(None),
                            );
                            let sub = lock_source.subscribe(
                                move |call, _| {
                                    if let WatchCall::Value { value, .. } = call
                                        && let Ok(raw) = value.to_serde()
                                        && let Ok(parsed) =
                                            serde_json::from_value::<Option<SessionLock>>(raw)
                                    {
                                        tx.send_replace(parsed);
                                    }
                                    Ok(())
                                },
                                |_| {},
                            );
                            let initial = rx.borrow().clone();
                            let _ =
                                mirror_lock(&parts.durable, initial.as_ref(), &ctx.context).await;
                            while ctx
                                .context
                                .await_with(rx.changed())
                                .await
                                .is_ok_and(|r| r.is_ok())
                            {
                                let next = rx.borrow_and_update().clone();
                                if let Err(err) =
                                    mirror_lock(&parts.durable, next.as_ref(), &ctx.context).await
                                {
                                    eprintln!(
                                        "p2 sessions: mirroring the session's lock failed: {err}"
                                    );
                                }
                            }
                            drop(sub);
                            view.release();
                            Ok(())
                        })
                    });
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )?;
        }

        // Contribute `rename_session` tool (`p2-session-title`).
        {
            let tools_worker = worker.clone();
            let sc = sessions_contract.clone();
            host.serve(
                session_tools_contract.clone(),
                move |_, _, object| {
                    let ext = create_session_title_extension(object.host(), sc.clone());
                    let ext_name = ext.name.clone();
                    let access = IndexMap::from([(
                        "rename_session".into(),
                        ToolAccess {
                            main_only: true,
                            locked: LockedTool::Pass,
                        },
                    )]);
                    let registries = tools_worker.registries.clone();
                    registries.extensions.install_with_access(ext, &access)?;
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
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )?;
        }

        host.serve(transfer_contract.clone(), move |params, principal, _| {
            if principal.cloned().map(serde_json::from_value).transpose()? != Some(Principal::Server) {
                return Err(ChordError::new("forbidden", "server only").into());
            }
            if params.to_serde()?["session"] != id {
                return Err(Error::Invalid("Session transfer address mismatch".into()));
            }
            let mut members = Members::variant("server");
            for method in ["history", "copyFiles", "seed"] {
                let worker = worker.clone();
                members.method(method, move |args, ctx| {
                    let worker = worker.clone();
                    Box::pin(async move {
                        let args: Value = args.to_serde()?;
                        worker.call(async {
                            let harness = worker.harness()?;
                            match method {
                                "history" => {
                                    Ok(wire(json!({"entries": history(&harness.durable, serde_json::from_value(args)?, &ctx.context).await?})))
                                }
                                "copyFiles" => {
                                    let id = worker.init.session()?.id;
                                    let to = arg(&args, "to")?;
                                    if to == id {
                                        return Err(Error::Invalid("Cannot copy files to the same session".into()));
                                    }
                                    let directory = &worker.init.data_dir;
                                    copy::copy(
                                        &host_files(&session_directory(directory, &id)?),
                                        &host_files(&session_directory(directory, to)?),
                                    ).await?;
                                    Ok(wire(Value::Null))
                                }
                                _ => {
                                    seed(&harness.durable, serde_json::from_value(args["entries"].clone())?, &ctx.context).await?;
                                    Ok(wire(Value::Null))
                                }
                            }
                        }).await
                    })
                })?;
            }
            Ok(members)
        }, ServeOptions::default())
    }))
}

async fn history(
    harness: &Harness,
    request: History,
    context: &Context,
) -> Result<Vec<EntryDraft>> {
    let found = harness
        .conversation(request.conversation, context)
        .await
        .map_err(durable_error)?
        .ok_or_else(|| Error::Invalid(format!("Unknown conversation {}", request.conversation)))?;
    let top = found
        .commit(
            async |tx| {
                Ok(tx
                    .conversation(request.conversation)
                    .await?
                    .is_some_and(|row| row.owner.is_none()))
            },
            context,
            CommitOptions::default(),
        )
        .await
        .map_err(durable_error)?;
    if !top {
        return Err(Error::Invalid(
            "Forking a subagent conversation is not supported yet".into(),
        ));
    }
    let entries = found
        .entries(
            EntryQuery {
                max_entry_id: Some(request.before),
                ..Default::default()
            },
            2,
            None,
            context,
        )
        .await
        .map_err(durable_error)?;
    let mut entries = entries.items.into_iter();
    if entries
        .next()
        .is_none_or(|entry| entry.id != request.before)
    {
        return Err(Error::Invalid("Unknown message".into()));
    }
    let Some(previous) = entries.next() else {
        return Ok(Vec::new());
    };
    let view = found
        .context(Some(previous.id), context)
        .await
        .map_err(durable_error)?;
    Ok(view
        .entries
        .into_iter()
        .zip(view.contributions)
        .enumerate()
        .map(|(index, (entry, model))| EntryDraft {
            kind: entry.kind,
            data: entry.data,
            model: (!model.is_empty()).then_some(model),
            head: (index == 0 && view.head.is_some()).then_some(EntryHead::SelfEntry),
            edits: None,
        })
        .collect())
}

async fn seed(harness: &Harness, entries: Vec<EntryDraft>, context: &Context) -> Result<()> {
    let root = harness
        .conversation(ROOT_CONVERSATION_ID, context)
        .await
        .map_err(durable_error)?
        .ok_or_else(|| Error::Invalid("No main conversation".into()))?;
    root.commit(
        async |tx| {
            if tx.latest_entry_id(ROOT_CONVERSATION_ID).await?.is_some() {
                return Err(pi_rs_durable::session::Error::invalid(
                    "The session has history already",
                ));
            }
            for entry in entries {
                tx.append_entry(ROOT_CONVERSATION_ID, entry).await?;
            }
            Ok(())
        },
        context,
        CommitOptions::default(),
    )
    .await
    .map_err(durable_error)
}

fn durable_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
