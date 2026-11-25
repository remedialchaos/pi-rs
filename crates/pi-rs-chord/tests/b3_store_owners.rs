use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::{Address, HostKind, read_params, schema},
    json::JsonValue,
    sqlite::SqliteDatabase,
    store::{
        self, ColumnRecord, ColumnType, DocumentBase, DocumentHistory, DocumentRecord,
        HostDataCounts, HostDataExisting, StoreEngine, StoreWrite, TableDefinition,
    },
};
use serde_json::{Value, json};

async fn engine() -> Result<StoreEngine> {
    StoreEngine::open(SqliteDatabase::open(":memory:", Default::default()).await?).await
}

fn kind() -> Result<HostKind> {
    HostKind::new(
        "session",
        read_params(&[("id".into(), schema::str())], "session.params")?,
    )
}

fn address(id: &str) -> Result<Address> {
    kind()?.address(JsonValue::from_serde(json!({"id":id})))
}

fn table(owner_in_key: bool) -> Result<TableDefinition> {
    let mut key = vec!["id".into()];
    if owner_in_key {
        key.insert(0, "session".into());
    }
    TableDefinition::new(
        "items",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("session".into(), ColumnRecord::host("session")),
        ]),
        key,
        IndexMap::new(),
    )?
    .bind_host("session", kind()?)
}

fn put(table: &TableDefinition, address: &Address, id: &str) -> Result<StoreWrite> {
    table.put(
        "owner",
        json!({"id":id,"session":{"kind":address.kind,"params":address.params}}),
    )
}

async fn seed(
    engine: &StoreEngine,
    table: &TableDefinition,
    address: &Address,
    id: &str,
    value: i32,
) -> Result<()> {
    let meta = engine.meta().await?;
    engine
        .commit(vec![
            put(table, address, id)?,
            StoreWrite::DocumentCreate {
                record: DocumentRecord {
                    id: meta.next_id,
                    owner: "owner".into(),
                    namespace: "chord".into(),
                    kind: "doc".into(),
                    key: Some(address.key.clone()),
                    history: DocumentHistory::None,
                    created_at: meta.last_seq + 1,
                    retired_at: None,
                    meta: None,
                    host: Some(address.key.clone()),
                    revision: 1,
                    version: 1,
                    fingerprint: "fingerprint".into(),
                },
                base: Some(DocumentBase {
                    version: 1,
                    fingerprint: "fingerprint".into(),
                    value: JsonValue::from_serde(json!(value)),
                }),
            },
        ])
        .await?;
    Ok(())
}

#[tokio::test]
async fn copy_remaps_owned_cells_and_document_identity_without_touching_other_addresses()
-> Result<()> {
    let store = engine().await?;
    let (a, b, c) = (address("a|b")?, address("copy")?, address("neighbor")?);
    let table = table(true)?;
    store.commit(table.declaration("owner")?).await?;
    seed(&store, &table, &a, "same", 10).await?;
    seed(&store, &table, &b, "stale", 20).await?;
    seed(&store, &table, &c, "same", 30).await?;
    let source = store.export_host_data(&a).await?;
    let neighbor = payload(&store.export_host_data(&c).await?)?;
    let old_id = store.meta().await?.next_id;
    assert!(
        store
            .import_host_data(
                store.prepare_host_data(&a, &b, &source)?,
                HostDataExisting::Refuse
            )
            .await
            .is_err()
    );
    let counts = store
        .import_host_data(
            store.prepare_host_data(&a, &b, &source)?,
            HostDataExisting::Replace,
        )
        .await?;
    assert_eq!(
        counts,
        HostDataCounts {
            documents: 1,
            rows: 1
        }
    );
    let copied = payload(&store.export_host_data(&b).await?)?;
    assert_eq!(copied[0]["row"], json!({"id":"same","session":b.key}));
    assert_eq!(copied[1]["record"]["id"], old_id);
    assert_eq!(copied[1]["record"]["key"], b.key);
    assert_eq!(copied[1]["record"]["host"], b.key);
    assert_eq!(copied[2]["content"]["value"], 10);
    assert_eq!(
        store.delete_host_data(&a).await?,
        HostDataCounts {
            documents: 1,
            rows: 1
        }
    );
    assert!(payload(&store.export_host_data(&a).await?)?.is_empty());
    assert_eq!(payload(&store.export_host_data(&b).await?)?, copied);
    assert_eq!(payload(&store.export_host_data(&c).await?)?, neighbor);
    Ok(())
}

fn payload(bytes: &[u8]) -> Result<Vec<Value>> {
    Ok(store::decode_archive(bytes)?
        .into_iter()
        .filter(|p| {
            matches!(
                p["kind"].as_str(),
                Some("put" | "document.create" | "document.revision")
            )
        })
        .collect())
}

