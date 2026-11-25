use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use pi_rs_chord::context::Context;
use pi_rs_durable::{
    Extension, Harness, HarnessOptions, HarnessRegistry, Invocation, TaskOutcome, TaskOwnership,
    TaskState, create_registry,
    session::{Error, TaskOptions},
    storage::sqlite::SqliteStorage,
    tasks::{PhaseHandler, TaskDefinition, define_task},
};
use pi_rs_telemetry::{
    AttributeValue, Context as Recorder, SpanOptions, SpanStatus, TelemetryContext,
};
use serde_json::{Value, json};

fn write_draft() -> Result<pi_rs_durable::SubmissionDraft, Error> {
    Ok(serde_json::from_value(json!({
        "type": "write",
        "entry": { "kind": "test.note", "data": {"text":"private-entry-content"} }
    }))?)
}

#[tokio::test]
async fn write_and_provisional_admission_publish_only_committed_events()
-> Result<(), Box<dyn std::error::Error>> {
    let telemetry = Recorder::new();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("telemetry.sqlite");
    let ctx = Context::background();
    let harness = Harness::open_with(
        Arc::new(SqliteStorage::open(&path)?),
        HarnessOptions {
            telemetry: telemetry.clone(),
            ..Default::default()
        },
        &ctx,
    )
    .await?;
    let root = harness.root(&ctx).await?;
    telemetry
        .start_span(SpanOptions::new("caller"), |caller| async {
            let ctx = ctx.with_telemetry(caller);
            let first = root.submit(write_draft()?, &ctx).await?;
            let provisional = root
                .commit(
                    async |tx| {
                        root.admit(tx, write_draft()?).await?;
                        Err::<(), _>(Error::invalid("private-rollback-message"))
                    },
                    &ctx,
                    Default::default(),
                )
                .await;
            assert!(provisional.is_err());
            let second = root
                .commit(
                    async |tx| root.admit(tx, write_draft()?).await,
                    &ctx,
                    Default::default(),
                )
                .await?;
            assert_ne!(first.id, second);

            // Refuse a real storage write after the transaction has staged both records.
            rusqlite::Connection::open(&path)
                .and_then(|connection| {
                    connection.execute_batch(
                        "CREATE TRIGGER refuse_entry BEFORE INSERT ON entries BEGIN
             SELECT RAISE(ABORT, 'private-storage-error'); END;",
                    )
                })
                .map_err(|error| Error::Callback(Box::new(error)))?;
            assert!(root.submit(write_draft()?, &ctx).await.is_err());
            Ok::<_, Error>(())
        })
        .await?;
    let _ = harness.close(&ctx).await;
    assert!(matches!(
        harness.session.closed().await,
        pi_rs_durable::session::SessionEnd::Failed(_)
    ));

    let spans = telemetry.get_spans();
    let caller = &spans[0];
    let writes = &spans[1..];
    assert_eq!(writes.len(), 4);
    assert!(writes.iter().all(|span| span.name == "pi.durable.submit"
        && span.parent_context.as_ref() == Some(&caller.context)
        && span.settled));
    for span in [caller, &writes[0]] {
        assert_eq!(
            span.events
                .iter()
                .map(|event| event.name.as_str())
                .collect::<Vec<_>>(),
            ["pi.durable.entry", "pi.durable.submission"]
        );
    }
    assert!(writes[1..3].iter().all(|span| span.events.is_empty()));
    assert_eq!(writes[3].events.len(), 1);
    assert_eq!(writes[3].events[0].name, "pi.durable.report");
    assert_eq!(
        writes[3].events[0].attributes,
        [("type".into(), "storage_failed".into())].into()
    );
    assert_eq!(writes[3].status, SpanStatus::Error { error: None });
    assert!(!format!("{spans:?}").contains("private-"));
    let metrics = telemetry.get_metrics();
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].name, "pi.durable.reports");
    assert_eq!(metrics[0].value, 1.0);
    assert_eq!(
        metrics[0].attributes,
        [("type".into(), "storage_failed".into())].into()
    );
    Ok(())
}

