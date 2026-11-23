use super::Harness;
use crate::{
    ConversationId, ConversationQuery, TaskId, TaskQuery, TaskRecord, TaskState, TaskStatus,
    session::Result,
};
use pi_rs_chord::context::Context;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    MissingTask,
    TaskTooOld,
    MigrationFailed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraphNode {
    pub id: TaskId,
    pub kind: String,
    pub conversation_id: ConversationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    pub background: bool,
    pub abort_requested: bool,
    pub state: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
    pub conversations: Vec<ConversationId>,
}

#[derive(Debug, Default, Serialize)]
pub struct TaskGraph {
    pub tasks: BTreeMap<TaskId, TaskGraphNode>,
}

impl Harness {
    /// Read live nodes and their owned conversations on one serialized session revision.
    pub async fn task_graph(
        &self,
        blocked: impl Fn(&TaskRecord) -> Option<BlockedReason>,
        ctx: &Context,
    ) -> Result<TaskGraph> {
        self.session
            .commit(
                async |tx| {
                    let mut graph = TaskGraph::default();
                    for status in [
                        TaskStatus::Pending,
                        TaskStatus::Running,
                        TaskStatus::Waiting,
                        TaskStatus::Sleeping,
                        TaskStatus::Completing,
                    ] {
                        let query = TaskQuery {
                            status: Some(status),
                            ..Default::default()
                        };
                        let mut cursor = None;
                        loop {
                            let page = tx.scan_tasks(&query, 256, cursor.as_ref()).await?;
                            for record in page.items {
                                let blocked = blocked(&record);
                                let mut state = json!({"status":record.state.status().as_str()});
                                match &record.state {
                                    TaskState::Waiting { on, policy, .. } => {
                                        state["on"] = json!(on);
                                        state["policy"] = json!(policy);
                                    }
                                    TaskState::Sleeping { until, signals, .. } => {
                                        if let Some(until) = until {
                                            state["until"] = json!(until);
                                        }
                                        if let Some(signals) = signals {
                                            state["signals"] = json!(signals);
                                        }
                                    }
                                    TaskState::Completing { outcome } => {
                                        state["outcome"] =
                                            serde_json::to_value(outcome)?["status"].clone();
                                    }
                                    TaskState::Terminal { .. } => continue,
                                    _ => {}
                                }
                                if let TaskState::Pending { checkpoint, .. }
                                | TaskState::Running { checkpoint, .. }
                                | TaskState::Waiting { checkpoint, .. }
                                | TaskState::Sleeping { checkpoint, .. } = &record.state
                                    && let Some(phase) = checkpoint.get("phase")
                                {
                                    state["phase"] = phase.clone();
                                }
                                let owned = ConversationQuery {
                                    owner_task_id: Some(record.id),
                                    ..Default::default()
                                };
                                let mut next = None;
                                let mut conversations = Vec::new();
                                loop {
                                    let children =
                                        tx.scan_conversations(&owned, 256, next.as_ref()).await?;
                                    conversations.extend(children.items.into_iter().map(|c| c.id));
                                    next = children.next;
                                    if next.is_none() {
                                        break;
                                    }
                                }
                                conversations.sort_unstable();
                                graph.tasks.insert(
                                    record.id,
                                    TaskGraphNode {
                                        id: record.id,
                                        kind: record.kind,
                                        conversation_id: record.conversation_id,
                                        owner: record.owner,
                                        background: record.background,
                                        abort_requested: record.abort_requested,
                                        state,
                                        blocked,
                                        conversations,
                                    },
                                );
                            }
                            cursor = page.next;
                            if cursor.is_none() {
                                break;
                            }
                        }
                    }
                    Ok(graph)
                },
                ctx,
            )
            .await
    }
}
