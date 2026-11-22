//! Read-plan trust boundaries and store-step live publication contracts.

#[tokio::test]
async fn live_decoder_can_close_its_query_without_deadlock_or_stale_publication() -> Result<()> {
    use pi_rs_chord::json::JsonValue;
    use std::sync::atomic::AtomicBool;
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let table = items()?;
    let mut writes = table.declaration("app")?;
    writes.push(table.put("app", json!({"id":"a","group_id":"g","n":1}))?);
    engine.commit(writes).await?;
    let live = engine.live_query(LiveShape::Rows {
        range: ReadRange {
            limit: 2,
            ..Default::default()
        },
        paged: false,
    })?;
    let completed = Arc::new(AtomicBool::new(false));
    let observed = completed.clone();
    let closing = live.clone();
    live.read_mapped(
        &engine,
        plan(Query::new("app", table)?)?,
        Context::background(),
        move |_, _| {
            let (send, receive) = std::sync::mpsc::channel();
            let closing = closing.clone();
            std::thread::spawn(move || {
                closing.close("closed during decode");
                let _ = send.send(());
            });
            // A bounded wait makes the original lock inversion fail instead of hanging the suite.
            if receive
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
            {
                observed.store(true, Ordering::SeqCst);
            }
            Ok(JsonValue::from_serde(json!("stale")))
        },
    )
    .await?;
    assert!(
        completed.load(Ordering::SeqCst),
        "decoder ran under the query mutex"
    );
    assert!(
        live.value().is_err(),
        "a closed query published a stale decoded value"
    );
    db.close().await
}
use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    sqlite::{Options, SqliteDatabase},
    store::{
        Bindings, Bound, ColumnRecord, ColumnSet, ColumnType, Joined, LiveShape, Projection, Query,
        ReadPlan, ReadRange, ReadRole, StoreEngine, TableDefinition, WhereNode,
    },
};
use rusqlite::{params_from_iter, types::Value as SqlValue};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn items() -> Result<TableDefinition> {
    TableDefinition::new(
        "items",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("group_id".into(), ColumnRecord::new(ColumnType::Str)),
            ("n".into(), ColumnRecord::new(ColumnType::Int)),
        ]),
        vec!["id".into()],
        IndexMap::from([("by_group".into(), vec!["group_id".into()])]),
    )
}
fn groups() -> Result<TableDefinition> {
    TableDefinition::new(
        "groups",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("name".into(), ColumnRecord::new(ColumnType::Str)),
        ]),
        vec!["id".into()],
        IndexMap::new(),
    )
}
fn plan(query: Query) -> Result<ReadPlan> {
    ReadPlan::new(
        query,
        None,
        Projection::default(),
        false,
        Bindings::default(),
    )
}
#[tokio::test]
async fn joined_exists_reads_ranges_counts_and_probes_validate_current_bindings() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let items = items()?;
    let groups = groups()?;
    let mut writes = items.declaration("app")?;
    writes.extend(groups.declaration("app")?);
    writes.extend([
        groups.put("app", json!({"id":"g","name":"one"}))?,
        groups.put("app", json!({"id":"h","name":"two"}))?,
        items.put("app", json!({"id":"a","group_id":"g","n":1}))?,
        items.put("app", json!({"id":"b","group_id":"h","n":2}))?,
        items.put("app", json!({"id":"c","group_id":"missing","n":3}))?,
    ]);
    engine.commit(writes).await?;
    let set = ColumnSet::new("groups", groups.columns.clone(), groups.key.clone(), vec![])?;
    let mut bindings = Bindings::default();
    bindings.bind(
        &set,
        Bound {
            table: "app:groups".into(),
            definition: groups.clone(),
        },
    )?;
    let mut query = Query::new("app", items.clone())?;
    query.condition = Some(WhereNode::Exists((
        "groups".into(),
        vec![("id".into(), "group_id".into())],
        Some(Box::new(WhereNode::Eq(("name".into(), json!("one"))))),
    )));
    let exists = ReadPlan::new(query, None, Projection::default(), false, bindings.clone())?;
    assert_eq!(exists.uses[1].role, ReadRole::Exists);
    assert!(exists.uses[1].keyed);
    let rows = db
        .call(move |c| {
            exists.read(
                c,
                &ReadRange {
                    limit: 10,
                    ..Default::default()
                },
            )
        })
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], SqlValue::Text("a".into()));
    let joined = ReadPlan::new(
        Query::new("app", items.clone())?,
        Some(Joined {
            set: set.clone(),
            pairs: vec![("id".into(), "group_id".into())],
        }),
        Projection {
            reads: Some(vec!["n".into()]),
            partner_reads: Some(vec!["name".into()]),
        },
        false,
        bindings.clone(),
    )?;
    assert!(
        joined
            .select(&ReadRange {
                after: Some(vec![SqlValue::Null]),
                ..Default::default()
            })
            .is_err()
    );
    let statement = joined.probe(
        &ReadRange {
            after: Some(vec![SqlValue::Text("a".into())]),
            through: Some(vec![SqlValue::Text("b".into())]),
            limit: 1,
        },
        &["id".into()],
        &[SqlValue::Text("b".into())],
    )?;
    let probe = db
        .call(move |c| {
            Ok(
                c.query_row(&statement.sql, params_from_iter(statement.params), |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    assert_eq!(probe, 1);
    let counted = joined.count()?;
    let count = db
        .call(move |c| {
            Ok(
                c.query_row(&counted.sql, params_from_iter(counted.params), |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    assert_eq!(count, 2);
    let rows = db
        .call(move |c| {
            joined.read(
                c,
                &ReadRange {
                    after: Some(vec![SqlValue::Text("a".into())]),
                    through: Some(vec![SqlValue::Text("b".into())]),
                    limit: 4,
                },
            )
        })
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["groups:name"], SqlValue::Text("two".into()));
    assert_eq!(rows[0]["group_id"], SqlValue::Text("h".into()));
    assert!(
        ReadPlan::new(
            Query::new("app", items.clone())?,
            Some(Joined {
                set: set.clone(),
                pairs: vec![("name".into(), "group_id".into())]
            }),
            Projection::default(),
            false,
            bindings.clone()
        )
        .is_err()
    );
    bindings.unbind("groups");
    assert!(bindings.get("groups").is_err());
    let mut bad = groups.clone();
    bad.columns
        .insert("name".into(), ColumnRecord::new(ColumnType::Int));
    assert!(
        bindings
            .bind(
                &set,
                Bound {
                    table: "app:groups".into(),
                    definition: bad
                }
            )
            .is_err()
    );
    db.close().await
}
#[tokio::test]
async fn live_windows_reuse_unchanged_rows_switch_atomically_and_close() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let engine = StoreEngine::open(db.clone()).await?;
    let table = items()?;
    engine.commit(table.declaration("app")?).await?;
    let window = engine.live_query(LiveShape::Rows {
        range: ReadRange {
            limit: 2,
            ..Default::default()
        },
        paged: false,
    })?;
    let count = engine.live_query(LiveShape::Count)?;
    let ctx = Context::background();
    let p = plan(Query::new("app", table.clone())?)?;
    window.read(&engine, p.clone(), ctx.clone()).await?;
    count.read(&engine, p.clone(), ctx.clone()).await?;
    let seen = Arc::new(Mutex::new(vec![]));
    let out = seen.clone();
    let counted = count.clone();
    let _subscription = window.source().subscribe(
        move |_, _| {
            out.lock()
                .map_err(|_| pi_rs_chord::Error::Closed)?
                .push(counted.value()?.to_json().to_serde()?);
            Ok(())
        },
        |_| {},
    );
    engine
        .commit(vec![
            table.put("app", json!({"id":"a","group_id":"g","n":1}))?,
            table.put("app", json!({"id":"b","group_id":"g","n":2}))?,
            table.put("app", json!({"id":"c","group_id":"g","n":3}))?,
        ])
        .await?;
    assert_eq!(window.value()?.elements().map(<[_]>::len), Some(2));
    assert_eq!(count.value()?.to_json().to_serde()?, json!(3));
    assert_eq!(
        seen.lock().map_err(|_| pi_rs_chord::Error::Closed)?.last(),
        Some(&json!(3))
    );
    let first = window
        .value()?
        .elements()
        .and_then(|r| r.first())
        .cloned()
        .ok_or(pi_rs_chord::Error::Closed)?;
    engine
        .commit(vec![
            table.put("app", json!({"id":"b","group_id":"g","n":5}))?,
        ])
        .await?;
    assert!(
        window
            .value()?
            .elements()
            .and_then(|r| r.first())
            .is_some_and(|r| r.ptr_eq(&first))
    );
    // Rebinding keeps the old value until the queued SQL load adopts its replacement.
    let future = window.read(
        &engine,
        plan(Query::new("app", table.clone())?.filter(WhereNode::Gt(("n".into(), json!(3))))?)?,
        ctx.clone(),
    );
    future.await?;
    assert_eq!(
        window.value()?.to_json().to_serde()?,
        json!([{"id":"b","group_id":"g","n":5}])
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let changed = calls.clone();
    let _sub = window.source().subscribe(
        move |_, _| {
            changed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        |_| {},
    );
    window.close("released");
    engine
        .commit(vec![
            table.put("app", json!({"id":"d","group_id":"g","n":9}))?,
        ])
        .await?;
    assert!(window.value().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    db.close().await
}