#[tokio::test]
async fn task_phases_measure_fault_abort_and_close_without_recording_payloads()
-> Result<(), Box<dyn std::error::Error>> {
    let telemetry = Recorder::new();
    let reached = Arc::new(tokio::sync::Semaphore::new(0));
    let guard = reached.clone();
    let recover = Arc::new(AtomicBool::new(false));
    let recover_phase = recover.clone();
    let start: PhaseHandler<Invocation> =
        Arc::new(move |record, task, ctx| {
            let reached = reached.clone();
            let recover = recover_phase.clone();
            Box::pin(async move {
                if record.input == "fault" {
                    task.commit(ctx, async |_, _| Ok(TaskState::Running {
                        checkpoint: json!({"phase":"fail","attempt":2,"data":"private-checkpoint"}),
                        wake: None,
                    })).await?;
                } else if recover.load(Ordering::Relaxed) {
                    task.commit(ctx, async |_, _| {
                        Ok(TaskState::Terminal {
                            outcome: TaskOutcome::Completed {
                                result: Value::Null,
                            },
                        })
                    })
                    .await?;
                } else {
                    reached.add_permits(1);
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        });
    let fail: PhaseHandler<Invocation> = Arc::new(|_, _, _| {
        Box::pin(async { Err(std::io::Error::other("private-phase-error").into()) })
    });
    let phases = [("start".into(), start), ("fail".into(), fail)].into();
    let task = Arc::new(define_task(TaskDefinition {
        name: "test.phase".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json!({"phase":"start"}))),
        phases,
        abort: Arc::new(|_, task: &Invocation, ctx| {
            Box::pin(async move {
                task.commit(ctx, async |_, _| {
                    Ok(TaskState::Terminal {
                        outcome: TaskOutcome::Aborted {
                            reason: None,
                            result: None,
                        },
                    })
                })
                .await?;
                Ok(())
            })
        }),
        migrate: None,
        hooks: (),
    }));
    let registry = create_registry();
    registry.install(Extension {
        name: "test.telemetry".into(),
        tasks: vec![task.clone()],
        ..Default::default()
    })?;
    let ctx = Context::background();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("phases.sqlite");
    let registry = Arc::new(HarnessRegistry::Local(registry));
    let harness = Harness::open_with(
        Arc::new(SqliteStorage::open(&path)?),
        HarnessOptions {
            telemetry: telemetry.clone(),
            registry: registry.clone(),
            ..Default::default()
        },
        &ctx,
    )
    .await?;
    let root = harness.root(&ctx).await?;
    let ids = telemetry
        .start_span(SpanOptions::new("creator"), |creator| async {
            root.commit(
                async |tx| {
                    let mut ids = Vec::new();
                    for input in [json!("fault"), Value::Null, Value::Null] {
                        ids.push(
                            tx.create_task(
                                &task,
                                input,
                                TaskOptions::new(TaskOwnership::Conversation),
                            )
                            .await?,
                        );
                    }
                    Ok(ids)
                },
                &ctx.with_telemetry(creator),
                Default::default(),
            )
            .await
        })
        .await?;
    harness.resume()?;
    let _permits = tokio::time::timeout(Duration::from_secs(10), guard.acquire_many(2)).await??;
    harness.abort_task(ids[1], &ctx).await?;
    let faulted =
        tokio::time::timeout(Duration::from_secs(10), harness.wait_for_task(ids[0], &ctx))
            .await??;
    assert!(matches!(
        faulted.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Faulted { .. }
        }
    ));
    let aborted =
        tokio::time::timeout(Duration::from_secs(10), harness.wait_for_task(ids[1], &ctx))
            .await??;
    assert!(matches!(
        aborted.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    harness.close(&ctx).await?;

    let spans = telemetry.get_spans();
    let phases = &spans[1..];
    assert_eq!(phases.len(), 5);
    assert!(
        phases
            .iter()
            .all(|phase| phase.settled && phase.parent_context.as_ref() == Some(&spans[0].context))
    );
    assert_eq!(
        phases
            .iter()
            .filter(
                |phase| phase.attributes["outcome"] == AttributeValue::from("aborted")
                    && phase.status == SpanStatus::Ok
            )
            .count(),
        3
    );
    let fault = phases
        .iter()
        .find(|span| span.attributes["phase"] == AttributeValue::from("fail"))
        .ok_or("missing fault phase")?;
    assert_eq!(fault.attributes["attempt"], AttributeValue::from(2.0));
    assert!(
        matches!(&fault.status, SpanStatus::Error { error: Some(error) } if error.name == "task_faulted")
    );
    assert!(
        fault
            .events
            .iter()
            .any(|event| event.name == "pi.durable.report")
    );
    let checkpoints: Vec<_> = phases
        .iter()
        .flat_map(|span| &span.events)
        .filter(|event| event.name == "pi.durable.checkpoint")
        .collect();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(
        checkpoints[0].attributes["phase"],
        AttributeValue::from("fail")
    );
    assert!(!format!("{spans:?}").contains("private-"));
    let metrics = telemetry.get_metrics();
    let active: Vec<_> = metrics
        .iter()
        .filter(|metric| metric.name == "pi.durable.tasks.active")
        .collect();
    assert_eq!(active.len(), 6);
    assert_eq!(active.iter().map(|metric| metric.value).sum::<f64>(), 0.0);
    let durations: Vec<_> = metrics
        .iter()
        .filter(|metric| metric.name == "pi.durable.phase.duration")
        .collect();
    assert_eq!(durations.len(), phases.len());
    assert!(durations.iter().all(|metric| {
        metric.unit.as_deref() == Some("ms")
            && metric.value >= 0.0
            && metric.attributes.len() == 3
            && phases
                .iter()
                .any(|phase| Some(&phase.context) == metric.context.as_ref())
    }));
    let reports: Vec<_> = metrics
        .iter()
        .filter(|metric| metric.name == "pi.durable.reports")
        .collect();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].attributes,
        [("type".into(), "task_faulted".into())].into()
    );
    recover.store(true, Ordering::Relaxed);
    let reopened_telemetry = Recorder::new();
    let reopened = Harness::open_with(
        Arc::new(SqliteStorage::open(&path)?),
        HarnessOptions {
            telemetry: reopened_telemetry.clone(),
            registry,
            ..Default::default()
        },
        &ctx,
    )
    .await?;
    let completed = tokio::time::timeout(
        Duration::from_secs(10),
        reopened.wait_for_task(ids[2], &ctx),
    )
    .await??;
    assert!(matches!(
        completed.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    reopened.wait_for_idle(&ctx).await?;
    reopened.close(&ctx).await?;
    let restored = reopened_telemetry.get_spans();
    assert_eq!(restored.len(), 1);
    assert!(restored[0].settled);
    assert_eq!(restored[0].parent_context.as_ref(), Some(&spans[0].context));
    assert_eq!(
        restored[0].attributes["resumed"],
        AttributeValue::from(true)
    );
    assert_eq!(
        restored[0].attributes["outcome"],
        AttributeValue::from("done")
    );
    let metrics = reopened_telemetry.get_metrics();
    assert_eq!(
        metrics
            .iter()
            .filter(|metric| metric.name == "pi.durable.tasks.active")
            .map(|metric| metric.value)
            .collect::<Vec<_>>(),
        [1.0, -1.0]
    );
    Ok(())
}
