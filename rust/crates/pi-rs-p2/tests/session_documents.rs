use pi_rs_chord::{context::Context, delta::Segment};
use pi_rs_durable::{
    documents::{InitialDocument, resolve_address},
    errors::BoxError,
    session::SessionOptions,
    *,
};
use pi_rs_p2::runtime::{
    main_conversation::*, session_lock::*, session_store::open_session_storage,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn legacy_branch_ids_gain_labels_and_session_lock_survives_reopen() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let ctx = Context::background();
    let open = open_session_storage(temp.path(), "legacy").await?;
    let session = Session::new(open.storage.clone(), SessionOptions::default());
    assert_eq!(main_conversation(&session, &ctx).await?, 1);
    assert_eq!(session_lock(&session, &ctx).await?, None);
    let cutoff = session
        .commit(
            async |tx| {
                tx.create_root_conversation().await?;
                tx.append_entry(
                    1,
                    EntryDraft {
                        kind: "pi.user".into(),
                        head: Some(EntryHead::SelfEntry),
                        ..Default::default()
                    },
                )
                .await
            },
            &ctx,
        )
        .await?;
    let fork = session
        .commit(
            async |tx| {
                tx.fork_conversation(1, cutoff.id, ConversationOwnership::Ownerless)
                    .await
            },
            &ctx,
        )
        .await?;
    let mut old = main_doc();
    let def = Arc::get_mut(&mut old).ok_or("shared definition")?;
    def.version = 1;
    def.initial = InitialDocument::Singleton(Arc::new(|| {
        Ok(JsonObject::from_iter([
            ("conversation".into(), json!(1)),
            ("branches".into(), json!([1])),
        ]))
    }));
    let address = resolve_address(&old, None, None)?;
    session
        .commit(
            async |tx| {
                let change = tx.doc(&old, &address, None).await?;
                change.set(
                    &[Segment::Key("branches".into())],
                    json!([1, fork.id]).into(),
                )?;
                Ok(())
            },
            &ctx,
        )
        .await?;
    session.close(&ctx).await?;
    open.close().await?;
    let open = open_session_storage(temp.path(), "legacy").await?;
    let session = Session::new(open.storage.clone(), SessionOptions::default());
    let mut state = main_state(&session, &ctx).await?;
    assert_eq!(
        state.branches,
        vec![
            MainBranch {
                conversation: 1,
                label: None
            },
            MainBranch {
                conversation: fork.id,
                label: None
            }
        ]
    );
    state.conversation = fork.id;
    state.branches[1].label = Some("renamed".into());
    set_main_state(&session, &state, &ctx).await?;
    let lock = SessionLock {
        mcp: vec!["tools".into()],
        project_mcp: false,
        instructions: true,
        capabilities: vec![LockCapability::Ports],
    };
    set_session_lock(&session, Some(&lock), &ctx).await?;
    session.close(&ctx).await?;
    open.close().await?;
    let open = open_session_storage(temp.path(), "legacy").await?;
    let session = Session::new(open.storage.clone(), SessionOptions::default());
    assert_eq!(main_state(&session, &ctx).await?, state);
    assert_eq!(agent_conversation(&session, fork.id, &ctx).await?, 1);
    assert_eq!(
        top_level_records(&session, &ctx)
            .await?
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![1, fork.id]
    );
    assert_eq!(session_lock(&session, &ctx).await?, Some(lock));
    session.close(&ctx).await?;
    open.close().await
}
