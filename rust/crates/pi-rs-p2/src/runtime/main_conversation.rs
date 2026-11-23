use pi_rs_chord::context::Context;
use pi_rs_durable::{
    documents::{DocumentSemantics, InitialDocument, resolve_address},
    errors::BoxError,
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MainBranch {
    pub conversation: ConversationId,
    pub label: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MainState {
    pub conversation: ConversationId,
    pub branches: Vec<MainBranch>,
}

pub fn main_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.main".into(),
        version: 2,
        semantics: DocumentSemantics::Session,
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("conversation".into(), json!(ROOT_CONVERSATION_ID)),
                (
                    "branches".into(),
                    json!([{"conversation": ROOT_CONVERSATION_ID,"label":null}]),
                ),
            ]))
        })),
        migrate: Some(Arc::new(|mut value, version| {
            if version != 1 {
                return Err(format!("Unknown p2.main version {version}").into());
            }
            let branches = value
                .get_mut("branches")
                .and_then(Value::as_array_mut)
                .ok_or("Missing branches")?;
            for branch in branches {
                *branch = json!({"conversation":branch,"label":null});
            }
            Ok(value)
        })),
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}

pub async fn main_state(session: &Session, context: &Context) -> Result<MainState, BoxError> {
    let doc = main_doc();
    let snapshot = session
        .snapshot(&doc, &resolve_address(&doc, None, None)?, context)
        .await?;
    let value = match snapshot {
        Some(value) => (*value).clone(),
        None => doc.initial(None)?,
    };
    Ok(serde_json::from_value(Value::Object(value))?)
}

pub async fn main_conversation(
    session: &Session,
    context: &Context,
) -> Result<ConversationId, BoxError> {
    Ok(main_state(session, context).await?.conversation)
}
pub async fn top_level_conversations(
    session: &Session,
    context: &Context,
) -> Result<Vec<ConversationId>, BoxError> {
    Ok(main_state(session, context)
        .await?
        .branches
        .into_iter()
        .map(|branch| branch.conversation)
        .collect())
}
pub async fn is_top_level(
    session: &Session,
    id: ConversationId,
    context: &Context,
) -> Result<bool, BoxError> {
    Ok(top_level_conversations(session, context)
        .await?
        .contains(&id))
}
pub async fn agent_conversation(
    session: &Session,
    id: ConversationId,
    context: &Context,
) -> Result<ConversationId, BoxError> {
    Ok(if is_top_level(session, id, context).await? {
        ROOT_CONVERSATION_ID
    } else {
        id
    })
}
pub async fn top_level_records(
    session: &Session,
    context: &Context,
) -> Result<Vec<Arc<ConversationRecord>>, BoxError> {
    let ids = top_level_conversations(session, context).await?;
    Ok(session
        .commit(
            async move |tx| {
                let mut records = Vec::new();
                for id in ids {
                    if let Some(record) = tx.conversation(id).await? {
                        records.push(record);
                    }
                }
                records.sort_by_key(|record| record.id);
                Ok(records)
            },
            context,
        )
        .await?)
}

/// Persist the current branch and ordered labels together.
pub async fn set_main_state(
    session: &Session,
    state: &MainState,
    context: &Context,
) -> Result<(), BoxError> {
    let doc = main_doc();
    let address = resolve_address(&doc, None, None)?;
    let branches = serde_json::to_value(&state.branches)?;
    session
        .commit(
            async |tx| {
                use pi_rs_chord::delta::Segment;
                let change = tx.doc(&doc, &address, None).await?;
                change.set(
                    &[Segment::Key("conversation".into())],
                    json!(state.conversation).into(),
                )?;
                change.set(&[Segment::Key("branches".into())], branches.into())?;
                Ok(())
            },
            context,
        )
        .await?;
    Ok(())
}
