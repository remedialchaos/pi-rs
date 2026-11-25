use super::{
    Harness, INBOX_DOC,
    telemetry::{OpenSpan, link},
};
use crate::{
    CommitChange, ConversationId, Session, SubmissionId, SubmissionRecord, SubmissionSettlement,
    SubmissionStatus,
    documents::resolve_address,
    session::{Error, ObservedCommit, Result},
    signals::is_settled,
};
use pi_rs_chord::{context::Context, delta::Segment};
use pi_rs_telemetry::{Context as TraceContext, SpanOptions, SpanStatus, TelemetrySpan};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortSubmissionResult {
    Aborted,
    AlreadyPlaced,
    Settled,
    NotFound,
}

#[derive(Clone)]
pub struct Submission {
    pub id: SubmissionId,
    harness: Harness,
}
impl Harness {
    pub async fn submission(&self, id: SubmissionId, ctx: &Context) -> Result<Option<Submission>> {
        Ok(self.session.submission(id, ctx).await?.map(|_| Submission {
            id,
            harness: self.clone(),
        }))
    }
    pub async fn abort_submission(
        &self,
        id: SubmissionId,
        conversation: Option<ConversationId>,
        ctx: &Context,
    ) -> Result<AbortSubmissionResult> {
        abort(&self.session, id, conversation, ctx).await
    }
}
impl Submission {
    pub async fn status(&self, ctx: &Context) -> Result<Arc<SubmissionRecord>> {
        self.harness
            .session
            .submission(self.id, ctx)
            .await?
            .ok_or_else(|| Error::invalid(format!("Submission {} does not exist", self.id)))
    }
    pub async fn abort(&self, ctx: &Context) -> Result<AbortSubmissionResult> {
        match abort(&self.harness.session, self.id, None, ctx).await? {
            AbortSubmissionResult::NotFound => Err(Error::invalid(format!(
                "Submission {} does not exist",
                self.id
            ))),
            result => Ok(result),
        }
    }
    pub async fn wait(&self, ctx: &Context) -> Result<Arc<SubmissionRecord>> {
        self.harness.resume()?;
        let notify = Arc::new(Notify::new());
        let signal = notify.clone();
        let id = self.id;
        let _subscription=self.harness.session.follow_commits(move |change,_|{
            if let ObservedCommit::Stored(commit)=change && commit.changes.iter().any(|c|matches!(c,CommitChange::Submission{value} if value.id==id && is_settled(value))){
                signal.notify_one();
            }
            Ok(())
        })?;
        let closed = notify.clone();
        let _close = self
            .harness
            .session
            .subscribe_close(move || closed.notify_one())?;
        loop {
            let record = self.status(ctx).await?;
            if is_settled(&record) {
                return Ok(record);
            }
            ctx.await_with(notify.notified()).await?;
        }
    }
}

async fn abort(
    session: &Session,
    id: SubmissionId,
    conversation: Option<ConversationId>,
    ctx: &Context,
) -> Result<AbortSubmissionResult> {
    session
        .commit(
            async |tx| {
                let Some(record) = tx.submission(id).await? else {
                    return Ok(AbortSubmissionResult::NotFound);
                };
                if conversation.is_some_and(|c| c != record.conversation_id) {
                    return Ok(AbortSubmissionResult::NotFound);
                }
                match record.status {
                    SubmissionStatus::Queued => {
                        let address =
                            resolve_address(&INBOX_DOC, Some(record.conversation_id), None)?;
                        let doc = tx.doc(&INBOX_DOC, &address, None).await?;
                        let value = serde_json::to_value(doc.value()?.to_json())?;
                        if let Some(index) = value["items"].as_array().and_then(|items| {
                            items
                                .iter()
                                .position(|item| item.get("id").and_then(Value::as_u64) == Some(id))
                        }) {
                            doc.splice(&[Segment::Key("items".into())], index, 1, Vec::new())?;
                        }
                        tx.settle_submission(
                            id,
                            SubmissionSettlement::Unanswered {
                                reason: "aborted".into(),
                                detail: None,
                            },
                        )?;
                        Ok(AbortSubmissionResult::Aborted)
                    }
                    SubmissionStatus::Placed => Ok(AbortSubmissionResult::AlreadyPlaced),
                    _ => Ok(AbortSubmissionResult::Settled),
                }
            },
            ctx,
        )
        .await
}

