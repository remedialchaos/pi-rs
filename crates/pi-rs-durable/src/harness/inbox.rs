use super::INBOX_DOC;
use crate::{
    ConversationId, EntryDraft, EntryHead, EntryId, EntryRecord, SubmissionId,
    SubmissionSettlement,
    documents::resolve_address,
    session::{Result, Transaction},
};
use pi_rs_chord::delta::Segment;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputEntry {
    pub kind: String,
    #[serde(
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum InboxItem {
    Write {
        id: SubmissionId,
        entry: EntryDraft,
    },
    Steer {
        id: SubmissionId,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        entry: Option<InputEntry>,
        #[serde(rename = "onAbort", skip_serializing_if = "Option::is_none")]
        on_abort: Option<String>,
    },
    FollowUp {
        id: SubmissionId,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        entry: Option<InputEntry>,
        #[serde(rename = "onAbort", skip_serializing_if = "Option::is_none")]
        on_abort: Option<String>,
    },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InboxState {
    pub items: Vec<InboxItem>,
}
pub struct Boundary {
    conversation: ConversationId,
    pub(super) head: Option<EntryId>,
    all_steers: bool,
    all_follow_ups: bool,
}
pub struct BoundaryResult {
    pub users: Vec<SubmissionId>,
    pub first: Option<EntryId>,
    pub reset: bool,
}

pub async fn prepare_boundary(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    all_steers: bool,
    all_follow_ups: bool,
) -> Result<Boundary> {
    let head = tx
        .latest_head_marker(conversation)
        .await?
        .and_then(|e| e.head);
    let address = resolve_address(&INBOX_DOC, Some(conversation), None)?;
    tx.doc(&INBOX_DOC, &address, None).await?;
    Ok(Boundary {
        conversation,
        head,
        all_steers,
        all_follow_ups,
    })
}

pub async fn append_input(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    content: Value,
    entry: Option<InputEntry>,
    now: u64,
) -> Result<EntryRecord> {
    let entry = entry.unwrap_or_else(|| InputEntry {
        kind: "pi.user".into(),
        data: None,
    });
    tx.append_entry(
        conversation,
        EntryDraft {
            kind: entry.kind,
            data: entry.data,
            model: Some(vec![
                json!({"role":"user","content":content,"timestamp":now}),
            ]),
            head: None,
            edits: None,
        },
    )
    .await
}

pub async fn apply_boundary(
    tx: &mut Transaction<'_>,
    mut boundary: Boundary,
    at_final: bool,
    now: u64,
) -> Result<BoundaryResult> {
    let conversation = boundary.conversation;
    let address = resolve_address(&INBOX_DOC, Some(conversation), None)?;
    let doc = tx.doc(&INBOX_DOC, &address, None).await?;
    let inbox: InboxState = serde_json::from_value(serde_json::to_value(doc.value()?.to_json())?)?;
    let reset = inbox.items.iter().any(
        |item| matches!(item,InboxItem::Write{entry,..} if entry.head==Some(EntryHead::SelfEntry)),
    );
    let mut first = None;
    let mut users = Vec::new();
    let mut keep = Vec::new();
    let mut steer = true;
    let mut follow = at_final || reset;
    for item in &inbox.items {
        if let InboxItem::Write { id, entry } = item {
            if matches!(entry.head,Some(EntryHead::Id(target)) if boundary.head.is_some_and(|h|target<h))
            {
                tx.settle_submission(
                    *id,
                    SubmissionSettlement::Unanswered {
                        reason: "stale".into(),
                        detail: None,
                    },
                )?;
                continue;
            }
            let placed = tx.append_entry(conversation, entry.clone()).await?;
            if let Some(head) = placed.head {
                boundary.head = Some(head);
            }
            tx.place_submission(*id, placed.id)?;
            first.get_or_insert(placed.id);
        }
    }
    for item in inbox.items {
        let (id, content, entry) = match item {
            InboxItem::Write { .. } => continue,
            InboxItem::Steer {
                id, content, entry, ..
            } if steer => {
                steer = boundary.all_steers;
                (id, content, entry)
            }
            InboxItem::FollowUp {
                id, content, entry, ..
            } if follow => {
                follow = boundary.all_follow_ups;
                (id, content, entry)
            }
            item => {
                keep.push(item);
                continue;
            }
        };
        let placed = append_input(tx, conversation, content, entry, now).await?;
        tx.place_submission(id, placed.id)?;
        first.get_or_insert(placed.id);
        users.push(id);
    }
    tx.doc(&INBOX_DOC, &address, None).await?.set(
        &[Segment::Key("items".into())],
        serde_json::to_value(keep)?.into(),
    )?;
    Ok(BoundaryResult {
        users,
        first,
        reset,
    })
}

fn present_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

pub(super) async fn withdraw_inputs(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    reason: &str,
    honor_keep: bool,
) -> Result<()> {
    let address = resolve_address(&INBOX_DOC, Some(conversation), None)?;
    let state: InboxState = serde_json::from_value(serde_json::to_value(
        tx.doc(&INBOX_DOC, &address, None).await?.value()?.to_json(),
    )?)?;
    for (index, item) in state.items.into_iter().enumerate().rev() {
        let (InboxItem::Steer { id, on_abort, .. } | InboxItem::FollowUp { id, on_abort, .. }) =
            item
        else {
            continue;
        };
        if honor_keep && on_abort.as_deref() == Some("keep") {
            continue;
        }
        tx.settle_submission(
            id,
            SubmissionSettlement::Unanswered {
                reason: reason.into(),
                detail: None,
            },
        )?;
        tx.doc(&INBOX_DOC, &address, None).await?.splice(
            &[Segment::Key("items".into())],
            index,
            1,
            vec![],
        )?;
    }
    Ok(())
}
