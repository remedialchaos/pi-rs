use pi_rs_chord::{context::Context, jobs::*, json::JsonValue, state::Batch};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
fn definition() -> pi_rs_chord::Result<JobDefinition> {
    define_job(
        "test/work",
        JobOptions {
            concurrency: Some(1),
            ..JobOptions::new(
                1,
                JsonValue::parse("{\"type\":\"integer\"}")?,
                JsonValue::Bool(true),
            )
        },
    )
}

#[tokio::test]
async fn job_state_fingerprints_use_schema_and_invalid_changes_do_not_commit()
-> pi_rs_chord::Result<()> {
    use pi_rs_chord::{
        contracts::schema::*,
        documents::{Definition, KeyedBy},
    };
    let state = |n: i64| -> pi_rs_chord::Result<Arc<Definition>> {
        Ok(Arc::new(Definition::new(
            "progress",
            obj([("n", required(int()))]),
            1,
            move || Ok(JsonValue::from_serde(serde_json::json!({"n":n}))),
        )?))
    };
    let make = |state| {
        define_job(
            "stateful",
            JobOptions {
                state: Some(state),
                ..JobOptions::new(1, int(), int())
            },
        )
    };
    let first = make(state(0)?)?;
    assert_eq!(first.fingerprint, make(state(99)?)?.fingerprint);
    assert!(
        define_job(
            "bad_state",
            JobOptions {
                state: Some(state(0)?),
                persist: Some(Resume::Rerun),
                ..JobOptions::new(1, int(), int())
            }
        )
        .is_err()
    );
    let keyed = Arc::new(
        Definition::new("keyed", obj::<&str>([]), 1, || {
            Ok(JsonValue::Object(Default::default()))
        })?
        .keyed(KeyedBy::String),
    );
    assert!(make(keyed).is_err());
    let job = Job::new(
        first,
        JobImplementation::new(|_, ctx| {
            Box::pin(async move {
                assert!(
                    ctx.change_state(|value| {
                        *value = JsonValue::String("not an object".into());
                        Ok(())
                    })
                    .await
                    .is_err()
                );
                assert_eq!(
                    ctx.state()?,
                    Some(JsonValue::from_serde(serde_json::json!({"n":0})))
                );
                ctx.change_state(|value| {
                    *value = JsonValue::from_serde(serde_json::json!({"n":4}));
                    Ok(())
                })
                .await?;
                Ok(JsonValue::Number(4.0))
            })
        }),
        Batch::new(|_| {}),
        None,
    )
    .await?;
    job.live()?;
    let outcome = job
        .start(JsonValue::Number(1.0), None, &Context::background())
        .await?
        .outcome(&Context::background())
        .await?;
    assert_eq!(
        outcome.state,
        Some(JsonValue::from_serde(serde_json::json!({"n":4})))
    );
    Ok(())
}
#[tokio::test]
async fn job_validates_before_dedupe_and_enforces_concurrency() -> pi_rs_chord::Result<()> {
    let ctx = Context::background();
    let gate = Arc::new(tokio::sync::Notify::new());
    let opened = gate.clone();
    let job = Job::new(
        definition()?,
        JobImplementation::new(move |args, _| {
            let gate = opened.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(args)
            })
        }),
        Batch::new(|_| {}),
        None,
    )
    .await?;
    job.live()?;
    let run = job
        .start(JsonValue::Number(2.0), Some("same".into()), &ctx)
        .await?;
    let duplicate = job
        .start(JsonValue::Number(9.0), Some("same".into()), &ctx)
        .await?;
    assert_eq!(run.id, duplicate.id);
    assert!(
        job.start(JsonValue::Null, Some("same".into()), &ctx)
            .await
            .is_err()
    );
    assert!(job.start(JsonValue::Number(3.0), None, &ctx).await.is_err());
    tokio::task::yield_now().await;
    gate.notify_one();
    assert!(matches!(
        run.outcome(&ctx).await?.outcome,
        JobResult::Done {
            result: JsonValue::Number(2.0)
        }
    ));
    assert!(job.outcome(&run.id, &ctx).await.is_err());
    Ok(())
}
#[tokio::test]
async fn cancelling_an_outcome_wait_does_not_cancel_the_job() -> pi_rs_chord::Result<()> {
    let ctx = Context::background();
    let gate = Arc::new(tokio::sync::Notify::new());
    let opened = gate.clone();
    let job = Job::new(
        definition()?,
        JobImplementation::new(move |args, _| {
            let gate = opened.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(args)
            })
        }),
        Batch::new(|_| {}),
        None,
    )
    .await?;
    job.live()?;
    let run = job.start(JsonValue::Number(1.0), None, &ctx).await?;
    let (wait, cancel) = ctx.with_cancel();
    cancel.cancel("caller left");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), run.outcome(&wait))
            .await
            .unwrap()
            .is_err()
    );
    tokio::task::yield_now().await;
    gate.notify_one();
    assert!(matches!(
        run.outcome(&ctx).await?.outcome,
        JobResult::Done { .. }
    ));
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn failed_finishes_retry_five_times_then_block_without_losing_the_key()
-> pi_rs_chord::Result<()> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let implementation = JobImplementation::new(|args, _| Box::pin(async move { Ok(args) }))
        .with_finish(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(pi_rs_chord::Error::Invalid("finish failed".into())) })
        });
    let job = Job::new(definition()?, implementation, Batch::new(|_| {}), None).await?;
    job.live()?;
    let ctx = Context::background();
    let run = job
        .start(JsonValue::Number(1.0), Some("key".into()), &ctx)
        .await?;
    for _ in 0..10 {
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 5);
    assert_eq!(job.running()[0].phase, Phase::Blocked);
    assert_eq!(
        job.start(JsonValue::Number(2.0), Some("key".into()), &ctx)
            .await?
            .id,
        run.id
    );
    assert_eq!(job.abort(&run.id, &ctx).await?, AbortStatus::Marked);
    assert!(run.outcome(&ctx).await.is_err());
    Ok(())
}
#[tokio::test]
async fn abort_signals_the_run_and_state_is_fenced_after_settlement() -> pi_rs_chord::Result<()> {
    let ctx = Context::background();
    let state = Arc::new(std::sync::Mutex::new(None));
    let captured = state.clone();
    let job = Job::new(
        JobDefinition {
            state: Some(Arc::new(pi_rs_chord::documents::Definition::new(
                "progress",
                pi_rs_chord::contracts::schema::obj::<&str>([]),
                1,
                || Ok(JsonValue::Object(Default::default())),
            )?)),
            ..definition()?
        },
        JobImplementation::new(move |_, ctx| {
            *captured.lock().unwrap() = Some(ctx.clone());
            Box::pin(async move {
                if let Some(signal) = ctx.context.abort_signal() {
                    signal.cancelled().await;
                }
                Err(pi_rs_chord::Error::Invalid("cancelled".into()))
            })
        }),
        Batch::new(|_| {}),
        None,
    )
    .await?;
    job.live()?;
    let run = job.start(JsonValue::Number(1.0), None, &ctx).await?;
    tokio::task::yield_now().await;
    job.abort(&run.id, &ctx).await?;
    assert!(matches!(
        run.outcome(&ctx).await?.outcome,
        JobResult::Failed {
            error: JobError::Aborted { .. }
        }
    ));
    let captured = state.lock().unwrap().clone().unwrap();
    assert!(captured.change_state(|_| Ok(())).await.is_err());
    Ok(())
}

