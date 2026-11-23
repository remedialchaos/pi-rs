use pi_rs_chord::{
    context::Context,
    delta::{Op, Segment},
    watch::WatchCall,
};
use pi_rs_durable::{
    ConversationView, ConversationWindow, EntryDraft, Harness,
    session::{Error, Observer},
    storage::memory::MemoryStorage,
};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn observer_sees_each_committed_window_before_return_and_detaches() -> Result<(), Error> {
    let ctx = Context::background();
    let harness = Harness::open(Arc::new(MemoryStorage::new()), Default::default(), &ctx).await?;
    let root = harness.root(&ctx).await?;
    let windows = Arc::new(Mutex::new(Vec::new()));
    let seen = windows.clone();
    let mut observation = harness
        .observe_conversation(
            root.id,
            ConversationWindow {
                from: Some(9999),
                min: Some(1),
                max: Some(2),
            },
            Observer::new(
                move |call: &WatchCall<ConversationView>, _| {
                    let WatchCall::Value {
                        value: view,
                        ops,
                        retired: false,
                    } = call
                    else {
                        return Err("unexpected frame".into());
                    };
                    seen.lock().unwrap_or_else(|e| e.into_inner()).push((
                        view.before,
                        view.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
                        ops.clone(),
                    ));
                    Ok(())
                },
                |_| {},
            ),
            &ctx,
        )
        .await?;
    assert!(observation.value.entries.is_empty());
    let mut ids = Vec::new();
    for index in 0..4 {
        if index == 3 {
            observation.subscription.detach();
        }
        let entry = root
            .commit(
                async |tx| {
                    tx.append_entry(
                        root.id,
                        EntryDraft {
                            kind: "note".into(),
                            ..Default::default()
                        },
                    )
                    .await
                },
                &ctx,
                Default::default(),
            )
            .await?;
        ids.push(entry.id);
        let windows = windows.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(windows.len(), (index + 1).min(3));
        if index < 3 {
            let path = vec![Segment::Key("entries".into())];
            let mut expected = vec![Op::Splice(
                path.clone(),
                index,
                0,
                vec![serde_json::to_value(&entry)?.into()],
            )];
            if index == 2 {
                expected.push(Op::Splice(path, 0, 2, vec![]));
                expected.push(Op::Set(
                    vec![Segment::Key("before".into())],
                    serde_json::json!(ids[1]).into(),
                ));
            }
            assert_eq!(windows[index].2, expected);
        }
        if index < 2 {
            assert_eq!((&windows[index].0, &windows[index].1), (&None, &ids));
        }
        if index == 2 {
            assert_eq!(
                (&windows[index].0, &windows[index].1),
                (&Some(ids[1]), &vec![ids[2]])
            );
        }
    }
    harness.close(&ctx).await
}

#[tokio::test]
async fn graph_observer_survives_attachment_cancel_and_fails_when_session_closes()
-> Result<(), Error> {
    use pi_rs_chord::watch::WatchEnd;
    let ctx = Context::background();
    let harness = Harness::open(Arc::new(MemoryStorage::new()), Default::default(), &ctx).await?;
    let (attach, cancel) = ctx.with_cancel();
    let ends = Arc::new(Mutex::new(Vec::new()));
    let recorded = ends.clone();
    let graph = harness
        .observe_task_graph(
            Observer::new(
                |_, _| Ok(()),
                move |end| {
                    recorded.lock().unwrap_or_else(|e| e.into_inner()).push(end);
                },
            ),
            &attach,
        )
        .await?;
    assert!(graph.value.tasks.is_empty());
    let watch = harness.watch_task_graph(&attach).await?;
    cancel.cancel("fixture cancelled");
    assert_eq!(watch.closed().await, WatchEnd::Aborted);
    assert!(ends.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    harness.close(&ctx).await?;
    assert_eq!(
        *ends.lock().unwrap_or_else(|e| e.into_inner()),
        vec![WatchEnd::Failed("session closed".into())]
    );
    Ok(())
}
