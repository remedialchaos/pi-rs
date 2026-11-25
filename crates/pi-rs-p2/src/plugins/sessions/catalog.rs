use super::{arg, now, wire};
use crate::api::v1::session_address;
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{HostKind, read_params, schema},
    data::ChordError,
    host::{Members, ObjectSelf},
    sqlite::SqliteValue,
    state::Source,
    store::{
        ColumnRecord, ColumnType, JsonColumn, Query, Step, StoreEngine, TableDefinition,
        TableHistory, WhereNode,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

const OWNER: &str = "p2.catalog";
const WINDOW: usize = 100_000;

#[derive(Clone)]
pub(super) struct CatalogTable {
    history: TableHistory,
    branches: JsonColumn,
}

impl CatalogTable {
    pub(super) fn new() -> Result<Self> {
        let host = HostKind::new(
            "session",
            read_params(&[("id".into(), schema::str())], "session")?,
        )?;
        let text = || ColumnRecord::new(ColumnType::Str);
        let integer = || ColumnRecord::new(ColumnType::Int);
        let boolean = || ColumnRecord::new(ColumnType::Bool);
        let nonempty = || wire(json!({"type":"string","minLength":1}));
        let positive = || wire(json!({"type":"integer","minimum":1}));
        let model = JsonColumn::new(
            schema::obj([
                ("provider", schema::required(nonempty())),
                ("modelId", schema::required(nonempty())),
            ]),
            1,
            None,
        )?;
        let old_fields = || {
            [
                ("conversation", schema::required(positive())),
                (
                    "label",
                    schema::required(schema::union([schema::str(), schema::nul()])),
                ),
            ]
        };
        let base_branches = JsonColumn::new(schema::arr(schema::obj(old_fields())), 1, None)?;
        let branches = JsonColumn::new(
            schema::arr(schema::obj(
                old_fields()
                    .into_iter()
                    .chain([("parent", schema::opt(positive()))]),
            )),
            2,
            Some(Arc::new(|value, _| Ok(value))),
        )?;
        let base = TableDefinition::new(
            "sessions",
            1,
            IndexMap::from([
                ("session".into(), ColumnRecord::host("session")),
                ("cwd".into(), text()),
                ("env".into(), text().optional()),
                ("title".into(), text()),
                ("titleEdited".into(), boolean()),
                ("createdAt".into(), integer()),
                ("updatedAt".into(), integer()),
                ("archivedAt".into(), integer().optional()),
                ("pinned".into(), boolean()),
                ("model".into(), model.record()),
                ("thinkingLevel".into(), text()),
                ("main".into(), integer()),
                ("branches".into(), base_branches.record()),
            ]),
            vec!["session".into()],
            IndexMap::new(),
        )?
        .bind_host("session", host)?
        .bind_json("model", model)?
        .bind_json("branches", base_branches)?;
        let history = TableHistory::base(base).evolve(|edit| {
            edit.json_schema("branches", branches.clone())?;
            Ok(())
        })?;
        Ok(Self { history, branches })
    }

    fn session(row: &Value) -> Value {
        json!({
            "id":row["session"]["params"]["id"], "env":row["env"], "cwd":row["cwd"],
            "title":row["title"], "createdAt":row["createdAt"], "updatedAt":row["updatedAt"],
            "archivedAt":row["archivedAt"], "pinned":row["pinned"], "model":row["model"],
            "thinkingLevel":row["thinkingLevel"], "main":row["main"], "branches":row["branches"],
        })
    }

    fn existing(&self, step: &Step<'_>, id: &str) -> Result<Option<Value>> {
        let key = pi_rs_chord::data::join_key(&["session".into(), id.into()]);
        let rows = step.read(
            r#"SELECT json_object('cwd', cwd, 'env', env, 'title', title,
                'titleEdited', json(CASE WHEN titleEdited THEN 'true' ELSE 'false' END),
                'createdAt', createdAt, 'updatedAt', updatedAt, 'archivedAt', archivedAt,
                'pinned', json(CASE WHEN pinned THEN 'true' ELSE 'false' END), 'model', json(model),
                'thinkingLevel', thinkingLevel, 'main', main),
                branches, "branches$v" FROM "p2.catalog:sessions" WHERE session = ?"#,
            vec![SqliteValue::Text(key)],
        )?;
        let Some(rows) = rows.first() else {
            return Ok(None);
        };
        let [
            SqliteValue::Text(row),
            SqliteValue::Text(branches),
            SqliteValue::Integer(version),
        ] = rows.as_slice()
        else {
            return Err(Error::Invalid("Malformed session catalog row".into()));
        };
        let mut row: Value = serde_json::from_str(row)?;
        row["session"] = session_address(id);
        row["branches"] = self.branches.read(branches, *version)?.to_serde()?;
        Ok(Some(row))
    }

    async fn query(&self, store: &StoreEngine, id: &str) -> Result<Value> {
        let address = session_address(id);
        let query = Query::new(OWNER, self.history.definition.clone())?
            .filter(WhereNode::Eq(("session".into(), address["key"].clone())))?
            .with_limit(1)?;
        store
            .read(query)
            .await?
            .rows
            .first()
            .map(Self::session)
            .ok_or_else(missing)
    }

    async fn publish(&self, store: &StoreEngine, target: &Source, context: &Context) -> Result<()> {
        let query = Query::new(OWNER, self.history.definition.clone())?
            .order_by("updatedAt", true)?
            .with_limit(WINDOW)?;
        let rows = store.read(query).await?.rows;
        target.set(
            wire(Value::Array(rows.iter().map(Self::session).collect())),
            context,
        )
    }

    fn change(&self, step: &mut Step<'_>, method: &str, args: Value) -> Result<Value> {
        let id = match args["id"].as_str() {
            Some(id) => id.to_owned(),
            None if method == "create" => uuid::Uuid::new_v4().to_string(),
            None => return Err(Error::Invalid("Missing session ID".into())),
        };
        let when = now();
        let previous = self.existing(step, &id)?;
        let row = match method {
            "activity" => {
                let Some(mut row) = previous else {
                    return Ok(Value::Null);
                };
                let timestamp = args["timestamp"]
                    .as_f64()
                    .ok_or_else(|| Error::Invalid("Missing timestamp".into()))?
                    .floor();
                let updated = row["updatedAt"]
                    .as_i64()
                    .unwrap_or_default()
                    .max(timestamp as i64);
                let suggested = args["firstUserText"]
                    .as_str()
                    .map(|text| super::truncate(&text.replace(['\r', '\n'], " "), 100));
                let title = suggested.filter(|title| {
                    !title.is_empty() && row["titleEdited"] != true && row["title"] != *title
                });
                if title.is_none() && row["updatedAt"] == updated {
                    return Ok(Value::Null);
                }
                if let Some(title) = title {
                    row["title"] = json!(title);
                }
                row["updatedAt"] = json!(updated);
                row
            }
            "create" => {
                if previous.is_some() {
                    return Err(ChordError::new("conflict", "Session already exists").into());
                }
                json!({
                    "session":session_address(&id), "cwd":args["cwd"], "env":args["env"],
                    "title":args["title"], "titleEdited":args["titleEdited"] == true,
                    "createdAt":when, "updatedAt":when, "archivedAt":null, "pinned":false,
                    "model":args["model"], "thinkingLevel":args["thinkingLevel"], "main":1, "branches":[],
                })
            }
            "update" => {
                let mut row = previous.ok_or_else(missing)?;
                if args.get("cwd").is_some() || args.get("env").is_some() {
                    row["updatedAt"] =
                        json!(row["updatedAt"].as_i64().unwrap_or_default().max(when));
                }
                if args.get("title").is_some() {
                    row["titleEdited"] = Value::Bool(true);
                }
                if let Some(archive) = args["archived"].as_bool() {
                    row["archivedAt"] = if archive {
                        json!(row["archivedAt"].as_i64().unwrap_or(when))
                    } else {
                        Value::Null
                    };
                }
                for field in [
                    "cwd",
                    "env",
                    "title",
                    "pinned",
                    "model",
                    "thinkingLevel",
                    "main",
                    "branches",
                ] {
                    if let Some(value) = args.get(field) {
                        row[field] = value.clone();
                    }
                }
                row
            }
            _ => return Err(Error::Invalid("Unknown catalog write".into())),
        };
        let stored = row
            .as_object()
            .ok_or_else(|| Error::Invalid("Malformed session row".into()))?
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        step.apply(self.history.definition.put(OWNER, Value::Object(stored))?)?;
        Ok(if method == "activity" {
            Value::Null
        } else {
            Self::session(&row)
        })
    }

    pub(super) fn members(&self, object: &mut ObjectSelf) -> Result<Members> {
        let store = object
            .store()
            .cloned()
            .ok_or_else(|| Error::Invalid("Server store unavailable".into()))?;
        let scope = Arc::new(Mutex::new(()));
        let initial = self.clone();
        let initial_store = store.clone();
        let state = object.follow(move |source, ctx| {
            Box::pin(async move {
                initial_store
                    .prepare_tables(OWNER, std::slice::from_ref(&initial.history), &ctx.context)
                    .await?;
                initial
                    .publish(&initial_store, &source, &ctx.context)
                    .await?;
                Ok(Box::new(|| {}) as Box<dyn FnOnce() + Send>)
            })
        })?;
        let observer = self.clone();
        let observer_store = store.clone();
        let observer_state = state.clone();
        let observer_scope = scope.clone();
        let changes = store.changes();
        object.on_live(move |ctx| {
            let catalog = observer.clone();
            let store = observer_store.clone();
            let state = observer_state.clone();
            let scope = observer_scope.clone();
            let mut changes = changes.clone();
            Box::pin(async move {
                while let Ok(Ok(())) = ctx.context.await_with(changes.changed()).await {
                    let _permit = scope.lock().await;
                    catalog.publish(&store, &state, &ctx.context).await?;
                }
                Ok(())
            })
        });
        let mut members = Members::new();
        members.state("sessions", state.clone())?;
        let table = self.clone();
        let reader = store.clone();
        members.method("get", move |args, _| {
            let (table, store) = (table.clone(), reader.clone());
            Box::pin(async move {
                Ok(wire(
                    table.query(&store, arg(&args.to_serde()?, "id")?).await?,
                ))
            })
        })?;
        for method in ["create", "update", "activity"] {
            let (table, store, state, scope) =
                (self.clone(), store.clone(), state.clone(), scope.clone());
            members.method(method, move |args, ctx| {
                let (table, store, state, scope) =
                    (table.clone(), store.clone(), state.clone(), scope.clone());
                Box::pin(async move {
                    let args = args.to_serde()?;
                    let _permit = scope.lock().await;
                    let change = table.clone();
                    let result = store
                        .transact(move |step| change.change(step, method, args))
                        .await?;
                    table.publish(&store, &state, &ctx.context).await?;
                    Ok(wire(result))
                })
            })?;
        }
        Ok(members)
    }
}

fn missing() -> Error {
    ChordError::new("unavailable", "Session not found").into()
}
