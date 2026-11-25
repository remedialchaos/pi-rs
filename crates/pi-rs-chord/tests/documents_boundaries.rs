use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{HostKind, read_params, schema as s},
    delta::Segment,
    documents::*,
    json::JsonValue,
    state::Batch,
};
use std::sync::Arc;

fn j(text: &str) -> Result<JsonValue> {
    JsonValue::parse(text)
}

#[tokio::test]
async fn host_keyed_handles_yield_decoded_addresses_and_persist_host_identity() -> Result<()> {
    let db = pi_rs_chord::sqlite::SqliteDatabase::open(":memory:", Default::default()).await?;
    pi_rs_chord::store::apply_store_schema(&db).await?;
    let host = HostKind::new(
        "session",
        read_params(&[("id".into(), s::str())], "session.params")?,
    )?;
    let definition = Arc::new(
        Definition::new("items", s::obj([("n", s::required(s::int()))]), 1, || {
            j(r#"{"n":0}"#)
        })?
        .keyed(KeyedBy::Host(host.clone()))
        .persisted(true, |_, _, _| false),
    );
    let context = Context::background();
    let documents = Documents::new(
        "owner",
        Batch::new(|_| {}),
        Some(DocumentStore::new(db.clone(), "owner")),
        |_| {},
    );
    let attempt = documents.attempt();
    let handle = attempt.declare(definition)?;
    attempt.activate(&context).await?;
    let address = j(r#"{"kind":"session","params":{"id":"a|b\\c"},"key":"forged"}"#)?;
    let key = DocumentKey::Address(address.clone());
    let h = handle.clone();
    documents
        .transaction(handle.token(), &context, move |tx| async move {
            tx.draft(&h, Some(key))
                .await?
                .set(&[Segment::Key("n".into())], j("1")?)
        })
        .await?;
    let keys = handle.handle_keys(&context).await?;
    let [DocumentKey::Address(decoded)] = keys.as_slice() else {
        return Err(Error::Invalid("decoded host address missing".into()));
    };
    assert_eq!(decoded.get("kind"), address.get("kind"));
    assert_eq!(decoded.get("params"), address.get("params"));
    let stored_key = host
        .key_of_address(&address)?
        .ok_or_else(|| Error::Invalid("fixture host".into()))?;
    assert_eq!(
        decoded.get("key"),
        Some(&JsonValue::String(stored_key.clone().into()))
    );
    assert!(
        handle
            .get(
                DocumentKey::Address(j(r#"{"kind":"other","params":{"id":"a"}}"#)?),
                &context
            )
            .await
            .is_err()
    );
    assert_eq!(
        db.call(
            |c| Ok(c.query_row("SELECT host FROM chord_documents", [], |r| r
                .get::<_, String>(0))?)
        )
        .await?,
        stored_key
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn failing_pinned_key_migration_breaks_the_live_owner_before_publication() -> Result<()> {
    let documents = Documents::new("owner", Batch::new(|_| {}), None, |_| {});
    let context = Context::background();
    let definition = Arc::new(
        Definition::new("items", s::obj([("n", s::required(s::int()))]), 1, || {
            j(r#"{"n":0}"#)
        })?
        .keyed(KeyedBy::String),
    );
    let attempt = documents.attempt();
    let handle = attempt.declare(definition)?;
    let pin = attempt.at(&handle, "key")?;
    attempt.activate(&context).await?;
    let h = handle.clone();
    documents
        .transaction(handle.token(), &context, move |tx| async move {
            tx.draft(&h, Some("key".into()))
                .await?
                .set(&[Segment::Key("n".into())], j("1")?)
        })
        .await?;
    let retained = pin.value()?;
    let next = documents.attempt();
    next.declare(Arc::new(
        Definition::new("items", s::obj([("n", s::required(s::int()))]), 2, || {
            j(r#"{"n":0}"#)
        })?
        .keyed(KeyedBy::String)
        .migrating(|_, _| Err(Error::Invalid("migration failed".into()))),
    ))?;
    assert!(next.activate(&context).await.is_err());
    let result = documents
        .transaction(documents.token(), &context, |_| async { Ok(()) })
        .await;
    assert!(matches!(result, Err(Error::Chord(error)) if error.code == "broken"));
    assert_eq!(retained.to_json(), j(r#"{"n":1}"#)?);
    Ok(())
}
