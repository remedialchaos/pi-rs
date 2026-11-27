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

#[derive(Clone, Debug, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
        self.attach_graph(blocked, ctx, |graph, _| Ok(graph)).await
    }
    async fn attach_graph<T>(
        &self,
        blocked: impl Fn(&TaskRecord) -> Option<BlockedReason>,
        ctx: &Context,
        attach: impl FnOnce(TaskGraph, BTreeMap<TaskId, u64>) -> Result<T>,
    ) -> Result<T> {
        self.session
            .commit(
                async |tx| {
                    let mut versions = BTreeMap::new();
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
                                if can_block(&record) {
                                    versions.insert(record.id, record.version);
                                }
                                let blocked = blocked(&record);
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
                                graph
                                    .tasks
                                    .insert(record.id, node(&record, blocked, conversations)?);
                            }
                            cursor = page.next;
                            if cursor.is_none() {
                                break;
                            }
                        }
                    }
                    attach(graph, versions)
                },
                ctx,
            )
            .await
    }
}

fn node(
    record: &TaskRecord,
    blocked: Option<BlockedReason>,
    conversations: Vec<ConversationId>,
) -> Result<TaskGraphNode> {
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
            state["outcome"] = serde_json::to_value(outcome)?["status"].clone();
        }
        TaskState::Terminal { .. } => {
            return Err(crate::session::Error::invalid("terminal graph node"));
        }
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
    Ok(TaskGraphNode {
        id: record.id,
        kind: record.kind.clone(),
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
        abort_requested: record.abort_requested,
        state,
        blocked,
        conversations,
    })
}

use super::view::delta::key;
use crate::session::{Observation, ObservedCommit, Observer, Subscription};
use pi_rs_chord::{
    delta::Op,
    json::JsonValue,
    watch::{Watch, WatchCall, WatchEnd},
};
use std::sync::{Arc, Mutex};

fn can_block(record: &TaskRecord) -> bool {
    !matches!(
        record.state.status(),
        TaskStatus::Running | TaskStatus::Completing | TaskStatus::Terminal
    ) && (!record.abort_requested || record.abort_reason.is_some())
}

fn blocked(
    harness: &Harness,
    id: TaskId,
    registry: &super::RegistrySnapshot,
    kind: &str,
    version: u64,
) -> Option<BlockedReason> {
    match registry.task(kind) {
        None => Some(BlockedReason::MissingTask),
        Some(t) if t.version() < version => Some(BlockedReason::TaskTooOld),
        Some(t)
            if t.version() > version
                && (t.definition.migrate.is_none() || harness.migration_failed(id, t)) =>
        {
            Some(BlockedReason::MigrationFailed)
        }
        _ => None,
    }
}

