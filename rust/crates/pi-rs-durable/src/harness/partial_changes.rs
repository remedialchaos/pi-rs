use super::{AssignPlan, assign_json};
use crate::session::Result;
use pi_rs_ai::{
    streaming_json::{JsonChange, JsonPath, StreamingJsonParser},
    types::AssistantMessageEvent as Event,
};
use pi_rs_chord::delta::{Change, Segment};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Default)]
struct Call {
    parser: StreamingJsonParser,
    changes: Vec<JsonChange>,
    committed: bool,
}

#[derive(Default)]
pub struct PartialChanges {
    text: HashMap<usize, (&'static str, String)>,
    calls: HashMap<usize, Call>,
}
pub struct PartialCommit {
    plan: AssignPlan,
    calls: HashMap<usize, (Vec<JsonChange>, Value)>,
}

impl PartialChanges {
    pub fn observe(&mut self, event: &Event) {
        match event {
            Event::TextDelta {
                content_index,
                delta,
                ..
            }
            | Event::ThinkingDelta {
                content_index,
                delta,
                ..
            } => {
                let field = if matches!(event, Event::TextDelta { .. }) {
                    "text"
                } else {
                    "thinking"
                };
                let entry = self
                    .text
                    .entry(*content_index)
                    .or_insert((field, String::new()));
                if entry.0 != field {
                    *entry = (field, String::new());
                }
                entry.1.push_str(delta);
            }
            Event::TextEnd { content_index, .. } | Event::ThinkingEnd { content_index, .. } => {
                self.text.remove(content_index);
            }
            Event::ToolCallStart { content_index, .. } => {
                self.calls.insert(*content_index, Call::default());
            }
            Event::ToolCallEnd { content_index, .. } => {
                self.calls.remove(content_index);
            }
            Event::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                if let Some(call) = self.calls.get_mut(content_index) {
                    for change in call.parser.append(delta) {
                        if matches!(&change,JsonChange::Set{path,value} if path.is_empty() && !value.is_object())
                            || matches!(&change,JsonChange::Append{path,..} if path.is_empty())
                        {
                            self.calls.remove(content_index);
                            break;
                        }
                        call.changes.push(change);
                    }
                }
            }
            _ => {}
        }
    }
    pub fn take(&mut self, message: &mut Value) -> PartialCommit {
        let mut blocks: HashMap<_, _> = self
            .text
            .drain()
            .map(|(index, (name, text))| {
                (
                    Segment::Index(index),
                    AssignPlan::Fields(
                        [(Segment::Key(name.into()), AssignPlan::Append(text))].into(),
                    ),
                )
            })
            .collect();
        let mut calls = HashMap::new();
        for (index, call) in &mut self.calls {
            if message["content"]
                .get(*index)
                .is_some_and(|b| b["type"] == "toolCall")
            {
                let snapshot =
                    std::mem::replace(&mut message["content"][*index]["arguments"], json!({}));
                calls.insert(*index, (std::mem::take(&mut call.changes), snapshot));
                blocks.insert(
                    Segment::Index(*index),
                    AssignPlan::Fields(
                        [(Segment::Key("arguments".into()), AssignPlan::Keep)].into(),
                    ),
                );
            }
        }
        PartialCommit {
            plan: AssignPlan::Fields(
                [(Segment::Key("content".into()), AssignPlan::Fields(blocks))].into(),
            ),
            calls,
        }
    }
    pub fn restore(&mut self, commit: PartialCommit) {
        for (index, (mut changes, _)) in commit.calls {
            if let Some(call) = self.calls.get_mut(&index) {
                changes.append(&mut call.changes);
                call.changes = changes;
            }
        }
    }
    pub fn commit(
        &mut self,
        doc: &mut Change,
        message: &mut Value,
        commit: &PartialCommit,
    ) -> Result<()> {
        let current = serde_json::to_value(doc.value()?.to_json())?;
        let old = current["generation"].get("message");
        let size = old
            .and_then(|m| m["content"].as_array())
            .map_or(0, Vec::len);
        let mut apply = Vec::new();
        for (&index, (changes, snapshot)) in &commit.calls {
            let Some(call) = self.calls.get_mut(&index) else {
                continue;
            };
            if index >= size && call.committed {
                message["content"][index]["arguments"] = snapshot.clone();
                self.calls.remove(&index);
            } else {
                call.committed = true;
                apply.push((index, changes));
            }
        }
        let mut path = vec![
            Segment::Key("generation".into()),
            Segment::Key("message".into()),
        ];
        assign_json(doc, &mut path, old, message, Some(&commit.plan))?;
        for (index, changes) in apply {
            let mut args = path.clone();
            args.extend([
                Segment::Key("content".into()),
                Segment::Index(index),
                Segment::Key("arguments".into()),
            ]);
            for change in changes {
                let fields = match change {
                    JsonChange::Set { path, .. } | JsonChange::Append { path, .. } => path,
                };
                let mut dest = args.clone();
                dest.extend(fields.iter().map(|f| match f {
                    JsonPath::Key(k) => Segment::Key(k.as_str().into()),
                    JsonPath::Index(i) => Segment::Index(*i),
                }));
                match change {
                    JsonChange::Set { value, .. } => doc.set(&dest, value.clone().into())?,
                    JsonChange::Append { text, .. } => {
                        doc.append_text(&dest, text.as_str().into())?
                    }
                }
            }
        }
        Ok(())
    }
}