use super::generation::start_run;
use super::{Conversation, InputEntry, LIVE_DOC, append_input, apply_boundary, prepare_boundary};
use crate::{
    EntryDraft, EntryHead, SessionError, SubmissionCreate, SubmissionKind, SubmissionNotify,
    session::Transaction,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SubmissionBody {
    Write {
        entry: EntryDraft,
    },
    Input {
        content: Value,
        entry: Option<InputEntry>,
        when_busy: Option<String>,
        when_idle: Option<String>,
        on_abort: Option<String>,
    },
    Continue,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionDraft {
    pub request_id: Option<String>,
    pub notify: Option<SubmissionNotify>,
    #[serde(flatten)]
    pub body: SubmissionBody,
}
impl Conversation {
    pub async fn submit(&self, draft: SubmissionDraft, ctx: &Context) -> Result<Submission> {
        self.harness.resume()?;
        let span = self.submission_span(&draft, ctx);
        let context = ctx.with_telemetry(span.context.clone());
        let result = self
            .session
            .commit(
                async |tx| self.admit_record(tx, draft, Some(&span.context)).await,
                &context,
            )
            .await;
        let id = finish_admission(&span.context, result)?;
        Ok(Submission {
            id,
            harness: self.harness.clone(),
        })
    }
    pub async fn admit(
        &self,
        tx: &mut Transaction<'_>,
        draft: SubmissionDraft,
    ) -> Result<SubmissionId> {
        if !tx.belongs_to(&self.session) {
            return Err(Error::invalid("The transaction belongs to another Session"));
        }
        if tx.has_table_write() {
            return Err(SessionError::ReadAfterWrite("submit(tx, draft)").into());
        }
        self.harness.resume()?;
        let span = self.submission_span(&draft, tx.context());
        let result = self.admit_record(tx, draft, Some(&span.context)).await;
        finish_admission(&span.context, result)
    }

    fn submission_span(&self, draft: &SubmissionDraft, ctx: &Context) -> OpenSpan {
        let kind = match &draft.body {
            SubmissionBody::Write { .. } => "write",
            SubmissionBody::Input { .. } => "input",
            SubmissionBody::Continue => "continue",
        };
        let (parent, links) = if kind == "write" {
            (ctx.telemetry(), Vec::new())
        } else {
            (
                &self.harness.telemetry.root,
                vec![link(ctx.telemetry().clone())],
            )
        };
        let span = OpenSpan::new(
            parent,
            SpanOptions {
                name: "pi.durable.submit".into(),
                attributes: [
                    ("submission.type".into(), kind.into()),
                    ("conversation.id".into(), (self.id as f64).into()),
                ]
                .into(),
                links,
            },
        );
        span.context.set_status(SpanStatus::Error { error: None });
        span
    }

    pub(super) async fn admit_internal(
        &self,
        tx: &mut Transaction<'_>,
        draft: SubmissionDraft,
    ) -> Result<SubmissionId> {
        self.admit_record(tx, draft, None).await.map(|(id, _)| id)
    }

    async fn admit_record(
        &self,
        tx: &mut Transaction<'_>,
        draft: SubmissionDraft,
        span: Option<&TraceContext>,
    ) -> Result<(SubmissionId, SubmissionStatus)> {
        let kind = match &draft.body {
            SubmissionBody::Write { .. } => SubmissionKind::Write,
            SubmissionBody::Input { entry, .. } => {
                if let Some(entry) = entry
                    && (entry.kind.is_empty() || entry.kind.starts_with("pi."))
                {
                    return Err(Error::invalid(
                        "Input entry kind must be a non-reserved application kind",
                    ));
                }
                SubmissionKind::Input
            }
            SubmissionBody::Continue => SubmissionKind::Continue,
        };
        if let Some(request) = &draft.request_id
            && let Some(existing) = tx.submission_by_request(self.id, request).await?
        {
            if existing.kind != kind {
                return Err(Error::invalid(format!(
                    "Request {request} already identifies a submission of type {}",
                    match existing.kind {
                        SubmissionKind::Input => "input",
                        SubmissionKind::Write => "write",
                        SubmissionKind::Continue => "continue",
                    }
                )));
            }
            return Ok((existing.id, existing.status));
        }
        let mut create = SubmissionCreate {
            trace: span.and_then(|span| span.inject(span)),
            conversation_id: self.id,
            request_id: draft.request_id,
            notify: draft.notify,
            kind,
            status: SubmissionStatus::Queued,
            entry: None,
            answer: None,
            reason: None,
            detail: None,
        };
        let address = resolve_address(&INBOX_DOC, Some(self.id), None)?;
        let inbox =
            serde_json::to_value(tx.doc(&INBOX_DOC, &address, None).await?.value()?.to_json())?;
        let items = inbox["items"]
            .as_array()
            .ok_or_else(|| Error::invalid("Invalid inbox"))?;
        if let SubmissionBody::Write { entry } = &draft.body
            && entry.model.is_none()
            && entry.head.is_none()
            && entry.edits.is_none()
            && !items
                .iter()
                .any(|i| i["mode"] == "write" && i["entry"]["head"] == "self")
        {
            create.entry = Some(tx.append_entry(self.id, entry.clone()).await?.id);
            create.status = SubmissionStatus::Done;
            return Ok((
                tx.create_submission(create).await?.id,
                SubmissionStatus::Done,
            ));
        }
        let live = resolve_address(&LIVE_DOC, Some(self.id), None)?;
        let state = serde_json::to_value(tx.doc(&LIVE_DOC, &live, None).await?.value()?.to_json())?;
        let busy = state.get("run").is_some();
        if matches!(&draft.body, SubmissionBody::Input { when_busy, .. } if when_busy.as_deref() == Some("steer"))
            && let Some(span) = span
            && create.trace.is_some()
            && let Some(id) = state["run"]["taskId"].as_u64()
            && let Some(task) = tx.task(id).await?
            && let Some(carrier) = &task.trace
        {
            span.add_link(link(span.extract(Some(carrier))));
        }
        let now = (self.harness.now)();
        if let SubmissionBody::Input {
            when_busy,
            when_idle,
            ..
        } = &draft.body
        {
            if busy && when_busy.as_deref() == Some("reject") {
                return Err(SessionError::ConversationBusy(self.id).into());
            }
            if !busy && when_idle.as_deref() == Some("reject") {
                return Err(SessionError::ConversationIdle(self.id).into());
            }
        }
        let boundary = if busy {
            None
        } else {
            let settings = (self.harness.settings)();
            Some(
                prepare_boundary(
                    tx,
                    self.id,
                    settings["steeringMode"] == "all",
                    settings["followUpMode"] == "all",
                )
                .await?,
            )
        };
        if matches!(draft.body, SubmissionBody::Continue) {
            let boundary = boundary.ok_or(SessionError::ConversationBusy(self.id))?;
            create.status = SubmissionStatus::Placed;
            let id = tx.create_submission(create).await?.id;
            let mut placed = apply_boundary(tx, boundary, true, now).await?;
            placed.users.push(id);
            start_run(
                tx,
                self.id,
                placed.users,
                state["runFailure"]["first"].as_u64().or(placed.first),
            )
            .await?;
            return Ok((id, SubmissionStatus::Placed));
        }
        if busy || !items.is_empty() {
            let id = tx.create_submission(create).await?.id;
            let item = match draft.body {
                SubmissionBody::Write { entry } => json!({"id":id,"mode":"write","entry":entry}),
                SubmissionBody::Input {
                    content,
                    entry,
                    when_busy,
                    on_abort,
                    ..
                } => {
                    let mut item = json!({"id":id,"mode":if when_busy.as_deref()==Some("steer"){"steer"}else{"followUp"},"content":content});
                    if let Some(entry) = entry {
                        item["entry"] = json!(entry);
                    }
                    if on_abort.as_deref() == Some("keep") {
                        item["onAbort"] = json!("keep");
                    }
                    item
                }
                SubmissionBody::Continue => {
                    return Err(Error::invalid("Continue cannot be queued"));
                }
            };
            tx.doc(&INBOX_DOC, &address, None).await?.splice(
                &[Segment::Key("items".into())],
                items.len(),
                0,
                vec![item.into()],
            )?;
            let Some(boundary) = boundary else {
                return Ok((id, SubmissionStatus::Queued));
            };
            let placed = apply_boundary(tx, boundary, true, now).await?;
            let status = if kind == SubmissionKind::Write {
                SubmissionStatus::Done
            } else if placed.users.last() == Some(&id) {
                SubmissionStatus::Placed
            } else {
                SubmissionStatus::Queued
            };
            if !placed.users.is_empty() {
                start_run(tx, self.id, placed.users, placed.first).await?;
            }
            return Ok((id, status));
        }
        match draft.body {
            SubmissionBody::Write { entry } => {
                if matches!(entry.head,Some(EntryHead::Id(h)) if boundary.as_ref().and_then(|b|b.head).is_some_and(|start|h<start))
                {
                    create.status = SubmissionStatus::Unanswered;
                    create.reason = Some("stale".into());
                } else {
                    create.entry = Some(tx.append_entry(self.id, entry).await?.id);
                    create.status = SubmissionStatus::Done;
                }
                let record = tx.create_submission(create).await?;
                Ok((record.id, record.status))
            }
            SubmissionBody::Input { content, entry, .. } => {
                let first = append_input(tx, self.id, content, entry, now).await?.id;
                create.entry = Some(first);
                create.status = SubmissionStatus::Placed;
                let id = tx.create_submission(create).await?.id;
                start_run(tx, self.id, vec![id], Some(first)).await?;
                Ok((id, SubmissionStatus::Placed))
            }
            SubmissionBody::Continue => Err(Error::invalid("Continue already admitted")),
        }
    }
}

fn finish_admission(
    span: &TraceContext,
    result: Result<(SubmissionId, SubmissionStatus)>,
) -> Result<SubmissionId> {
    let (id, status) = result?;
    let status = if status == SubmissionStatus::Unanswered {
        "done"
    } else {
        status.as_str()
    };
    span.set_attributes(
        [
            ("submission.id".into(), (id as f64).into()),
            ("result".into(), status.into()),
        ]
        .into(),
    );
    span.set_status(SpanStatus::Ok);
    Ok(id)
}
