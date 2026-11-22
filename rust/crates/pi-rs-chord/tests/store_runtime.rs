//! Declared-table SQL and replicated write contracts from store/*.test.ts.
use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    sqlite::{Options, SqliteDatabase},
    store::{
        self, ColumnRecord, ColumnType, ExprNode, Query, StoreEngine, StoreWrite, TableDefinition,
        WhereNode,
    },
};
use serde_json::json;
fn table() -> Result<TableDefinition> {
    let mut columns = IndexMap::new();
    columns.insert("id".into(), ColumnRecord::new(ColumnType::Str));
    columns.insert(
        "n".into(),
        ColumnRecord::new(ColumnType::Int).defaulted(json!(0)),
    );
    columns.insert("flag".into(), ColumnRecord::new(ColumnType::Bool));
    columns.insert("body".into(), ColumnRecord::json(1, "0123456789abcdef"));
    columns.insert("note".into(), ColumnRecord::new(ColumnType::Str).optional());
    TableDefinition::new("items", 1, columns, vec!["id".into()], IndexMap::new())
}
#[tokio::test]
async fn claims_are_per_identity_and_prevent_replay_until_every_holder_releases() -> Result<()> {
    use pi_rs_chord::store::Claimant;
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let a = Claimant::new("same label");
    let b = Claimant::new("same label");
    store.claims().table("app:items", &a)?;
    store.claims().table("app:items", &a)?;
    assert!(store.claims().table("app:items", &b).is_err());
    assert!(store.claims().assert_table("app:items", &b).is_err());
    store.claims().documents("app", "chord", &b)?;
    assert!(store.claims().documents("app", "chord", &a).is_err());
    store.claims().documents("app", "other", &a)?;
    assert!(store.apply_batch(vec![]).await.is_err());
    store.claims().release(&a);
    store.claims().table("app:items", &b)?;
    assert!(store.apply_batch(vec![]).await.is_err());
    assert_eq!(store.meta().await?.last_seq, 0);
    store.claims().release(&b);
    assert!(!store.claims().held());
    store.apply_batch(vec![]).await?;
    db.close().await
}
#[tokio::test]
async fn declared_tables_check_cells_and_json_version_pairs() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let t = table()?;
    let writes = t.declaration("app")?;
    engine.commit(writes).await?;
    engine
        .commit(vec![
            t.put("app", json!({"id":"a","flag":true,"body":{"x":1}}))?,
        ])
        .await?;
    let rows = engine.read(Query::new("app", t.clone())?).await?;
    assert_eq!(
        rows.rows,
        vec![json!({"id":"a","n":0,"flag":true,"body":{"x":1}})]
    );
    let physical = db
        .call(|c| {
            Ok(c.query_row(
                "SELECT flag, \"body$v\", \"$v\" FROM \"app:items\"",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?)
        })
        .await?;
    assert_eq!(physical, (1, 1, 1));
    assert!(
        t.put("app", json!({"id":"a","flag":true,"body":{},"extra":1}))
            .is_err()
    );
    assert!(t.put("app", json!({"id":"a","flag":1,"body":{}})).is_err());
    assert!(
        db.call(|c| Ok(c.execute("UPDATE \"app:items\" SET n=9007199254740992", [])?))
            .await
            .is_err()
    );
    db.close().await
}
#[tokio::test]
async fn expressions_bind_values_preserve_null_and_cursor_order() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let t = table()?;
    engine.commit(t.declaration("app")?).await?;
    for id in ["a", "b", "😀", "\u{e000}"] {
        engine
            .commit(vec![
                t.put("app", json!({"id":id,"n":2,"flag":false,"body":null}))?,
            ])
            .await?;
    }
    let query = Query::new("app", t.clone())?.with_limit(2)?;
    let first = engine.read(query.clone()).await?;
    assert_eq!(
        first
            .rows
            .iter()
            .map(|r| r["id"].as_str())
            .collect::<Vec<_>>(),
        vec![Some("a"), Some("b")]
    );
    let second = engine.read(query.after(first.cursor.as_deref())?).await?;
    assert_eq!(second.rows[0]["id"], json!("\u{e000}"));
    let condition: WhereNode =
        serde_json::from_value(json!({"and":[{"isNull":"note"},{"prefix":["id","a"]}]}))?;
    let set: IndexMap<String, ExprNode> =
        serde_json::from_value(json!({"n":{"add":[{"col":"n"},3]},"note":{"concat":["x'","y"]}}))?;
    engine
        .commit(vec![StoreWrite::update_where(
            "app:items",
            condition,
            set,
            IndexMap::new(),
        )])
        .await?;
    assert_eq!(
        engine.read(Query::new("app", t)?).await?.rows[0]["n"],
        json!(5)
    );
    assert!(
        Query::new("other", table()?)?
            .after(first.cursor.as_deref())
            .is_err()
    );
    db.close().await
}
#[tokio::test]
async fn replicated_count_mismatch_rolls_back_and_marks_diverged() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let t = table()?;
    engine.commit(t.declaration("app")?).await?;
    let last = engine.meta().await?.last_seq;
    let write: StoreWrite = serde_json::from_value(
        json!({"kind":"delete","table":"app:items","key":{"id":"missing"},"count":1}),
    )?;
    let commit = store::OutboxCommit {
        seq: last + 1,
        next_id: 2,
        writes: vec![serde_json::to_value(write)?],
    };
    assert!(engine.apply_batch(vec![commit.clone()]).await.is_err());
    assert_eq!(engine.meta().await?.last_seq, last);
    assert!(engine.diverged());
    assert!(engine.apply_batch(vec![commit]).await.is_err());
    db.close().await
}

