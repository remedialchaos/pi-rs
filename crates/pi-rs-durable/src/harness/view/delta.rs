use pi_rs_chord::delta::{Op, Path, Segment};

fn prefixed(prefix: &Path, op: &Op) -> Op {
    let at = |path: &Path| prefix.iter().chain(path).cloned().collect();
    match op {
        Op::Replace(value) => Op::Set(prefix.clone(), value.clone()),
        Op::Set(path, value) => Op::Set(at(path), value.clone()),
        Op::Delete(path) => Op::Delete(at(path)),
        Op::Append(path, text) => Op::Append(at(path), text.clone()),
        Op::Truncate(path, len) => Op::Truncate(at(path), *len),
        Op::Splice(path, start, remove, items) => {
            Op::Splice(at(path), *start, *remove, items.clone())
        }
        Op::Permute(path, order) => Op::Permute(at(path), order.clone()),
    }
}

pub(in crate::harness) fn key(name: &str) -> Segment {
    Segment::Key(name.into())
}

use super::{AGENT_DOC, INBOX_DOC, Incarnations, LIVE_DOC, PROVIDER_DOC, USAGE_DOC};
use crate::{ConversationId, DocumentCommitChange, JsonObject};
use pi_rs_chord::json::JsonValue;
use serde_json::Value;
use std::sync::Arc;
pub(super) fn advance_document(
    id: ConversationId,
    doc: &DocumentCommitChange,
    incarnations: &mut Incarnations,
    docs: &mut Arc<JsonObject>,
    ops: &mut Vec<Op>,
) {
    let (record, conversation) = match doc {
        DocumentCommitChange::Change {
            record,
            conversation_id,
            ..
        }
        | DocumentCommitChange::Retire {
            record,
            conversation_id,
        } => (record, conversation_id),
    };
    if *conversation != Some(id)
        || record.key.is_some()
        || ![
            &*AGENT_DOC,
            &*LIVE_DOC,
            &*INBOX_DOC,
            &*PROVIDER_DOC,
            &*USAGE_DOC,
        ]
        .iter()
        .any(|token| token.kind == record.kind)
    {
        return;
    }
    let prefix = vec![key("docs"), key(&record.kind)];
    if let DocumentCommitChange::Change {
        value,
        version,
        ops: local,
        ..
    } = doc
    {
        if incarnations.insert(record.kind.clone(), (record.id, *version))
            == Some((record.id, *version))
        {
            ops.extend(local.iter().map(|op| prefixed(&prefix, op)));
        } else {
            ops.push(Op::Set(
                prefix,
                JsonValue::from_serde(Value::Object(value.clone())),
            ));
        }
        Arc::make_mut(docs).insert(record.kind.clone(), Value::Object(value.clone()));
    } else if incarnations
        .get(&record.kind)
        .is_some_and(|(id, _)| *id == record.id)
    {
        incarnations.remove(&record.kind);
        Arc::make_mut(docs).remove(&record.kind);
        ops.push(Op::Delete(prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn document_frames_follow_the_mounted_incarnation_and_version() -> crate::session::Result<()> {
        let kind = "pi.agent";
        let mut incarnations = Incarnations::from([(kind.to_owned(), (10, 1))]);
        let mut docs = Arc::new(JsonObject::from_iter([(
            kind.to_owned(),
            json!({"cwd":"a"}),
        )]));
        let mut frames = Vec::new();
        for (id, version, cwd) in [
            (10, 1, Some("b")),
            (11, 1, Some("c")),
            (10, 1, None),
            (11, 2, Some("d")),
            (11, 2, None),
        ] {
            let record = serde_json::from_value(
                json!({"kind":kind,"id":id,"createdAt":1,"scope":{"kind":"conversation","conversationId":1}}),
            )?;
            let doc = match cwd {
                Some(cwd) => DocumentCommitChange::Change {
                    record,
                    version,
                    conversation_id: Some(1),
                    value: JsonObject::from_iter([("cwd".to_owned(), json!(cwd))]),
                    ops: vec![Op::Set(vec![key("cwd")], json!(cwd).into())],
                },
                None => DocumentCommitChange::Retire {
                    record,
                    conversation_id: Some(1),
                },
            };
            let mut ops = Vec::new();
            advance_document(1, &doc, &mut incarnations, &mut docs, &mut ops);
            if id == 10 && cwd.is_none() {
                assert_eq!(docs.get(kind), Some(&json!({"cwd":"c"})));
                assert!(ops.is_empty());
            }
            if !ops.is_empty() {
                frames.push(ops);
            }
        }
        let path = vec![key("docs"), key(kind)];
        assert_eq!(
            frames,
            vec![
                vec![Op::Set(
                    vec![key("docs"), key(kind), key("cwd")],
                    json!("b").into()
                )],
                vec![Op::Set(path.clone(), json!({"cwd":"c"}).into())],
                vec![Op::Set(path.clone(), json!({"cwd":"d"}).into())],
                vec![Op::Delete(path)]
            ]
        );
        assert!(docs.is_empty());
        Ok(())
    }
}
