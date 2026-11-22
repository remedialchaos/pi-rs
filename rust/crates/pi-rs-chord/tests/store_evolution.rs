use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    sqlite::{Options, SqliteDatabase},
    store::{
        ColumnRecord, ColumnType, Query, SetNode, StoreEngine, TableDefinition, TableHistory,
        TablePlan,
    },
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn base() -> Result<TableHistory> {
    Ok(TableHistory::base(TableDefinition::new(
        "rows",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Int)),
            ("name".into(), ColumnRecord::new(ColumnType::Str)),
        ]),
        vec!["id".into()],
        IndexMap::new(),
    )?))
}
#[tokio::test]
async fn row_migration_commits_chunks_and_resumes_without_reapplying_stamped_rows() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let old = base()?;
    store
        .prepare_tables("owner", std::slice::from_ref(&old), &Context::background())
        .await?;
    let writes = (0..205)
        .map(|i| old.definition.put("owner", json!({"id":i,"name":"old"})))
        .collect::<Result<Vec<_>>>()?;
    store.commit(writes).await?;
    let failing = Arc::new(AtomicBool::new(true));
    let fail = failing.clone();
    let history = old.evolve(|t| {
        t.rename("name", "label")?
            .add(
                "count",
                ColumnRecord::new(ColumnType::Int).defaulted(json!(0)),
            )?
            .rows(move |r| {
                if r["id"] == 102 && fail.load(Ordering::Acquire) {
                    return Err(Error::Invalid("interrupted".into()));
                }
                Ok(json!({"count":r["count"].as_i64().unwrap_or(0)+1}))
            })?
            .index("label", vec!["label".into()])?;
        Ok(())
    })?;
    assert!(
        store
            .prepare_tables(
                "owner",
                std::slice::from_ref(&history),
                &Context::background()
            )
            .await
            .is_err()
    );
    let partial = db
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM \"owner:rows\" WHERE \"$v\"=2",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await?;
    assert_eq!(partial, 100);
    assert!(
        store
            .read(Query::new("owner", old.definition.clone())?)
            .await
            .is_err()
    );
    failing.store(false, Ordering::Release);
    store
        .prepare_tables(
            "owner",
            std::slice::from_ref(&history),
            &Context::background(),
        )
        .await?;
    let page = store
        .read(Query::new("owner", history.definition.clone())?.with_limit(300)?)
        .await?;
    assert_eq!(page.rows.len(), 205);
    assert!(
        page.rows
            .iter()
            .all(|r| r["count"] == 1 && r["label"] == "old")
    );
    let seq = store.meta().await?.last_seq;
    assert_eq!(seq, 6);
    store
        .prepare_tables("owner", &[history], &Context::background())
        .await?;
    assert_eq!(store.meta().await?.last_seq, seq);
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn empty_patch_stamps_rows_and_incompatible_history_is_refused_before_any_creation()
-> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let old = base()?;
    store
        .prepare_tables("owner", std::slice::from_ref(&old), &Context::background())
        .await?;
    store
        .commit(vec![
            old.definition.put("owner", json!({"id":1,"name":"kept"}))?,
        ])
        .await?;
    let evolved = old.evolve(|t| {
        t.rows(|_| Ok(json!({})))?;
        Ok(())
    })?;
    assert_eq!(evolved.plan(None)?, TablePlan::Create);
    store
        .prepare_tables(
            "owner",
            std::slice::from_ref(&evolved),
            &Context::background(),
        )
        .await?;
    assert_eq!(
        store
            .read(Query::new("owner", evolved.definition.clone())?)
            .await?
            .rows[0]["name"],
        "kept"
    );
    let update = evolved.evolve(|t| {
        t.update(None, SetNode::new())?;
        Ok(())
    })?;
    store
        .prepare_tables(
            "owner",
            std::slice::from_ref(&update),
            &Context::background(),
        )
        .await?;
    let stamped = db
        .call(|c| {
            Ok(c.query_row("SELECT \"$v\" FROM \"owner:rows\"", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .await?;
    assert_eq!(stamped, 3);
    let mut incompatible = update.clone();
    incompatible.definition.fingerprint = "edited".into();
    let fresh = TableHistory::base(TableDefinition::new(
        "fresh",
        1,
        IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Int))]),
        vec!["id".into()],
        IndexMap::new(),
    )?);
    let seq = store.meta().await?.last_seq;
    assert!(
        store
            .prepare_tables("owner", &[fresh, incompatible], &Context::background())
            .await
            .is_err()
    );
    assert_eq!(store.meta().await?.last_seq, seq);
    assert!(
        old.evolve(|t| {
            t.index("by_name", vec!["name".into()])?
                .rename("name", "label")?;
            Ok(())
        })
        .is_err()
    );
    db.close().await?;
    Ok(())
}
