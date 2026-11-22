use pi_rs_chord::{
    Error, Result,
    sqlite::{
        Migration, Mode, Options, SqliteDatabase, apply_sqlite_migrations,
        validate_sqlite_schema_prefix,
    },
};

#[tokio::test]
async fn disk_driver_preserves_fifo_rollback_and_read_only_snapshots() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nested/probe.db");
    let db = SqliteDatabase::open(&path, Options::default()).await?;
    db.exec("CREATE TABLE probe (value INTEGER) STRICT").await?;
    let failed = db.transaction(|c| {
        c.execute("INSERT INTO probe VALUES (1)", [])?;
        Err::<(), _>(Error::Invalid("rollback marker".into()))
    });
    let before =
        db.call(|c| Ok(c.query_row("SELECT count(*) FROM probe", [], |r| r.get::<_, i64>(0))?));
    let written = db.run("INSERT INTO probe VALUES (?)", vec![2.into()]);
    let after =
        db.call(|c| Ok(c.query_row("SELECT value FROM probe", [], |r| r.get::<_, i64>(0))?));
    assert!(matches!(failed.await, Err(Error::Invalid(s)) if s == "rollback marker"));
    assert_eq!(before.await?, 0);
    assert_eq!(written.await?, 1);
    assert_eq!(after.await?, 2);
    let pragmas = db
        .call(|c| {
            Ok((
                c.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0))?,
                c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?,
                c.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get::<_, i64>(0))?,
                c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
            ))
        })
        .await?;
    assert_eq!(pragmas, (5_000, 1, 1_000, "wal".into()));
    let ro = SqliteDatabase::open(
        &path,
        Options {
            mode: Mode::ReadOnly,
            ..Options::default()
        },
    )
    .await?;
    assert_eq!(
        ro.transaction(|c| Ok(c.query_row("SELECT value FROM probe", [], |r| r.get::<_, i64>(0))?))
            .await?,
        2
    );
    assert!(ro.exec("INSERT INTO probe VALUES (3)").await.is_err());
    ro.close().await?;
    db.close().await?;
    db.close().await?;
    assert!(matches!(db.exec("SELECT 1").await, Err(Error::Closed)));
    let direct = rusqlite::Connection::open(&path)?;
    assert_eq!(
        direct.query_row("SELECT value FROM probe", [], |r| r.get::<_, i64>(0))?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn migrations_rollback_bootstrap_reject_gaps_and_keep_independent_histories() -> Result<()> {
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    for invalid in [
        "",
        "1bad",
        "bad-prefix",
        "bad; DROP TABLE notes",
        "sqlite_app",
        "SQLITE_app",
    ] {
        assert!(validate_sqlite_schema_prefix(invalid).is_err());
    }
    let migrations = vec![
        Migration {
            version: 1,
            statements: vec![
                "CREATE TABLE notes (value TEXT) STRICT".into(),
                "INSERT INTO notes VALUES ('kept')".into(),
            ],
        },
        Migration {
            version: 2,
            statements: vec!["THIS IS NOT SQL".into()],
        },
    ];
    assert!(
        apply_sqlite_migrations(&db, "notes_schema".into(), migrations.clone())
            .await
            .is_err()
    );
    assert_eq!(
        db.call(
            |c| Ok(c.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await?,
        0
    );
    let first = vec![migrations[0].clone()];
    apply_sqlite_migrations(&db, "notes_schema".into(), first.clone()).await?;
    apply_sqlite_migrations(&db, "notes_schema".into(), first).await?;
    apply_sqlite_migrations(&db, "other_schema".into(), vec![]).await?;
    assert!(
        apply_sqlite_migrations(
            &db,
            "gap_schema".into(),
            vec![Migration {
                version: 2,
                statements: vec![]
            }]
        )
        .await
        .is_err()
    );
    let values = db
        .call(|c| {
            Ok((
                c.query_row("SELECT value FROM notes", [], |r| r.get::<_, String>(0))?,
                c.query_row("SELECT version FROM other_schema", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            ))
        })
        .await?;
    assert_eq!(values, ("kept".into(), 0));
    db.close().await
}
