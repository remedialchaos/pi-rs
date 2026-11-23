use crate::{
    ConversationId, DocDefinition, DocToken, DocumentFork, JsonObject, Session,
    documents::{DocumentSemantics, InitialDocument, LatestFork, resolve_address},
    session::{Error, Result},
};
use pi_rs_chord::context::Context;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

fn latest(kind: &str, initial: fn() -> JsonObject) -> DocDefinition {
    DocDefinition {
        kind: kind.into(),
        version: 1,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(move || Ok(initial()))),
        checkpoint_when: Arc::new(|_, _, _| true),
        migrate: None,
    }
}

pub static PROVIDER_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    Arc::new(latest("pi.provider", || {
        JsonObject::from_iter([("sessionId".into(), json!(uuid::Uuid::now_v7()))])
    }))
});

pub static AGENT_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    Arc::new(DocDefinition {
        semantics: DocumentSemantics::RewindableConversation {
            fork: DocumentFork::AsOf,
        },
        ..latest("pi.agent", JsonObject::new)
    })
});

pub static USAGE_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    Arc::new(latest("pi.usage", || {
        JsonObject::from_iter([("models".into(), json!({})), ("tools".into(), json!({}))])
    }))
});

pub static INBOX_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    Arc::new(DocDefinition {
        checkpoint_when: Arc::new(|value, _, _| {
            value
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        }),
        ..latest("pi.inbox", || {
            JsonObject::from_iter([("items".into(), json!([]))])
        })
    })
});

pub static LIVE_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    Arc::new(DocDefinition {
        version: 3,
        checkpoint_when: Arc::new(|value, _, _| {
            !value.contains_key("generation")
                && ["tools", "nestedTools"].iter().all(|key| {
                    !value
                        .get(*key)
                        .and_then(Value::as_array)
                        .is_some_and(|slots| slots.iter().any(|slot| slot["status"] == "running"))
                })
        }),
        migrate: Some(Arc::new(|mut value, _| {
            if let Some(run) = value.get_mut("run").and_then(Value::as_object_mut)
                && !run.get("turns").is_some_and(Value::is_number)
            {
                run.insert("turns".into(), json!(0));
            }
            if let Some(items) = value.get_mut("compactions").and_then(Value::as_array_mut) {
                for item in items {
                    rename_reason(item);
                }
            }
            if let Some(failure) = value.get_mut("compactionFailure") {
                rename_reason(failure);
            }
            Ok(value)
        })),
        ..latest("pi.live", JsonObject::new)
    })
});

fn rename_reason(item: &mut Value) {
    if let Some(fields) = item.as_object_mut() {
        *fields = std::mem::take(fields)
            .into_iter()
            .map(|(key, value)| {
                (
                    if key == "reason" {
                        "trigger".into()
                    } else {
                        key
                    },
                    value,
                )
            })
            .collect();
    }
}

/// Initialize the identity in a serialized transaction for conversations created before pi.provider existed.
pub async fn ensure_provider_session_id(
    session: &Session,
    conversation: ConversationId,
    context: &Context,
) -> Result<String> {
    let address = resolve_address(&PROVIDER_DOC, Some(conversation), None)?;
    if let Some(doc) = session.snapshot(&PROVIDER_DOC, &address, context).await?
        && let Some(id) = doc.get("sessionId").and_then(Value::as_str)
    {
        return Ok(id.into());
    }
    session
        .commit(
            async |tx| {
                let draft = tx.doc(&PROVIDER_DOC, &address, None).await?;
                let value = serde_json::to_value(draft.value()?.to_json())?;
                value["sessionId"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "Conversation {conversation} has no provider session ID"
                        ))
                    })
            },
            context,
        )
        .await
}
