use super::{arg, wire};
use crate::api::v1::{Principal, p2_contracts::lookup};
use indexmap::IndexSet;
use pi_rs_chord::{
    Error, Result,
    contracts::{Contract, LOCAL, schema},
    data::ChordError,
    delta::{Revision, Segment},
    documents::Definition,
    host::{FacetHost, Members, ObjectSelf, ServeOptions},
    json::JsonValue,
    state::Source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) struct Preferences {
    lock: Arc<Contract>,
    lock_store: Arc<Contract>,
    sidebar: Arc<Contract>,
    sidebar_store: Arc<Contract>,
    locks: Arc<Definition>,
    layout: Arc<Definition>,
}

fn schema_of(contract: &Contract, member: &str) -> Result<schema::Property> {
    contract
        .current()
        .variants
        .get(LOCAL)
        .and_then(|local| local.state.get(member))
        .map(|snapshot| schema::required(snapshot.schema.clone()))
        .ok_or_else(|| Error::Invalid(format!("Missing {} state {member}", contract.name())))
}

impl Preferences {
    pub(super) fn new() -> Result<Self> {
        let lock = Arc::new(lookup("p2.session-lock")?);
        let lock_store = Arc::new(lookup("p2.session-locks")?);
        let sidebar = Arc::new(lookup("p2.sidebar-settings")?);
        let sidebar_store = Arc::new(lookup("p2.sidebar-settings-store")?);
        let locks = Arc::new(
            Definition::new(
                "p2.session-locks",
                schema::obj([("locks", schema_of(&lock_store, "locks")?)]),
                1,
                || Ok(wire(json!({"locks":{}}))),
            )?
            .persisted(true, |_, _, _| true),
        );
        let layout = Arc::new(
            Definition::new(
                "p2.sidebar-settings-state",
                schema::obj(
                    ["order", "windows", "collapsed"]
                        .into_iter()
                        .map(|name| Ok((name, schema_of(&sidebar_store, name)?)))
                        .collect::<Result<Vec<_>>>()?,
                ),
                1,
                || Ok(wire(json!({"order":[],"windows":{},"collapsed":{}}))),
            )?
            .persisted(true, |_, _, deltas| deltas >= 20),
        );
        Ok(Self {
            lock,
            lock_store,
            sidebar,
            sidebar_store,
            locks,
            layout,
        })
    }

    pub(super) fn serve(&self, host: &mut FacetHost) -> Result<()> {
        let locks = self.locks.clone();
        host.serve(
            self.lock_store.clone(),
            move |_, _, object| {
                let locks = object.state(locks.clone())?;
                let mut members = Members::new();
                members.state("locks", field(object, locks.source()?, "locks".into()))?;
                members.method("set", move |args, ctx| {
                    let locks = locks.clone();
                    Box::pin(async move {
                        let args = args.to_serde()?;
                        let session = arg(&args, "session")?.to_owned();
                        let lock = args["lock"].clone();
                        locks
                            .change(&ctx.context, move |draft| {
                                Box::pin(async move {
                                    let path = [
                                        Segment::Key("locks".into()),
                                        Segment::Key(session.as_str().into()),
                                    ];
                                    if lock.is_null() {
                                        if draft
                                            .value()?
                                            .to_json()
                                            .get("locks")
                                            .and_then(|map| map.get(&session))
                                            .is_some()
                                        {
                                            draft.delete(&path)?;
                                        }
                                    } else {
                                        draft.set(&path, wire(lock))?;
                                    }
                                    Ok(())
                                })
                            })
                            .await?;
                        Ok(wire(Value::Null))
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let store = self.lock_store.clone();
        host.serve(
            self.lock.clone(),
            move |_, principal, object| match principal.map(Principal::deserialize).transpose()? {
                Some(Principal::Worker { session, .. }) => {
                    let locks = object
                        .get(store.clone(), wire(json!({})))?
                        .member("locks")?;
                    let mut members = Members::variant("worker");
                    members.state("lock", field(object, locks, session))?;
                    Ok(members)
                }
                Some(Principal::Owner { .. }) => {
                    mirror(object, store.clone(), &["locks"], &["set"])
                }
                _ => Err(ChordError::new("forbidden", "session locks are the owner's").into()),
            },
            ServeOptions::default(),
        )?;

        let layout = self.layout.clone();
        host.serve(
            self.sidebar_store.clone(),
            move |_, _, object| {
                let layout = object.state(layout.clone())?;
                let mut members = Members::new();
                for name in ["order", "windows", "collapsed"] {
                    members.state(name, field(object, layout.source()?, name.into()))?;
                }
                for (method, name) in [
                    ("setOrder", "order"),
                    ("setWindow", "windows"),
                    ("setCollapsed", "collapsed"),
                ] {
                    let layout = layout.clone();
                    members.method(method, move |args, ctx| {
                        let layout = layout.clone();
                        Box::pin(async move {
                            let args = args.to_serde()?;
                            let mut path = vec![Segment::Key(name.into())];
                            let value = if name == "order" {
                                let order: IndexSet<String> =
                                    serde_json::from_value(args["order"].clone())?;
                                json!(order)
                            } else {
                                path.push(Segment::Key(arg(&args, "key")?.into()));
                                args[if name == "windows" {
                                    "window"
                                } else {
                                    "collapsed"
                                }]
                                .clone()
                            };
                            layout
                                .change(&ctx.context, move |draft| {
                                    Box::pin(async move { draft.set(&path, wire(value)) })
                                })
                                .await?;
                            Ok(wire(Value::Null))
                        })
                    })?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let store = self.sidebar_store.clone();
        host.serve(
            self.sidebar.clone(),
            move |_, principal, object| {
                if !principal.is_some_and(|principal| principal["kind"] == "owner") {
                    return Err(
                        ChordError::new("forbidden", "sidebar settings are the owner's").into(),
                    );
                }
                mirror(
                    object,
                    store.clone(),
                    &["order", "windows", "collapsed"],
                    &["setOrder", "setWindow", "setCollapsed"],
                )
            },
            ServeOptions::default(),
        )
    }
}

fn field(object: &ObjectSelf, source: Source, key: String) -> Source {
    Source::derive(
        object.batch(),
        vec![source],
        move |values| {
            let value = values[0]
                .to_json()
                .get(&key)
                .cloned()
                .unwrap_or(JsonValue::Null);
            Ok(Revision::from_json(value)?)
        },
        |_| {},
    )
}

fn mirror(
    object: &mut ObjectSelf,
    store: Arc<Contract>,
    states: &[&str],
    methods: &[&'static str],
) -> Result<Members> {
    let held = object.get(store, wire(json!({})))?;
    let mut members = Members::variant("owner");
    for name in states {
        members.state(*name, held.member(name)?)?;
    }
    for &name in methods {
        let held = held.clone();
        members.method(name, move |args, ctx| {
            let held = held.clone();
            Box::pin(async move { held.call(name, args, &ctx.context).await })
        })?;
    }
    Ok(members)
}
