use super::LIVE_DOC;
use crate::{
    ConversationId, EntryId, SubmissionId, SubmissionSettlement, TaskId,
    documents::resolve_address,
    session::{Result, Transaction},
};
use pi_rs_chord::delta::Segment;
use serde_json::{Value, json};

pub enum RunEnd {
    Done {
        answer: EntryId,
    },
    Unanswered {
        reason: String,
        detail: Option<String>,
        settlement: Option<SubmissionSettlement>,
    },
}
pub async fn end_run(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    task: TaskId,
    end: RunEnd,
) -> Result<()> {
    let address = resolve_address(&LIVE_DOC, Some(conversation), None)?;
    let live = tx.doc(&LIVE_DOC, &address, None).await?;
    let state = serde_json::to_value(live.value()?.to_json())?;
    let mut settle = None;
    if let Some(run) = state
        .get("run")
        .filter(|run| run["taskId"].as_u64() == Some(task))
    {
        let inputs: Vec<SubmissionId> = serde_json::from_value(run["inputs"].clone())?;
        match end {
            RunEnd::Done { answer } => {
                settle = Some((inputs, SubmissionSettlement::Done { answer }))
            }
            RunEnd::Unanswered {
                reason,
                detail,
                settlement,
            } if run.get("afterRun").is_none() => {
                let mut failure = json!({"taskId":task,"inputs":inputs,"reason":reason});
                if let Some(first) = run.get("first") {
                    failure["first"] = first.clone();
                }
                if let Some(detail) = &detail {
                    failure["detail"] = json!(detail);
                }
                live.set(&[Segment::Key("runFailure".into())], failure.into())?;
                settle = Some((
                    inputs,
                    settlement.unwrap_or_else(|| SubmissionSettlement::Unanswered {
                        reason,
                        detail: detail.map(Value::String),
                    }),
                ));
            }
            _ => {}
        }
        live.delete(&[Segment::Key("run".into())])?;
    }
    for key in ["generation", "tools", "nestedTools"] {
        if state.get(key).is_some() {
            live.delete(&[Segment::Key(key.into())])?;
        }
    }
    if let Some((inputs, settlement)) = settle {
        for id in inputs {
            tx.settle_submission(id, settlement.clone())?;
        }
    }
    Ok(())
}

pub(super) async fn settle_scheduler_outcome(
    tx: &mut Transaction<'_>,
    record: &crate::TaskRecord,
    outcome: &crate::TaskOutcome,
) -> Result<()> {
    let (kind, message) = match outcome {
        crate::TaskOutcome::Faulted { error } => ("faulted", error.message.clone()),
        crate::TaskOutcome::Orphaned { reason } => ("orphaned", reason.clone()),
        _ => return Ok(()),
    };
    let address = resolve_address(&LIVE_DOC, Some(record.conversation_id), None)?;
    let live = tx.doc(&LIVE_DOC, &address, None).await?;
    let state = serde_json::to_value(live.value()?.to_json())?;
    match record.kind.as_str() {
        "pi.generation" if state["run"]["taskId"] == record.id => {
            if let Some(answer) = state["run"]["yield"]["answer"].as_u64() {
                return end_run(
                    tx,
                    record.conversation_id,
                    record.id,
                    RunEnd::Done { answer },
                )
                .await;
            }
            super::convert_partial(tx, record.conversation_id).await?;
            end_run(
                tx,
                record.conversation_id,
                record.id,
                RunEnd::Unanswered {
                    reason: kind.into(),
                    detail: Some(message.clone()),
                    settlement: (kind == "orphaned").then_some(SubmissionSettlement::Unanswered {
                        reason: message,
                        detail: None,
                    }),
                },
            )
            .await?;
        }
        "pi.compaction" => {
            if let Some(items) = state["compactions"].as_array() {
                let mut items = items.clone();
                if let Some(index) = items.iter().position(|v| v["taskId"] == record.id) {
                    let mut failure = items.remove(index);
                    failure["error"] = json!(message);
                    live.set(&[Segment::Key("compactionFailure".into())], failure.into())?;
                    live.set(&[Segment::Key("compactions".into())], json!(items).into())?;
                }
            }
        }
        "pi.tool" => {
            let mut removed = std::collections::HashSet::from([record.id]);
            for key in ["tools", "nestedTools"] {
                if let Some(items) = state[key].as_array() {
                    let mut items = items.clone();
                    items.retain_mut(|item| {
                        if item["parentTaskId"]
                            .as_u64()
                            .is_some_and(|id| removed.contains(&id))
                        {
                            if let Some(id) = item["taskId"].as_u64() {
                                removed.insert(id);
                            }
                            return false;
                        }
                        if item["taskId"] == record.id {
                            item["status"] = json!("done");
                        }
                        true
                    });
                    if state[key].as_array() != Some(&items) {
                        live.set(&[Segment::Key(key.into())], json!(items).into())?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}
