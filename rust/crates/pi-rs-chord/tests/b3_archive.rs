use indexmap::IndexMap;
use pi_rs_chord::{
    node::app::{
        ArchiveAddress, ArchiveLimits, ArchiveManifest, TarEntry, pack_data_to, read_tar,
        unpack_staging_from, write_tar,
    },
    sqlite::{Options, SqliteDatabase},
    store::{ColumnRecord, ColumnType, StoreEngine, TableDefinition},
};
use std::{fs, io::Read};

fn manifest() -> ArchiveManifest {
    ArchiveManifest {
        format: 2,
        app: "example".into(),
        api: "1".into(),
        address: ArchiveAddress {
            kind: "worker".into(),
            key: "source".into(),
        },
    }
}

#[tokio::test]
async fn file_archive_round_trips_store_server_and_finished_blobs() -> pi_rs_chord::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let table = TableDefinition::new(
        "archive_items",
        1,
        IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Str))]),
        vec!["id".into()],
        IndexMap::new(),
    )?;
    let mut writes = table.declaration("example")?;
    writes.push(table.put("example", serde_json::json!({"id":"saved"}))?);
    store.commit(writes).await?;
    let files = root.join("files");
    fs::create_dir_all(files.join("nested"))?;
    let blob = (0..250)
        .cycle()
        .take(3 * 64 * 1024 + 1)
        .collect::<Vec<u8>>();
    fs::write(files.join("nested/blob"), &blob)?;
    fs::write(files.join("nested/empty"), [])?;
    fs::write(files.join("nested/.upload"), b"unfinished")?;
    let server = root.join("server");
    fs::write(&server, b"server JSONL\n")?;
    let archive = root.join("archive.tar");
    fs::write(&archive, b"old export")?;
    assert!(
        pack_data_to(manifest(), &store, &files, &root.join("missing"), &archive)
            .await
            .is_err()
    );
    assert_eq!(fs::read(&archive)?, b"old export");
    assert_eq!(fs::read_dir(root)?.count(), 3);
    pack_data_to(manifest(), &store, &files, &server, &archive).await?;
    let entries = read_tar(&fs::read(&archive)?, ArchiveLimits::default())?;
    assert_eq!(
        entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        [
            "manifest.json",
            "store.archive",
            "server.archive",
            "files/nested/blob",
            "files/nested/empty"
        ]
    );
    let stage = root.join("stage");
    assert_eq!(
        unpack_staging_from(
            &archive,
            "example",
            "worker",
            &stage,
            ArchiveLimits::default()
        )
        .await?,
        manifest()
    );
    assert_eq!(fs::read(stage.join("server.archive"))?, b"server JSONL\n");
    assert_eq!(fs::read(stage.join("files/nested/blob"))?, blob);
    assert!(fs::read(stage.join("files/nested/empty"))?.is_empty());
    let restored_db = SqliteDatabase::open(stage.join("store.sqlite"), Options::default()).await?;
    let restored = StoreEngine::open(restored_db.clone()).await?;
    assert_eq!(
        pi_rs_chord::store::decode_archive(&restored.export_gzip().await?)?,
        pi_rs_chord::store::decode_archive(&store.export_gzip().await?)?
    );
    restored_db.close().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn file_import_rejects_invalid_input_without_leaving_staging() -> pi_rs_chord::Result<()> {
    let dir = tempfile::tempdir()?;
    let archive = dir.path().join("archive.tar");
    let stage = dir.path().join("stage");
    let entry = |path: &str, bytes: Vec<u8>| TarEntry {
        path: path.into(),
        bytes,
        directory: false,
    };
    let manifest = entry("manifest.json", serde_json::to_vec(&manifest())?);
    let db = SqliteDatabase::open(":memory:", Options::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(store.export_gzip().await?.as_slice()).read_to_end(&mut raw)?;
    db.close().await?;
    let store = entry("store.archive", raw);
    let server = entry("server.archive", vec![]);
    let valid_shape = write_tar(&[manifest.clone(), store.clone(), server.clone()])?;
    let cases = [
        (
            valid_shape.clone(),
            "wrong",
            "worker",
            ArchiveLimits::default(),
        ),
        (
            valid_shape.clone(),
            "example",
            "wrong",
            ArchiveLimits::default(),
        ),
        (
            valid_shape.clone(),
            "example",
            "worker",
            ArchiveLimits {
                bytes: 1,
                entries: 100,
            },
        ),
        (
            valid_shape.clone(),
            "example",
            "worker",
            ArchiveLimits {
                bytes: 1024,
                entries: 2,
            },
        ),
        (
            write_tar(&[
                manifest.clone(),
                entry("store.archive", b"not JSONL".to_vec()),
                server.clone(),
            ])?,
            "example",
            "worker",
            ArchiveLimits::default(),
        ),
        (
            valid_shape[..valid_shape.len() - 1].to_vec(),
            "example",
            "worker",
            ArchiveLimits::default(),
        ),
        (
            write_tar(&[manifest.clone(), store.clone()])?,
            "example",
            "worker",
            ArchiveLimits::default(),
        ),
        (
            write_tar(&[store, manifest, server])?,
            "example",
            "worker",
            ArchiveLimits::default(),
        ),
    ];
    for (bytes, app, kind, limits) in cases {
        fs::write(&archive, bytes)?;
        assert!(
            unpack_staging_from(&archive, app, kind, &stage, limits)
                .await
                .is_err()
        );
        assert!(!stage.exists());
    }
    fs::create_dir(&stage)?;
    fs::write(stage.join("keep"), b"live data")?;
    fs::write(&archive, valid_shape)?;
    assert!(
        unpack_staging_from(
            &archive,
            "example",
            "worker",
            &stage,
            ArchiveLimits::default()
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read(stage.join("keep"))?, b"live data");
    Ok(())
}