#[tokio::test]
async fn durable_restart_finishes_committed_outcomes_without_rerunning_work()
-> pi_rs_chord::Result<()> {
    let engine = pi_rs_chord::store::StoreEngine::open(
        pi_rs_chord::sqlite::SqliteDatabase::open(":memory:", Default::default()).await?,
    )
    .await?;
    let store = JobStore::new(engine.clone(), "owner")?;
    assert!(JobStore::new(engine.clone(), "owner").is_err());
    let definition = define_job(
        "test/work",
        JobOptions {
            persist: Some(Resume::Rerun),
            ..JobOptions::new(
                1,
                JsonValue::parse("{\"type\":\"integer\"}")?,
                JsonValue::Bool(true),
            )
        },
    )?;
    let stored = StoredRun {
        version: 1,
        fingerprint: definition.fingerprint.clone(),
        args: JsonValue::Number(7.0),
        key: Some("key".into()),
        phase: Phase::Finishing,
        restarts: 0,
        finish_failures: 0,
        abort_marked: false,
        blocked: None,
        outcome: Some(JobResult::Done {
            result: JsonValue::Number(9.0),
        }),
    };
    store.save("test/work", "saved", &stored, None).await?;
    assert_eq!(store.load("test/work").await?[0].1, stored);
    let ran = Arc::new(AtomicUsize::new(0));
    let runs = ran.clone();
    let finished = Arc::new(AtomicUsize::new(0));
    let finishes = finished.clone();
    let job = Job::new(
        definition,
        JobImplementation::new(move |args, _| {
            runs.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(args) })
        })
        .with_finish(move |outcome| {
            assert!(matches!(
                outcome.outcome,
                JobResult::Done {
                    result: JsonValue::Number(9.0)
                }
            ));
            finishes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }),
        engine.batch(),
        Some(store.clone()),
    )
    .await?;
    job.live()?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        job.outcome("saved", &Context::background()),
    )
    .await
    .unwrap()?;
    assert_eq!(result.id, "saved");
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(finished.load(Ordering::SeqCst), 1);
    assert!(store.load("test/work").await?.is_empty());
    job.close("closed").await?;
    drop(job);
    drop(store);
    assert!(JobStore::new(engine, "owner").is_ok());
    Ok(())
}

