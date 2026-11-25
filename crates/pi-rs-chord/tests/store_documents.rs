//! A document change and staged SQL form one atomic, replayable store step.
use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::schema as s,
    delta::Segment,
    documents::{Definition, DocumentStore, Documents, KeyedBy},
    json::JsonValue,
    sqlite::{Options, SqliteDatabase},
    state::Batch,
    store::{
        self, ColumnRecord, ColumnType, Query, StagedSql, StoreEngine, TableDefinition, WriteOrder,
    },
};
use serde_json::json;
use std::sync::Arc;
#[tokio::test]
async fn staged_sql_and_document_changes_commit_and_rollback_as_one_replica_step() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    db.call(|c| {
        c.execute("UPDATE chord_meta SET outbox=1", [])?;
        Ok(())
    })
    .await?;
    let table = TableDefinition::new(
        "index",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("n".into(), ColumnRecord::new(ColumnType::Int)),
        ]),
        vec!["id".into()],
        IndexMap::new(),
    )?;
    engine.commit(table.declaration("app")?).await?;
    let docs = Documents::new(
        "owner",
        Batch::new(|_| {}),
        Some(DocumentStore::from_engine(engine.clone(), "app")),
        |_| {},
    );
    let a = docs.attempt();
    let h = a.declare(Arc::new(
        Definition::new("list", s::obj([("n", s::required(s::int()))]), 1, || {
            Ok(JsonValue::from_serde(json!({"n":0})))
        })?
        .keyed(KeyedBy::Singleton)
        .persisted(true, |_, _, _| false),
    ))?;
    let ctx = Context::background();
    a.activate(&ctx).await?;
    let before = engine.meta().await?;
    let live = engine.live_query(store::LiveShape::Rows {
        range: store::ReadRange {
            limit: 10,
            ..Default::default()
        },
        paged: false,
    })?;
    let plan = store::ReadPlan::new(
        Query::new("app", table.clone())?,
        None,
        store::Projection::default(),
        false,
        store::Bindings::default(),
    )?;
    live.read(&engine, plan, ctx.clone()).await?;
    let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = frames.clone();
    let observed = live.clone();
    let _subscription = h.source()?.subscribe(
        move |_, _| {
            output
                .lock()
                .map_err(|_| pi_rs_chord::Error::Closed)?
                .push(observed.value()?.to_json().to_serde()?);
            Ok(())
        },
        |_| {},
    );
    let change = h.clone();
    let put = table.put("app", json!({"id":"a","n":3}))?;
    docs.transaction(h.token(), &ctx, move |tx| async move {
        tx.draft(&change, None)
            .await?
            .set(&[Segment::Key("n".into())], JsonValue::Number(3.0))?;
        tx.stage(Arc::new(StagedSql::new(vec![put], WriteOrder::Write)?))
    })
    .await?;
    assert_eq!(h.value()?.to_json().to_serde()?, json!({"n":3}));
    assert_eq!(
        engine.read(Query::new("app", table.clone())?).await?.rows,
        vec![json!({"id":"a","n":3})]
    );
    assert_eq!(engine.meta().await?.last_seq, before.last_seq + 1);
    assert_eq!(
        frames
            .lock()
            .map_err(|_| pi_rs_chord::Error::Closed)?
            .last(),
        Some(&json!([{"id":"a","n":3}]))
    );
    let committed = engine.meta().await?;
    let change = h.clone();
    let mut invalid = table.put("app", json!({"id":"b","n":9}))?;
    if let store::StoreWrite::Put { row, .. } = &mut invalid {
        row["n"] = json!("not an integer");
    }
    assert!(
        docs.transaction(h.token(), &ctx, move |tx| async move {
            tx.draft(&change, None)
                .await?
                .set(&[Segment::Key("n".into())], JsonValue::Number(9.0))?;
            tx.stage(Arc::new(StagedSql::new(vec![invalid], WriteOrder::Write)?))
        })
        .await
        .is_err()
    );
    assert_eq!(h.value()?.to_json().to_serde()?, json!({"n":3}));
    assert_eq!(engine.meta().await?, committed);
    assert_eq!(engine.count(Query::new("app", table.clone())?).await?, 1);
    // Even without document plans, persistent staged work must enter SQL admission.
    let put = table.put("app", json!({"id":"b","n":4}))?;
    docs.transaction(h.token(), &ctx, move |tx| async move {
        tx.stage(Arc::new(StagedSql::new(vec![put], WriteOrder::Write)?))
    })
    .await?;
    assert_eq!(engine.count(Query::new("app", table.clone())?).await?, 2);
    let archive = engine.export_gzip().await?;
    let replica_db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let replica = StoreEngine::import_gzip(replica_db.clone(), archive, false).await?;
    assert_eq!(replica.read(Query::new("app", table)?).await?.rows.len(), 2);
    let rows = db
        .call(|c| {
            Ok(
                c.query_row("SELECT writes FROM chord_outbox WHERE seq=2", [], |r| {
                    r.get::<_, String>(0)
                })?,
            )
        })
        .await?;
    let writes: Vec<serde_json::Value> = serde_json::from_str(&rows)?;
    assert!(writes.iter().any(|w| w["kind"] == "document.create"));
    assert!(writes.iter().any(|w| w["kind"] == "put"));
    db.close().await?;
    replica_db.close().await
}
