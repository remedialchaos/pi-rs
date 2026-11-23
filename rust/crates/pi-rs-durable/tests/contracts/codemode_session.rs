#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use pi_rs_ai::InputContent;
use pi_rs_chord::context::Context;
use pi_rs_codemode::{During, StoreWrites};
use pi_rs_durable::{
    ROOT_CONVERSATION_ID, Session, StorageWrite, TaskRecord,
    codemode::{
        CodemodeCatalog, CodemodeDocuments, CodemodeError, CodemodeInvocation,
        CodemodeJournalEntry, CodemodeJournalUpdate, CodemodeNestedInput, CodemodeNestedRequest,
        CodemodeNestedResult, CodemodeSnapshot, CodemodeStart, CodemodeToolInput,
        CodemodeToolOptions, CodemodeTools, SessionCodemodeDocuments, create_codemode_tool,
    },
    errors::BoxError,
    storage::sqlite::SqliteStorage,
    testing::commit_writes,
};
use serde_json::{Value, json};

fn start(seed: u32) -> CodemodeStart {
    CodemodeStart {
        version: 2,
        started_at: 50,
        seed,
        fingerprint: None,
        deadline: None,
        tools: Vec::new(),
        namespaces: Vec::new(),
        globals: Vec::new(),
        max_output_tokens: 1000,
        store: Default::default(),
    }
}

struct NoNestedTools;

#[async_trait]
impl CodemodeTools for NoNestedTools {
    async fn available_tools(&self, _context: &Context) -> Result<CodemodeCatalog, CodemodeError> {
        Ok(CodemodeCatalog::default())
    }
    async fn previous_nested_call(
        &self,
        key: &str,
        _context: &Context,
    ) -> Result<Option<CodemodeNestedInput>, CodemodeError> {
        Err(CodemodeError::Dispatch(format!(
            "Unexpected nested call {key}"
        )))
    }
    async fn execute_nested(
        &self,
        request: CodemodeNestedRequest<'_>,
        _context: &Context,
    ) -> Result<CodemodeNestedResult, CodemodeError> {
        Err(CodemodeError::Dispatch(format!(
            "Unexpected nested tool {}",
            request.name
        )))
    }
}

#[tokio::test]
async fn codemode_journal_result_and_store_survive_sqlite_reopen() -> Result<(), BoxError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("session.db");
    let ctx = Context::background();
    let storage = Arc::new(SqliteStorage::open(&path)?);
    let records = [2, 3, 4].into_iter().map(|id| {
        serde_json::from_value::<TaskRecord>(json!({
            "id":id, "conversationId":ROOT_CONVERSATION_ID, "kind":"codemode-test", "version":1,
            "input":null, "background":false, "abortRequested":false,
            "state":{"status":"running","checkpoint":{}}
        }))
        .map(|value| StorageWrite::Task { value })
    });
    let mut writes = vec![serde_json::from_value(
        json!({"type":"conversation","value":{"id":ROOT_CONVERSATION_ID}}),
    )?];
    writes.extend(records.collect::<Result<Vec<_>, _>>()?);
    commit_writes(storage.as_ref(), writes)?;
    let session = Session::new(storage, Default::default());
    let journal = SessionCodemodeDocuments::new(session.clone(), ROOT_CONVERSATION_ID, 2)?;
    assert!(matches!(
        journal.begin(start(42), &ctx).await?,
        CodemodeSnapshot::Started { rerun: false, .. }
    ));
    journal
        .record_fingerprint("first-fingerprint".into(), &ctx)
        .await?;
    journal
        .update_journal(
            CodemodeJournalUpdate {
                acknowledged: Some(During { seq: 0, issued: 1 }),
                delivered: vec![CodemodeJournalEntry {
                    id: 1,
                    at: 55,
                    issued_after: None,
                    global: None,
                }],
            },
            &ctx,
        )
        .await?;
    journal
        .update_journal(
            CodemodeJournalUpdate {
                acknowledged: Some(During { seq: 1, issued: 1 }),
                delivered: Vec::new(),
            },
            &ctx,
        )
        .await?;
    assert_eq!(
        journal
            .output_dir(Some("original".into()), &ctx)
            .await?
            .as_deref(),
        Some("original")
    );
    let document = SessionCodemodeDocuments::new(session.clone(), ROOT_CONVERSATION_ID, 3)?;
    let tool = create_codemode_tool(CodemodeToolOptions {
        models: false,
        ..Default::default()
    });
    let result = tool
        .execute(
            CodemodeToolInput {
                code: "store('remember', {count: 7}); text(load('remember').count);".into(),
            },
            CodemodeInvocation {
                tools: &NoNestedTools,
                documents: &document,
                models: None,
                env: None,
            },
            &ctx,
        )
        .await?;
    assert!(!result.is_error);
    assert!(
        result
            .output
            .iter()
            .any(|item| matches!(item, InputContent::Text(text) if text.text == "7"))
    );
    session.close(&ctx).await?;

    let restored = Session::new(Arc::new(SqliteStorage::open(&path)?), Default::default());
    let resumed = SessionCodemodeDocuments::new(restored.clone(), ROOT_CONVERSATION_ID, 2)?;
    let CodemodeSnapshot::Started {
        start: resumed_start,
        journal,
        rerun: true,
    } = resumed.begin(start(900), &ctx).await?
    else {
        return Err("resume did not recover the unfinished journal".into());
    };
    assert_eq!(resumed_start.seed, 42);
    assert_eq!(
        resumed_start.fingerprint.as_deref(),
        Some("first-fingerprint")
    );
    assert_eq!(journal.first_issued, Some(1));
    assert_eq!(journal.entries[0].issued_after, Some(1));
    assert_eq!(
        resumed
            .output_dir(Some("replacement".into()), &ctx)
            .await?
            .as_deref(),
        Some("original")
    );
    let document = SessionCodemodeDocuments::new(restored.clone(), ROOT_CONVERSATION_ID, 3)?;
    let CodemodeSnapshot::Finished(saved) = document.begin(start(7), &ctx).await? else {
        return Err("missing saved result".into());
    };
    assert_eq!(saved, result);
    let rerun = document
        .finish(
            result.clone(),
            Some(StoreWrites {
                set: [("remember".into(), Value::Null)].into_iter().collect(),
                delete: Vec::new(),
            }),
            &ctx,
        )
        .await?;
    assert_eq!(rerun, result);
    let next = SessionCodemodeDocuments::new(restored.clone(), ROOT_CONVERSATION_ID, 4)?;
    let CodemodeSnapshot::Started {
        start: new,
        rerun: false,
        ..
    } = next.begin(start(8), &ctx).await?
    else {
        return Err("missing fresh run".into());
    };
    assert_eq!(new.store.get("remember"), Some(&json!({"count":7})));
    restored.close(&ctx).await?;
    Ok(())
}