#[tokio::test]
async fn durable_crash_limit_and_incompatible_versions_block_or_finish_without_work()
-> pi_rs_chord::Result<()> {
    let engine = pi_rs_chord::store::StoreEngine::open(
        pi_rs_chord::sqlite::SqliteDatabase::open(":memory:", Default::default()).await?,
    )
    .await?;
    let store = JobStore::new(engine.clone(), "owner")?;
    let definition = define_job(
        "test/work",
        JobOptions {
            persist: Some(Resume::Rerun),
            ..JobOptions::new(
                1,
                JsonValue::parse("{\"type\":\"integer\"}")?,
                JsonValue::Bool(true),
            )
        },
    )?;
    let mut stored = StoredRun {
        version: 1,
        fingerprint: definition.fingerprint.clone(),
        args: JsonValue::Number(7.0),
        key: None,
        phase: Phase::Running,
        restarts: 4,
        finish_failures: 0,
        abort_marked: false,
        blocked: None,
        outcome: None,
    };
    store.save("test/work", "crashed", &stored, None).await?;
    stored.fingerprint = "incompatible".into();
    store.save("test/work", "mismatch", &stored, None).await?;
    let job = Job::new(
        definition,
        JobImplementation::new(|_, _| {
            Box::pin(async { Err(pi_rs_chord::Error::Invalid("work must not rerun".into())) })
        }),
        engine.batch(),
        Some(store.clone()),
    )
    .await?;
    assert_eq!(
        job.running()
            .iter()
            .find(|r| r.id == "mismatch")
            .unwrap()
            .reason
            .as_deref(),
        Some("version")
    );
    job.live()?;
    assert!(matches!(
        job.outcome("crashed", &Context::background())
            .await?
            .outcome,
        JobResult::Failed {
            error: JobError::RestartLimit { restarts: 5 }
        }
    ));
    job.abort("mismatch", &Context::background()).await?;
    assert!(store.load("test/work").await?.is_empty());
    job.close("closed").await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn retry_blocked_commits_finish_writes_and_job_retirement_together() -> pi_rs_chord::Result<()>
{
    use pi_rs_chord::store::{DocumentBase, DocumentHistory, DocumentRecord, StoreWrite};
    let engine = pi_rs_chord::store::StoreEngine::open(
        pi_rs_chord::sqlite::SqliteDatabase::open(":memory:", Default::default()).await?,
    )
    .await?;
    let store = JobStore::new(engine.clone(), "owner")?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let job = Job::new(
        JobDefinition {
            persist: Some(Resume::Rerun),
            ..definition()?
        },
        JobImplementation::new(|args, _| Box::pin(async move { Ok(args) }))
            .with_transactional_finish(move |outcome, step| {
                let id = step.mint()?;
                step.apply(StoreWrite::DocumentCreate {
                    record: DocumentRecord {
                        id,
                        owner: "owner".into(),
                        namespace: "results".into(),
                        kind: "done".into(),
                        key: Some(outcome.id.clone()),
                        history: DocumentHistory::None,
                        created_at: step.seq,
                        retired_at: None,
                        meta: None,
                        host: None,
                        revision: 1,
                        version: 1,
                        fingerprint: "result".into(),
                    },
                    base: Some(DocumentBase {
                        version: 1,
                        fingerprint: "result".into(),
                        value: outcome.args.clone(),
                    }),
                })?;
                if calls.fetch_add(1, Ordering::SeqCst) < 5 {
                    return Err(pi_rs_chord::Error::Invalid("retry".into()));
                }
                Ok(())
            }),
        engine.batch(),
        Some(store.clone()),
    )
    .await?;
    job.live()?;
    let ctx = Context::background();
    let run = job.start(JsonValue::Number(9.0), None, &ctx).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !job.running().iter().any(|r| r.phase == Phase::Blocked) {
        assert!(
            !job.running().is_empty(),
            "run retired despite failing finish"
        );
        assert!(std::time::Instant::now() < deadline, "run never blocked");
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
    }
    let db = engine.database();
    let count = db
        .call(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM chord_documents WHERE namespace='results'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await?;
    assert_eq!(count, 0);
    job.retry_blocked(&ctx).await?;
    assert_eq!(run.outcome(&ctx).await?.args, JsonValue::Number(9.0));
    let same_commit = db.call(|c| {
        Ok(c.query_row(
            "SELECT a.created_at = m.last_seq AND NOT EXISTS (
                 SELECT 1 FROM chord_documents b WHERE b.namespace = ? AND b.key = a.key
             ) FROM chord_documents a CROSS JOIN chord_meta m WHERE a.namespace = ? AND m.singleton = 1",
            ["chord.jobs", "results"], |row| row.get::<_, bool>(0),
        )?)
    }).await?;
    assert!(same_commit);
    Ok(())
}

#[tokio::test]
async fn restored_run_migrates_stored_state_before_finishing() -> pi_rs_chord::Result<()> {
    use pi_rs_chord::{contracts::schema::*, documents::Definition};
    let engine = pi_rs_chord::store::StoreEngine::open(
        pi_rs_chord::sqlite::SqliteDatabase::open(":memory:", Default::default()).await?,
    )
    .await?;
    let store = JobStore::new(engine.clone(), "owner")?;
    let definition = define_job(
        "migrating",
        JobOptions {
            persist: Some(Resume::Rerun),
            state: Some(Arc::new(
                Definition::new("state", obj([("next", required(int()))]), 2, || {
                    JsonValue::parse(r#"{"next":0}"#)
                })?
                .persisted(true, |_, _, _| false)
                .migrating(|old, from| {
                    assert_eq!(from, 1);
                    assert_eq!(old, JsonValue::parse(r#"{"before":3}"#)?);
                    JsonValue::parse(r#"{"next":4}"#)
                }),
            )),
            ..JobOptions::new(1, int(), int())
        },
    )?;
    store
        .save(
            "migrating",
            "saved",
            &StoredRun {
                version: 1,
                fingerprint: definition.fingerprint.clone(),
                args: JsonValue::Number(1.0),
                key: None,
                phase: Phase::Finishing,
                restarts: 0,
                finish_failures: 0,
                abort_marked: false,
                blocked: None,
                outcome: Some(JobResult::Done {
                    result: JsonValue::Number(1.0),
                }),
            },
            Some(&JsonValue::parse(r#"{"before":0}"#)?),
        )
        .await?;
    let state_id = engine
        .database()
        .call(|c| {
            Ok(c.query_row(
                "SELECT id FROM chord_documents WHERE namespace='chord.job-states' AND key='saved'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await?;
    engine
        .commit(vec![pi_rs_chord::store::StoreWrite::DocumentChange {
            id: state_id,
            revision: 1,
            content: pi_rs_chord::store::DocumentContent::Delta {
                version: 1,
                ops: vec![
                    pi_rs_chord::delta::Op::Replace(JsonValue::parse(r#"{"before":3}"#)?).to_json(),
                ],
            },
        }])
        .await?;
    let job = Job::new(
        definition,
        JobImplementation::new(|args, _| Box::pin(async move { Ok(args) })),
        engine.batch(),
        Some(store),
    )
    .await?;
    job.live()?;
    let outcome = job.outcome("saved", &Context::background()).await?;
    assert_eq!(outcome.state, Some(JsonValue::parse(r#"{"next":4}"#)?));
    Ok(())
}
