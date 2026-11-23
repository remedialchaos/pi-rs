//! Stored accounts for an application-defined profile schema.
use crate::{
    Error, Result,
    contracts::{Contract, Member, schema as s},
    delta::{Revision, Segment},
    documents::Definition,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
};
use indexmap::IndexMap;
use serde_json::json;
use std::sync::Arc;

/// Defines `chord.app.users` and the matching facet to install on the server host.
pub fn define_users(profile: JsonValue) -> Result<(Arc<Contract>, Facet)> {
    const NAME: &str = "chord.app.users";
    let item = || {
        s::obj([
            ("id", s::required(s::str())),
            ("profile", s::required(profile.clone())),
        ])
    };
    let contract = Arc::new(Contract::local(
        NAME,
        1,
        vec![],
        IndexMap::from([
            ("list".into(), Member::State(s::arr(item()))),
            (
                "get".into(),
                s::method(Some(s::str()), Some(s::union([profile.clone(), s::nul()]))),
            ),
            ("update".into(), s::method(Some(item()), None)),
            ("remove".into(), s::method(Some(s::str()), None)),
        ]),
    )?);
    let accounts = Arc::new(
        Definition::new(
            NAME,
            s::obj([("users", s::required(s::map(profile)))]),
            1,
            || Ok(JsonValue::from_serde(json!({"users": {}}))),
        )?
        .persisted(true, |_, _, deltas| deltas >= 50),
    );
    let served = contract.clone();
    let facet = define_facet(move |host| {
        let accounts = accounts.clone();
        host.serve(
            served.clone(),
            move |_, _, this| {
                let accounts = this.state(accounts.clone())?;
                let list = Source::derive(
                    this.batch(),
                    vec![accounts.source()?],
                    |values| {
                        let state = values[0].to_json();
                        let Some(JsonValue::Object(users)) = state.get("users") else {
                            return Err(Error::Invalid("invalid accounts document".into()));
                        };
                        let users = users
                            .iter()
                            .map(|(id, profile)| {
                                JsonValue::Object(IndexMap::from([
                                    ("id".into(), JsonValue::String(id.clone())),
                                    ("profile".into(), profile.clone()),
                                ]))
                            })
                            .collect();
                        Ok(Revision::from_json(JsonValue::Array(users))?)
                    },
                    |_| {},
                );
                let mut members = Members::new();
                members.state("list", list)?;
                let read = accounts.clone();
                members.method("get", move |id, _| {
                    let read = read.clone();
                    Box::pin(async move {
                        let JsonValue::String(id) = id else {
                            return Err(Error::Invalid("expected user id".into()));
                        };
                        Ok(read
                            .value()?
                            .get("users")
                            .and_then(|users| users.get(&id.to_utf8().ok()?))
                            .cloned()
                            .unwrap_or(JsonValue::Null))
                    })
                })?;
                for method in ["update", "remove"] {
                    let accounts = accounts.clone();
                    members.method(method, move |args, ctx| {
                        let accounts = accounts.clone();
                        Box::pin(async move {
                            let id = if method == "update" {
                                args.get("id").unwrap_or(&JsonValue::Null)
                            } else {
                                &args
                            };
                            let JsonValue::String(id) = id else {
                                return Err(Error::Invalid("expected user id".into()));
                            };
                            if id == &"__proto__".into() {
                                return if method == "update" {
                                    Err(Error::Invalid("__proto__ is not a user id".into()))
                                } else {
                                    Ok(JsonValue::Null)
                                };
                            }
                            let id = id.clone();
                            let profile = args.get("profile").cloned();
                            accounts
                                .change(&ctx.context, move |draft| {
                                    Box::pin(async move {
                                        let key = [Segment::Key("users".into()), Segment::Key(id)];
                                        if let Some(profile) = profile {
                                            draft.set(&key, profile)?;
                                        } else {
                                            draft.delete(&key)?;
                                        }
                                        Ok(JsonValue::Null)
                                    })
                                })
                                .await
                        })
                    })?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    });
    Ok((contract, facet))
}
