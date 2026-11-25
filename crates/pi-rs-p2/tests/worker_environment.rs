use pi_rs_chord::context::Context;
use pi_rs_durable::{ConversationId, DocToken, DocumentAddress, JsonObject};
use pi_rs_p2::runtime::{location::*, worker_environment::*};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

struct Local;
impl EnvironmentProvider for Local {
    fn open(&self, name: &str) -> Result<Machine, WorkerEnvironmentError> {
        Err(WorkerEnvironmentError::Provider(name.into()))
    }
    fn sandbox(&self, name: &str) -> Result<bool, WorkerEnvironmentError> {
        Ok(name == "gpu")
    }
}
struct Reader;
impl PlacementReader for Reader {
    fn snapshot<'a>(
        &'a self,
        _: &'a DocToken,
        _: &'a DocumentAddress,
        _: &'a Context,
    ) -> PlacementFuture<'a, Option<Arc<JsonObject>>> {
        Box::pin(async {
            Err(WorkerEnvironmentError::Placement(
                "unexpected document read".into(),
            ))
        })
    }
}

struct Plugin {
    env: &'static str,
    supplied: Arc<dyn PlacementReader>,
}
impl ConversationPlacement for Plugin {
    fn own<'a>(
        &'a self,
        read: &'a dyn PlacementReader,
        id: ConversationId,
        ctx: &'a Context,
    ) -> PlacementFuture<'a, Option<Location>> {
        Box::pin(async move {
            match id {
                1 => Err(WorkerEnvironmentError::Placement(
                    "root reached placement".into(),
                )),
                2 if std::ptr::addr_eq(read, self.supplied.as_ref()) => Ok(Some(Location {
                    cwd: "/work".into(),
                    env: Some(self.env.into()),
                })),
                3 => {
                    let signal = ctx
                        .abort_signal()
                        .ok_or(WorkerEnvironmentError::LocationClosed)?;
                    Err(pi_rs_chord::context::AbortError(signal.cancelled().await).into())
                }
                _ => Ok(None),
            }
        })
    }
}
#[test]
fn relative_inputs_follow_the_selected_environment() -> Result<(), String> {
    let envs = vec!["gpu".into()];
    assert_eq!(
        parse_location("gpu:sub", &envs, None, Some("/work"))?.path,
        "~/sub"
    );
    assert_eq!(
        parse_location("../src", &envs, None, Some("C:\\repo\\tests"))?.path,
        "C:\\repo\\src"
    );
    assert!(parse_location("other:~", &envs, None, None).is_err());
    Ok(())
}
#[tokio::test]
async fn session_moves_preserve_placed_children_and_retire_unused_directories()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    std::fs::create_dir(fixture.path().join("sub"))?;
    let root = Location {
        env: None,
        cwd: fixture.path().display().to_string(),
    };
    let mut ex = Execution::new(root.clone(), Arc::new(Local));
    let ctx = Context::background();
    let moved = resolve_directory(
        &mut ex,
        &LocationInput {
            env: None,
            path: "sub".into(),
        },
        &root,
        &ctx,
    )
    .await?;
    assert_eq!(moved.cwd, fixture.path().join("sub").display().to_string());
    let session = SessionLocation::new(root.clone());
    let next = session.next(&ctx);
    session.set(moved.clone());
    session.set(root.clone());
    assert_eq!(next.await?, moved);
    assert_eq!(session.version(), 2);
    let first = ex.at(&root)?;
    assert!(Arc::ptr_eq(&first, &ex.at(&root)?));
    ex.switch(moved.clone());
    ex.retire(std::slice::from_ref(&moved)).await;
    assert!(!Arc::ptr_eq(&first, &ex.at(&root)?));
    let supplied: Arc<dyn PlacementReader> = Arc::new(Reader);
    let plugin: Arc<dyn ConversationPlacement> = Arc::new(Plugin {
        env: "gpu",
        supplied: supplied.clone(),
    });
    let replacement: Arc<dyn ConversationPlacement> = Arc::new(Plugin {
        env: "other",
        supplied: supplied.clone(),
    });
    let installed = Arc::new(AtomicU8::new(0));
    let live = installed.clone();
    ex.set_placement_lookup(move || match live.load(Ordering::SeqCst) {
        1 => Some(plugin.clone()),
        2 => Some(replacement.clone()),
        _ => None,
    });
    ex.set_reader(Some(Arc::new(Reader)));
    let supplied = Some(supplied.as_ref());
    assert_eq!(
        locate_conversation(Some(&ex), 2, &ctx, supplied).await?,
        moved
    );
    installed.store(1, Ordering::SeqCst);
    assert_eq!(locate_conversation(Some(&ex), 1, &ctx, None).await?, moved);
    assert_eq!(locate_conversation(Some(&ex), 2, &ctx, None).await?, moved);
    assert_eq!(
        conversation_confinement(&ex, 2, &ctx, supplied).await?,
        Some(Confinement {
            sandbox: "gpu".into()
        })
    );
    let (cancel_ctx, cancel) = ctx.with_cancel();
    {
        let pending = locate_conversation(Some(&ex), 3, &cancel_ctx, supplied);
        tokio::pin!(pending);
        tokio::select! {biased; result= &mut pending=>return Err(format!("unexpected placement {result:?}").into()), ()=std::future::ready(())=>{}}
        cancel.cancel("stop placement");
        assert!(pending.await.is_err_and(|e| matches!(e, WorkerEnvironmentError::Cancelled(reason) if reason.0 == "stop placement")));
    }
    installed.store(2, Ordering::SeqCst);
    assert_eq!(
        locate_conversation(Some(&ex), 2, &ctx, supplied)
            .await?
            .env
            .as_deref(),
        Some("other")
    );
    assert_eq!(
        conversation_confinement(&ex, 2, &ctx, supplied).await?,
        None
    );
    ex.close().await;
    assert!(ex.at(&moved).is_err());
    Ok(())
}