#[tokio::test]
async fn archive_preserves_versions_and_refuses_bad_end_atomically() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let t = table()?;
    engine.commit(t.declaration("app")?).await?;
    engine
        .commit(vec![
            t.put("app", json!({"id":"a","flag":true,"body":{"x":1}}))?,
        ])
        .await?;
    let bytes = engine.export_gzip().await?;
    let target = SqliteDatabase::open(":memory:", Options::default()).await?;
    let imported = StoreEngine::import_gzip(target.clone(), bytes.clone(), true).await?;
    assert_eq!(
        imported.meta().await?.last_seq,
        engine.meta().await?.last_seq
    );
    assert_eq!(
        imported.read(Query::new("app", t)?).await?.rows[0]["body"],
        json!({"x":1})
    );
    assert!(imported.meta().await?.outbox);
    let mut parts = store::decode_archive(&bytes)?;
    let end = parts
        .last_mut()
        .ok_or_else(|| pi_rs_chord::Error::Invalid("missing archive end".into()))?;
    *end = json!({"kind":"end","writes":999});
    let bad = store::encode_archive(&parts)?;
    let refused = SqliteDatabase::open(":memory:", Options::default()).await?;
    assert!(
        StoreEngine::import_gzip(refused.clone(), bad, false)
            .await
            .is_err()
    );
    assert_eq!(
        refused
            .call(|c| Ok(c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='chord_meta'",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await?,
        0
    );
    db.close().await?;
    target.close().await?;
    refused.close().await
}

#[tokio::test]
async fn json_cells_migrate_only_on_access_validate_results_and_keep_old_storage() -> Result<()> {
    use pi_rs_chord::{
        context::Context,
        contracts::schema,
        json::JsonValue,
        store::{Bindings, JsonColumn, LiveShape, Projection, ReadPlan, ReadRange, TableHistory},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let old = JsonColumn::new(schema::int(), 1, None)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let migrated = calls.clone();
    let new = JsonColumn::new(
        schema::str(),
        2,
        Some(Arc::new(move |value, from| {
            assert_eq!(from, 1);
            migrated.fetch_add(1, Ordering::SeqCst);
            Ok(JsonValue::String(value.to_serde()?.to_string().into()))
        })),
    )?;
    let definition = TableDefinition::new(
        "cells",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("cell".into(), old.record()),
        ]),
        vec!["id".into()],
        IndexMap::new(),
    )?
    .bind_json("cell", old)?;
    let base = TableHistory::base(definition);
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    store
        .prepare_tables("app", std::slice::from_ref(&base), &Context::background())
        .await?;
    store
        .commit(vec![
            base.definition.put("app", json!({"id":"a","cell":7}))?,
        ])
        .await?;
    let next = base.evolve(|edit| {
        edit.json_schema("cell", new.clone())?;
        Ok(())
    })?;
    store
        .prepare_tables("app", std::slice::from_ref(&next), &Context::background())
        .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .read(Query::new("app", next.definition.clone())?)
            .await?
            .rows[0]["cell"],
        json!("7")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let live = store.live_query(LiveShape::Rows {
        range: ReadRange {
            limit: 2,
            ..Default::default()
        },
        paged: false,
    })?;
    live.read(
        &store,
        ReadPlan::new(
            Query::new("app", next.definition.clone())?,
            None,
            Projection::default(),
            false,
            Bindings::default(),
        )?,
        Context::background(),
    )
    .await?;
    assert_eq!(
        live.value()?.to_json().to_serde()?,
        json!([{"id":"a","cell":"7"}])
    );
    let raw = db
        .call(|c| {
            Ok(
                c.query_row("SELECT cell,\"cell$v\" FROM \"app:cells\"", [], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?,
            )
        })
        .await?;
    assert_eq!(raw, ("7".into(), 1));
    assert!(
        JsonColumn::new(schema::str(), 2, None)?
            .read("7", 1)
            .is_err()
    );
    assert!(new.read("7", 3).is_err());
    let bad = JsonColumn::new(
        schema::str(),
        2,
        Some(Arc::new(|_, _| Ok(JsonValue::Bool(false)))),
    )?;
    assert!(bad.read("7", 1).is_err());
    // Current-version writes are trusted and not walked against the JSON schema.
    store
        .commit(vec![
            next.definition.put("app", json!({"id":"b","cell":false}))?,
        ])
        .await?;
    assert_eq!(
        store.read(Query::new("app", next.definition)?).await?.rows[1]["cell"],
        json!(false)
    );
    db.close().await
}

#[tokio::test]
async fn archive_stream_backpressures_sqlite_and_dropping_unread_body_releases_queue() -> Result<()>
{
    use std::time::Duration;
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let t = table()?;
    let mut random = 1u64;
    let text: String = (0..512 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            char::from(32 + (random % 95) as u8)
        })
        .collect();
    let mut writes = t.declaration("app")?;
    writes.push(t.put("app", json!({"id":"a","flag":true,"body":text}))?);
    store.commit(writes).await?;
    let (head, body) = store.export_stream().await?;
    assert_eq!(head, 1);
    let queued = store.meta();
    tokio::pin!(queued);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), &mut queued)
            .await
            .is_err()
    );
    drop(body);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), queued)
            .await
            .map_err(|_| pi_rs_chord::Error::Closed)??
            .last_seq,
        1
    );
    let (_, mut body) = store.export_stream().await?;
    let mut compressed = vec![];
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        assert!(chunk.len() <= 64 * 1024);
        compressed.extend(chunk);
    }
    let expected = store::decode_archive(&store.export_gzip().await?)?;
    assert_eq!(store::decode_archive(&compressed)?, expected);
    db.close().await
}
