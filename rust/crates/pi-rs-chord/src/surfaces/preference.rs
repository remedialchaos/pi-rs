//! Principal-scoped choices backed by one bounded, persisted singleton document.
use crate::{
    Error, Result,
    contracts::{Contract, HostKind, Member, read_params, schema::*},
    delta::Revision,
    documents::Definition,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
};
use indexmap::IndexMap;
use std::sync::Arc;

pub fn preference_contract(name: &str, pick: JsonValue) -> Result<Contract> {
    Contract::remote(
        name,
        1,
        vec![],
        IndexMap::from([(
            "main".into(),
            IndexMap::from([
                ("pick".into(), Member::State(pick.clone())),
                ("choose".into(), method(Some(pick), None)),
            ]),
        )]),
        Arc::new(|_| {
            HostKind::new("server", read_params(&[], "server")?)?
                .address(JsonValue::Object(IndexMap::new()))
        }),
    )
}
pub fn preference_facet(
    choice: Arc<Contract>,
    store: &str,
    pick: JsonValue,
    none: JsonValue,
) -> Result<Facet> {
    let definition = Arc::new(
        Definition::new(
            store,
            obj([("picks", required(map(pick.clone())))]),
            1,
            || Ok(JsonValue::from_serde(serde_json::json!({"picks":{}}))),
        )?
        .persisted(true, |_, _, deltas| deltas >= 50),
    );
    let storage = Arc::new(Contract::local(
        store,
        1,
        vec![],
        IndexMap::from([
            ("picks".into(), Member::State(map(pick.clone()))),
            (
                "set".into(),
                method(
                    Some(obj([("key", required(str())), ("pick", required(pick))])),
                    None,
                ),
            ),
        ]),
    )?);
    Ok(define_facet(move |setup| {
        let declaration = definition.clone();
        setup.serve(
            storage.clone(),
            move |_, _, self_| {
                let document = self_.state(declaration.clone())?;
                let source = document.source()?;
                let batch = self_.batch();
                let picks = Source::derive(
                    batch,
                    vec![source],
                    |values| {
                        let value = values.first().ok_or(Error::Closed)?.to_json();
                        Ok(Revision::from_json(
                            value
                                .get("picks")
                                .cloned()
                                .ok_or_else(|| Error::Invalid("missing preference picks".into()))?,
                        )?)
                    },
                    |_| {},
                );
                let mut members = Members::new();
                members.state("picks", picks)?;
                members.method("set", move |args, ctx| {
                    let document = document.clone();
                    Box::pin(async move {
                        let Some(JsonValue::String(key)) = args.get("key") else {
                            return Err(Error::Invalid("preference key is a string".into()));
                        };
                        let key = key.clone();
                        let pick = args
                            .get("pick")
                            .cloned()
                            .ok_or_else(|| Error::Invalid("missing preference pick".into()))?;
                        document
                            .change(&ctx.context, move |draft| {
                                Box::pin(async move {
                                    let mut value = draft.value()?.to_json();
                                    let JsonValue::Object(root) = &mut value else {
                                        return Err(Error::Invalid(
                                            "preference document is an object".into(),
                                        ));
                                    };
                                    let Some(JsonValue::Object(picks)) =
                                        root.get_mut(&crate::json::JsonString::from("picks"))
                                    else {
                                        return Err(Error::Invalid(
                                            "preference picks are an object".into(),
                                        ));
                                    };
                                    picks.shift_remove(&key);
                                    picks.insert(key, pick);
                                    while picks.len() > 10_000 {
                                        picks.shift_remove_index(0);
                                    }
                                    draft.set(
                                        &[crate::delta::Segment::Key("picks".into())],
                                        JsonValue::Object(picks.clone()),
                                    )
                                })
                            })
                            .await?;
                        Ok(JsonValue::Null)
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        let storage = storage.clone();
        let none = none.clone();
        setup.serve(
            choice.clone(),
            move |_, principal, self_| {
                let principal = principal
                    .ok_or_else(|| Error::Invalid("preference requires a principal".into()))?;
                let key = crate::data::principal_key(principal)?;
                let held = self_.get(storage.clone(), JsonValue::Object(IndexMap::new()))?;
                let picks = held.member("picks")?;
                let fallback = none.clone();
                let read_key = key.clone();
                let source = Source::derive(
                    self_.batch(),
                    vec![picks],
                    move |values| {
                        let value = values.first().ok_or(Error::Closed)?.to_json();
                        Ok(Revision::from_json(
                            value
                                .get(&read_key)
                                .cloned()
                                .unwrap_or_else(|| fallback.clone()),
                        )?)
                    },
                    |_| {},
                );
                let mut members = Members::variant("main");
                members.state("pick", source)?;
                members.method("choose", move |pick, ctx| {
                    let held = held.clone();
                    let key = key.clone();
                    Box::pin(async move {
                        ctx.call(
                            &held,
                            "set",
                            JsonValue::Object(IndexMap::from([
                                ("key".into(), JsonValue::String(key.into())),
                                ("pick".into(), pick),
                            ])),
                        )
                        .await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}
pub fn theme_choice() -> Result<Contract> {
    preference_contract("chord.themes.choice", theme_pick_schema()?)
}
fn theme_pick_schema() -> Result<JsonValue> {
    let id = union([
        bounded(
            str(),
            Bounds {
                min: None,
                max: Some(128.0),
            },
        )?,
        nul(),
    ]);
    Ok(obj([
        ("dark", required(id.clone())),
        ("light", required(id)),
    ]))
}
pub fn theme_choice_facet() -> Result<Facet> {
    preference_facet(
        Arc::new(theme_choice()?),
        "chord.themes.picks",
        theme_pick_schema()?,
        JsonValue::from_serde(serde_json::json!({"dark":null,"light":null})),
    )
}
pub fn locale_choice() -> Result<Contract> {
    preference_contract("chord.locale.choice", locale_pick_schema()?)
}
fn locale_pick_schema() -> Result<JsonValue> {
    Ok(union([
        pattern(
            bounded(
                str(),
                Bounds {
                    min: None,
                    max: Some(35.0),
                },
            )?,
            "^[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*$",
        ),
        nul(),
    ]))
}
pub fn locale_choice_facet() -> Result<Facet> {
    preference_facet(
        Arc::new(locale_choice()?),
        "chord.locale.picks",
        locale_pick_schema()?,
        JsonValue::Null,
    )
}