impl Harness {
    async fn attach_graph_listener<T>(
        &self,
        ctx: &Context,
        create: impl FnOnce(Arc<TaskGraph>) -> Result<(T, Observer<WatchCall<Arc<TaskGraph>>>)>,
    ) -> Result<(T, Subscription)> {
        let mut changes = self.registry.subscribe();
        let mut migrations = self.driver.migration_changes.subscribe();
        self.attach_graph(
            |_| None,
            ctx,
            |mut graph, versions| {
                ctx.check()?;
                let snapshot = changes.borrow_and_update().clone();
                for (id, version) in &versions {
                    if let Some(node) = graph.tasks.get_mut(id) {
                        node.blocked = blocked(self, *id, &snapshot, &node.kind, *version);
                    }
                }
                let graph = Arc::new(graph);
                let (handle, observer) = create(graph.clone())?;
                let observer = Arc::new(observer);
                let state = Arc::new(Mutex::new((graph, versions)));
                let current = state.clone();
                let listener = observer.clone();
                let harness = self.clone();
                let registry = self.registry.clone();
                let commits = self.session.follow_commits(move |publication, context| {
                    let ObservedCommit::Stored(commit) = publication else {
                        return Ok(());
                    };
                    let mut guard = current.lock().unwrap_or_else(|e| e.into_inner());
                    let (graph, versions) = &mut *guard;
                    let snapshot = registry.snapshot();
                    let mut ops = Vec::new();
                    for change in &commit.changes {
                        let crate::CommitChange::Task { value: r } = change else {
                            continue;
                        };
                        if r.state.status() == TaskStatus::Terminal {
                            versions.remove(&r.id);
                            if graph.tasks.contains_key(&r.id) {
                                Arc::make_mut(graph).tasks.remove(&r.id);
                                ops.push(Op::Delete(vec![key("tasks"), key(&r.id.to_string())]));
                            }
                        } else {
                            if can_block(r) {
                                versions.insert(r.id, r.version);
                            } else {
                                versions.remove(&r.id);
                            }
                            let prev = graph.tasks.get(&r.id);
                            let next = node(
                                r,
                                if can_block(r) {
                                    blocked(&harness, r.id, &snapshot, &r.kind, r.version)
                                } else {
                                    None
                                },
                                prev.map(|n| n.conversations.clone()).unwrap_or_default(),
                            )?;
                            if prev != Some(&next) {
                                ops.push(Op::Set(
                                    vec![key("tasks"), key(&r.id.to_string())],
                                    serde_json::to_value(&next)?.into(),
                                ));
                                Arc::make_mut(graph).tasks.insert(r.id, next);
                            }
                        }
                    }
                    let mut created = indexmap::IndexMap::<TaskId, Vec<ConversationId>>::new();
                    for change in &commit.changes {
                        if let crate::CommitChange::Conversation { value } = change
                            && let Some(owner) = value.owner
                            && graph.tasks.contains_key(&owner.task_id)
                        {
                            created.entry(owner.task_id).or_default().push(value.id);
                        }
                    }
                    for (id, ids) in created {
                        if let Some(task) = Arc::make_mut(graph).tasks.get_mut(&id) {
                            task.conversations.extend(ids);
                            task.conversations.sort_unstable();
                            ops.push(Op::Set(
                                vec![key("tasks"), key(&id.to_string()), key("conversations")],
                                serde_json::to_value(&task.conversations)?.into(),
                            ));
                        }
                    }
                    if !ops.is_empty() {
                        let value = graph.clone();
                        drop(guard);
                        (listener.advance)(
                            &WatchCall::Value {
                                value,
                                ops,
                                retired: false,
                            },
                            &context.without_abort_signal(),
                        )
                        .map_err(|e| crate::session::Error::invalid(e.to_string()))?;
                    }
                    Ok(())
                })?;
                let session = self.session.clone();
                let closing = observer.clone();
                let refresh_session = session.clone();
                let harness = self.clone();
                let refresh = tokio::spawn(async move {
                    let context = Context::background();
                    while tokio::select! { result = changes.changed() => result, result = migrations.changed() => result }.is_ok() {
                        let snapshot = changes.borrow_and_update().clone();
                        let result = refresh_session
                            .commit(
                                async |_| {
                                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                                    let (graph, versions) = &mut *guard;
                                    let mut ops = Vec::new();
                                    for (id, version) in versions.iter() {
                                        if let Some(task) = graph.tasks.get(id) {
                                            let reason = blocked(&harness, *id, &snapshot, &task.kind, *version);
                                            if reason != task.blocked
                                                && let Some(task) =
                                                    Arc::make_mut(graph).tasks.get_mut(id)
                                            {
                                                task.blocked = reason;
                                                let path = vec![
                                                    key("tasks"),
                                                    key(&id.to_string()),
                                                    key("blocked"),
                                                ];
                                                ops.push(match reason {
                                                    Some(reason) => Op::Set(
                                                        path,
                                                        serde_json::to_value(reason)?.into(),
                                                    ),
                                                    None => Op::Delete(path),
                                                });
                                            }
                                        }
                                    }
                                    if !ops.is_empty() {
                                        let value = graph.clone();
                                        drop(guard);
                                        (observer.advance)(
                                            &WatchCall::Value {
                                                value,
                                                ops,
                                                retired: false,
                                            },
                                            &context,
                                        )
                                        .map_err(|e| {
                                            crate::session::Error::invalid(e.to_string())
                                        })?;
                                    }
                                    Ok(())
                                },
                                &context,
                            )
                            .await;
                        if result.is_err() {
                            break;
                        }
                    }
                });
                let abort = refresh.abort_handle();
                let close = self.session.subscribe_close(move || {
                    abort.abort();
                    (closing.end)(session.failure().map_or(WatchEnd::Ended, WatchEnd::Failed));
                });
                if close.is_err() {
                    refresh.abort();
                }
                let close = close?;
                Ok((
                    handle,
                    Subscription::new(move || {
                        refresh.abort();
                        drop((commits, close));
                    }),
                ))
            },
        )
        .await
    }

    pub async fn observe_task_graph(
        &self,
        observer: Observer<WatchCall<Arc<TaskGraph>>>,
        ctx: &Context,
    ) -> Result<Observation<Arc<TaskGraph>>> {
        let (value, subscription) = self
            .attach_graph_listener(ctx, |value| {
                Ok((value, super::view::host_observer(observer)))
            })
            .await?;
        ctx.check()?;
        Ok(Observation {
            value,
            subscription,
        })
    }

    pub async fn watch_task_graph(&self, ctx: &Context) -> Result<Watch> {
        let holder = Arc::new(Mutex::new(None));
        let cleanup = holder.clone();
        let (watch, subscription) = self
            .attach_graph_listener(ctx, |value| {
                let watch = Watch::new(
                    JsonValue::from_serde(serde_json::to_value(value.as_ref())?),
                    move || {
                        cleanup.lock().unwrap_or_else(|e| e.into_inner()).take();
                    },
                    |_| {},
                );
                let output = watch.clone();
                let ending = watch.clone();
                Ok((
                    watch,
                    Observer::new(
                        move |call: &WatchCall<Arc<TaskGraph>>, c| {
                            if let WatchCall::Value {
                                value,
                                ops,
                                retired,
                            } = call
                            {
                                output.advance(
                                    WatchCall::Value {
                                        value: serde_json::to_value(value.as_ref())?.into(),
                                        ops: ops.clone(),
                                        retired: *retired,
                                    },
                                    c.clone(),
                                );
                            }
                            Ok(())
                        },
                        move |end| {
                            ending.end(end);
                        },
                    ),
                ))
            })
            .await?;
        *holder.lock().unwrap_or_else(|e| e.into_inner()) = Some(subscription);
        if let Some(signal) = ctx.abort_signal()
            && let Err(e) = watch.observe_abort(signal)
        {
            watch.stop();
            return Err(crate::session::Error::invalid(e.to_string()));
        }
        if let Err(error) = ctx.check() {
            watch.stop();
            return Err(error.into());
        }
        Ok(watch)
    }
}