#[tokio::test]
async fn malformed_ownership_is_rejected_and_primary_key_collision_rolls_back_replacement()
-> Result<()> {
    let store = engine().await?;
    let (a, b) = (address("a")?, address("b")?);
    let table = table(false)?;
    store.commit(table.declaration("owner")?).await?;
    seed(&store, &table, &a, "global-key", 1).await?;
    seed(&store, &table, &b, "must-survive", 2).await?;
    let source = store.export_host_data(&a).await?;
    let mut parts = store::decode_archive(&source)?;
    let row = parts
        .iter_mut()
        .find(|p| p["kind"] == "put")
        .ok_or_else(|| pi_rs_chord::Error::Invalid("missing fixture row".into()))?;
    row["row"]["session"] = json!(b.key);
    assert!(
        store
            .prepare_host_data(&a, &b, &store::encode_archive(&parts)?)
            .is_err()
    );
    let before = store.export_gzip().await?;
    assert!(
        store
            .import_host_data(
                store.prepare_host_data(&a, &b, &source)?,
                HostDataExisting::Replace
            )
            .await
            .is_err()
    );
    assert_eq!(store.export_gzip().await?, before);
    Ok(())
}

#[tokio::test]
async fn live_host_document_reads_observe_replacement_and_deletion() -> Result<()> {
    use pi_rs_chord::{
        context::Context,
        contracts::Contract,
        documents::{Definition, DocumentKey, KeyedBy},
        host::{HostOptions, Members, PluginFacet, ServeOptions, create_host, define_facet},
    };
    use std::sync::Arc;
    let host = create_host(HostOptions {
        store: Some(engine().await?),
        ..Default::default()
    })?;
    let kind = kind()?;
    let definition = Arc::new(
        Definition::new(
            "value",
            schema::obj([("n", schema::required(schema::int()))]),
            1,
            || JsonValue::parse(r#"{"n":0}"#),
        )?
        .keyed(KeyedBy::Host(kind))
        .persisted(true, |_, _, _| false),
    );
    let contract = Arc::new(Contract::local(
        "test.owner",
        1,
        vec![],
        IndexMap::from([
            ("get".into(), schema::method(None, Some(schema::int()))),
            ("set".into(), schema::method(Some(schema::int()), None)),
        ]),
    )?);
    let serving = contract.clone();
    let address = address("a")?;
    let key = DocumentKey::Address(JsonValue::from_serde(
        json!({"kind":address.kind,"params":address.params}),
    ));
    let facet = define_facet(move |setup| {
        let (definition, key) = (definition.clone(), key.clone());
        setup.serve(
            serving.clone(),
            move |_, _, owner| {
                let document = owner.state(definition.clone())?;
                let reader = document.clone();
                let read_key = key.clone();
                let update_key = key.clone();
                let mut members = Members::new();
                members.method("get", move |_, ctx| {
                    let (reader, key) = (reader.clone(), read_key.clone());
                    Box::pin(async move {
                        Ok(reader
                            .keyed_handle()?
                            .get(key, &ctx.context)
                            .await?
                            .and_then(|v| v.to_json().get("n").cloned())
                            .unwrap_or(JsonValue::Number(-1.0)))
                    })
                })?;
                members.method("set", move |value, ctx| {
                    let (document, key) = (document.clone(), update_key.clone());
                    Box::pin(async move {
                        document
                            .change_at(Some(key), &ctx.context, move |draft| {
                                Box::pin(async move {
                                    draft
                                        .set(&[pi_rs_chord::delta::Segment::Key("n".into())], value)
                                })
                            })
                            .await?;
                        Ok(JsonValue::Null)
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    });
    let context = Context::background();
    host.load(vec![PluginFacet::new("owner", facet)], &context)
        .await?;
    let held = host.hold(contract, JsonValue::Object(Default::default()), &context)?;
    held.wait(&context).await?;
    held.call("set", JsonValue::Number(1.0), &context).await?;
    held.call("set", JsonValue::Number(2.0), &context).await?; // retained delta in the archive
    let archive = host.export_host_data(&address).await?;
    held.call("set", JsonValue::Number(3.0), &context).await?;
    host.import_host_data(
        host.prepare_host_data(&address, &address, &archive)?,
        HostDataExisting::Replace,
    )
    .await?;
    assert_eq!(
        held.call("get", JsonValue::Null, &context).await?,
        JsonValue::Number(2.0)
    );
    host.delete_host_data(&address).await?;
    assert_eq!(
        held.call("get", JsonValue::Null, &context).await?,
        JsonValue::Number(-1.0)
    );
    host.close(&context).await
}
