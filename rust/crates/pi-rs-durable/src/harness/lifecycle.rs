use super::{
    Agent, Boundary, Invocation, LIVE_DOC, RunEnd, apply_boundary, end_run, generation::start_run,
};
use crate::{
    EntryId, JoinPolicy, SubmissionSettlement, TaskId, TaskOutcome, TaskOwnership, TaskState,
    documents::resolve_address,
    session::{Error, Result, TaskOptions, Transaction},
};
use pi_rs_chord::{context::Context, delta::Segment};
use serde_json::{Value, json};

impl Invocation {
    pub(super) async fn hook_decisions(
        &self,
        agent: &Agent,
        event: &str,
        input: Value,
        ctx: &Context,
    ) -> Result<Vec<Value>> {
        let mut decisions = Vec::new();
        for hook in agent.hooks("pi.generation", event) {
            match hook(input.clone(), self, ctx).await {
                Ok(Some(decision)) => {
                    decisions.push(decision);
                    if event == "onYield" {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    ctx.check()?;
                    self.harness
                        .session
                        .harness_report(crate::session::ReportKind::Extension, &error.to_string());
                }
            }
        }
        Ok(decisions)
    }
    pub(super) async fn hook_children(
        &self,
        tx: &mut Transaction<'_>,
        decisions: &[Value],
    ) -> Result<Vec<TaskId>> {
        let registry = self.harness.registry.snapshot();
        let mut children = Vec::new();
        for request in decisions
            .iter()
            .flat_map(|v| v["tasks"].as_array().into_iter().flatten())
        {
            let Some(name) = request["task"].as_str() else {
                continue;
            };
            let Some(task) = registry.task(name).filter(|_| !name.starts_with("pi.")) else {
                continue;
            };
            let options = TaskOptions::new(TaskOwnership::Task {
                task_id: self.record.id,
            });
            children.push(
                tx.create_task(task, request["input"].clone(), options)
                    .await?,
            );
        }
        Ok(children)
    }
    pub(super) async fn conclude_answer(
        &self,
        tx: &mut Transaction<'_>,
        boundary: Boundary,
        answer: EntryId,
        continuation: Option<Value>,
        after_run: bool,
    ) -> Result<TaskState> {
        let conversation = self.record.conversation_id;
        let id = self.record.id;
        let live = resolve_address(&LIVE_DOC, Some(conversation), None)?;
        let state = serde_json::to_value(tx.doc(&LIVE_DOC, &live, None).await?.value()?.to_json())?;
        let inbox = resolve_address(&super::INBOX_DOC, Some(conversation), None)?;
        let queued = serde_json::to_value(
            tx.doc(&super::INBOX_DOC, &inbox, None)
                .await?
                .value()?
                .to_json(),
        )?;
        let continuation = continuation.filter(|_| {
            !queued["items"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["mode"] != "write" || v["entry"]["head"] == "self")
        });
        if state["run"]["taskId"] == id && after_run && continuation.is_none() {
            let mut run = state["run"].clone();
            for input in serde_json::from_value::<Vec<u64>>(run["inputs"].clone())? {
                tx.settle_submission(input, SubmissionSettlement::Done { answer })?;
            }
            run["inputs"] = json!([]);
            run["afterRun"] = json!({});
            if let Some(obj) = run.as_object_mut() {
                obj.remove("yield");
            }
            let doc = tx.doc(&LIVE_DOC, &live, None).await?;
            doc.set(&[Segment::Key("run".into())], run.clone().into())?;
            for key in ["generation", "tools", "nestedTools"] {
                if state.get(key).is_some() {
                    doc.delete(&[Segment::Key(key.into())])?;
                }
            }
            return Ok(TaskState::Running {
                checkpoint: json!({"phase":"finish","run":{"reason":"answer","answer":answer,"last":answer,"first":run["first"].as_u64().unwrap_or(answer)}}),
                wake: None,
            });
        }
        let now = (self.harness.now)();
        let placed = apply_boundary(tx, boundary, true, now).await?;
        if let Some(input) = continuation {
            super::append_input(tx, conversation, input, None, now).await?;
            let next = tx
                .create_task(super::generation_task().as_ref(), json!({}), {
                    let mut options = TaskOptions::new(TaskOwnership::Conversation);
                    options.conversation_id = Some(conversation);
                    options
                })
                .await?;
            let doc = tx.doc(&LIVE_DOC, &live, None).await?;
            let mut run = state["run"].clone();
            if let Some(fields) = run.as_object_mut() {
                fields.remove("yield");
            }
            run["taskId"] = json!(next);
            run["turns"] = json!(0);
            doc.set(&[Segment::Key("run".into())], run.into())?;
            if state.get("generation").is_some() {
                doc.delete(&[Segment::Key("generation".into())])?;
            }
        } else {
            end_run(tx, conversation, id, RunEnd::Done { answer }).await?;
            if !placed.users.is_empty() {
                start_run(tx, conversation, placed.users, placed.first).await?;
            }
        }
        Ok(TaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: json!({"entryId":answer}),
            },
        })
    }
    pub(super) async fn finish_answer(
        &self,
        checkpoint: &Value,
        agent: &Agent,
        settings: &Value,
        ctx: &Context,
    ) -> Result<()> {
        let phase = checkpoint["phase"].as_str();
        let answer = checkpoint["answer"]
            .as_u64()
            .or_else(|| checkpoint["run"]["answer"].as_u64())
            .ok_or_else(|| Error::invalid("Missing answer entry"))?;
        let decisions = if phase == Some("finish") {
            self.hook_decisions(agent, "afterRun", checkpoint["run"].clone(), ctx)
                .await?
        } else {
            vec![]
        };
        self.commit(ctx, async |tx, _| {
            let boundary = super::prepare_boundary(
                tx,
                self.record.conversation_id,
                settings["steeringMode"] == "all",
                settings["followUpMode"] == "all",
            )
            .await?;
            let mut continuation = None;
            if phase == Some("yield") {
                for child in serde_json::from_value::<Vec<u64>>(checkpoint["tasks"].clone())? {
                    if let Some(task) = tx.task(child).await?
                        && let TaskState::Terminal {
                            outcome: TaskOutcome::Completed { result },
                        } = &task.state
                        && let Some(input) = result.get("continue")
                    {
                        continuation = Some(input.clone());
                        break;
                    }
                }
            }
            let children = self.hook_children(tx, &decisions).await?;
            if !children.is_empty() {
                let live = resolve_address(&LIVE_DOC, Some(self.record.conversation_id), None)?;
                tx.doc(&LIVE_DOC, &live, None).await?.set(
                    &[
                        Segment::Key("run".into()),
                        Segment::Key("afterRun".into()),
                        Segment::Key("tasks".into()),
                    ],
                    json!(children).into(),
                )?;
                return Ok(TaskState::Waiting {
                    checkpoint: json!({"phase":"drain","answer":answer,"tasks":children}),
                    on: children,
                    policy: JoinPolicy::AllSettled,
                });
            }
            self.conclude_answer(
                tx,
                boundary,
                answer,
                continuation,
                phase == Some("yield") && !agent.hooks("pi.generation", "afterRun").is_empty(),
            )
            .await
        })
        .await
    }
}
