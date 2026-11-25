use pi_rs_chord::context::Context;
use pi_rs_durable::{
    documents::{DocumentSemantics, InitialDocument, resolve_address},
    errors::BoxError,
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;

pub const LOCK_CAPABILITIES: [&str; 2] = ["ports", "conversations"];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockCapability {
    Ports,
    Conversations,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionLock {
    pub mcp: Vec<String>,
    pub project_mcp: bool,
    pub instructions: bool,
    pub capabilities: Vec<LockCapability>,
}
pub fn session_lock_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.session-lock".into(),
        version: 1,
        semantics: DocumentSemantics::Session,
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([("lock".into(), Value::Null)]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}
pub async fn session_lock(
    session: &Session,
    context: &Context,
) -> Result<Option<SessionLock>, BoxError> {
    let doc = session_lock_doc();
    let snapshot = session
        .snapshot(&doc, &resolve_address(&doc, None, None)?, context)
        .await?;
    Ok(serde_json::from_value(
        snapshot
            .as_ref()
            .and_then(|v| v.get("lock"))
            .cloned()
            .unwrap_or(Value::Null),
    )?)
}

pub async fn set_session_lock(
    session: &Session,
    lock: Option<&SessionLock>,
    context: &Context,
) -> Result<(), BoxError> {
    if let Some(lock) = lock
        && (lock.mcp.len() > 200
            || lock.capabilities.len() > 8
            || lock.mcp.iter().any(|name| name.chars().count() > 200))
    {
        return Err("Session lock exceeds schema limits".into());
    }
    let doc = session_lock_doc();
    let address = resolve_address(&doc, None, None)?;
    let value = serde_json::to_value(lock)?;
    session
        .commit(
            async |tx| {
                tx.doc(&doc, &address, None).await?.set(
                    &[pi_rs_chord::delta::Segment::Key("lock".into())],
                    value.into(),
                )?;
                Ok(())
            },
            context,
        )
        .await?;
    Ok(())
}
