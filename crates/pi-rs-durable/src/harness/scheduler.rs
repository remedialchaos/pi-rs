use super::Harness;
use crate::{
    TaskId, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState,
    session::{Error, Result, Transaction, TransactionScope},
    signals::{check_sleep, sleep_wake},
    tasks::Task,
};
use pi_rs_chord::context::Context;
use pi_rs_telemetry::TelemetrySpan;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct Invocation {
    pub(super) progress: Arc<std::sync::Mutex<super::tool_progress::Progress>>,
    pub(super) progress_notice: Arc<tokio::sync::Notify>,
    pub(super) base_parks: Arc<std::sync::Mutex<Vec<Arc<super::ToolPark>>>>,
    pub(super) harness: Harness,
    pub record: TaskRecord,
}
impl crate::tasks::TaskRuntime for Invocation {
    fn task_id(&self) -> TaskId {
        self.record.id
    }
    fn conversation_id(&self) -> crate::ConversationId {
        self.record.conversation_id
    }
}

impl Invocation {
    /// Stage the phase's effects and checkpoint on the same transaction line.
    pub async fn commit(
        &self,
        ctx: &Context,
        change: impl for<'a, 'tx> AsyncFnOnce(&'a mut Transaction<'tx>, TaskRecord) -> Result<TaskState>,
    ) -> Result<()> {
        let id = self.record.id;
        let checkpoint = self
            .harness
            .session
            .commit_with(
                async |tx| {
                    let mut current = tx
                        .task(id)
                        .await?
                        .ok_or_else(|| Error::invalid(format!("Task {id} does not exist")))?
                        .as_ref()
                        .clone();
                    if current.state != self.record.state || current.abort_requested != self.record.abort_requested
                    {
                        return Err(Error::invalid(format!("Task {id} invocation ended")));
                    }
                    let mut ownership = super::ownership::Ownership::read(tx).await?;
                    let mut state = change(tx, current.clone()).await?;
                    ownership.overlay(tx);
                    if let TaskState::Terminal { outcome } = &state
                        && ownership.has_children(id)
                    {
                        state = TaskState::Completing {
                            outcome: outcome.clone(),
                        };
                    }
                    if let TaskState::Waiting { on, policy, .. } = &state {
                        for child in on {
                            let target = tx
                                .current_task(*child)
                                .await?
                                .ok_or_else(|| Error::invalid(format!("Task {child} does not exist")))?;
                            if *policy == crate::JoinPolicy::FailFast && target.owner != Some(id) {
                                return Err(Error::invalid(format!(
                                    "Task {id} can wait failFast only on tasks it owns; {child} is not one"
                                )));
                            }
                            if target.owner != Some(id)
                                && !matches!(target.state, TaskState::Terminal { .. })
                                && let Some(path) = ownership.wait_cycle(id, *child)
                            {
                                let cycle = std::iter::once(id)
                                    .chain(path)
                                    .map(|n| n.to_string())
                                    .collect::<Vec<_>>()
                                    .join(" -> ");
                                return Err(Error::invalid(format!(
                                    "Task {id} cannot wait on {child}: {cycle} is a wait cycle"
                                )));
                            }
                        }
                    }
                    if matches!(state, TaskState::Sleeping { .. }) {
                        if current.abort_requested {
                            return Err(Error::invalid(format!(
                                "Abort handler of task {id} cannot sleep"
                            )));
                        }
                        check_sleep(&state, current.memos.as_ref(), id)
                            .map_err(|e| Error::invalid(e.to_string()))?;
                    }
                    if current.abort_requested && matches!(state, TaskState::Waiting { .. }) {
                        return Err(Error::invalid(format!(
                            "Abort handler of task {id} cannot wait"
                        )));
                    }
                    if state != current.state {
                        current.restarts = None;
                    }
                    if matches!(
                        state,
                        TaskState::Terminal { .. } | TaskState::Completing { .. }
                    ) {
                        current.memos = None;
                    }
                    let advanced = match (&current.state, &state) {
                        (
                            TaskState::Running {
                                checkpoint: previous,
                                ..
                            },
                            TaskState::Running { checkpoint, .. },
                        ) if previous["phase"] != checkpoint["phase"] => {
                            checkpoint["phase"].as_str().map(str::to_owned)
                        }
                        _ => None,
                    };
                    current.state = state;
                    tx.set_task(current)?;
                    Ok(advanced)
                },
                ctx,
                TransactionScope {
                    task_id: Some(id),
                    conversation_id: Some(self.record.conversation_id),
                },
                Default::default(),
            )
            .await?;
        if let Some(phase) = checkpoint {
            ctx.telemetry().add_event(
                "pi.durable.checkpoint",
                [("phase".into(), phase.into())].into(),
            );
        }
        Ok(())
    }
}

impl Harness {
    /// Reserve a pending or woken task, store the running checkpoint, then invoke its registered phase.
    pub async fn dispatch(
        &self,
        id: TaskId,
        task: Arc<Task<Invocation>>,
        now: u64,
        ctx: &Context,
    ) -> Result<bool> {
        let reserved = self
            .session
            .commit(
                async |tx| {
                    let Some(current) = tx.task(id).await? else {
                        return Ok(None);
                    };
                    if current.kind != task.name() || current.version > task.version() {
                        return Ok(None);
                    }
                    let mut record = current.as_ref().clone();
                    if record.abort_requested && Ownership::read(tx).await?.has_children(id) {
                        return Ok(None);
                    }
                    let (mut checkpoint, wake) = match &record.state {
                        TaskState::Pending { checkpoint, wake } => {
                            (checkpoint.clone(), wake.clone())
                        }
                        TaskState::Sleeping { checkpoint, .. } if record.abort_requested => {
                            (checkpoint.clone(), None)
                        }
                        TaskState::Sleeping { checkpoint, .. } => {
                            let Some(woke) = sleep_wake(&record.state, record.memos.as_ref(), now)
                            else {
                                return Ok(None);
                            };
                            record
                                .memos
                                .get_or_insert_with(Default::default)
                                .extend(woke.memos);
                            (checkpoint.clone(), Some(woke.wake))
                        }
                        TaskState::Waiting {
                            checkpoint,
                            on,
                            policy,
                        } => {
                            let mut ended = Vec::new();
                            for child in on {
                                if tx
                                    .task(*child)
                                    .await?
                                    .is_none_or(|r| matches!(r.state, TaskState::Terminal { .. }))
                                {
                                    ended.push(*child);
                                }
                            }
                            if !record.abort_requested
                                && ended.len() != on.len()
                                && (*policy != crate::JoinPolicy::Any || ended.is_empty())
                            {
                                return Ok(None);
                            }
                            (checkpoint.clone(), Some(crate::WakeReason::Tasks { ended }))
                        }
                        _ => return Ok(None),
                    };
                    if current.version < task.version() {
                        let Some(migrate) = &task.definition.migrate else {
                            return Ok(None);
                        };
                        let migrated = match migrate(record.input, checkpoint, current.version) {
                            Ok(migrated) => migrated,
                            Err(error) => {
                                self.driver
                                    .state
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .failed_migrations
                                    .insert(id, Arc::downgrade(&task));
                                self.report_migration_failure(id, &error.to_string(), ctx);
                                self.driver.migration_changes.send_replace(());
                                self.driver.notify.notify_one();
                                return Ok(None);
                            }
                        };
                        record.input = migrated.input;
                        checkpoint = migrated.checkpoint;
                        record.version = task.version();
                    }
                    let first = record.started_at.is_none();
                    record.started_at.get_or_insert(now);
                    record.state = TaskState::Running { checkpoint, wake };
                    tx.set_task(record.clone())?;
                    Ok(Some((record, first)))
                },
                ctx,
            )
            .await?;
        let Some((mut record, mut first)) = reserved else {
            return Ok(false);
        };
        let mut continued = None;
        while let TaskState::Running { checkpoint, .. } = &record.state {
            let phase = checkpoint
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or("");
            let handler = if record.abort_requested {
                Some(&task.definition.abort)
            } else {
                task.definition.phases.get(phase)
            };
            let invocation = Invocation {
                progress: Default::default(),
                progress_notice: Default::default(),
                base_parks: Default::default(),
                harness: self.clone(),
                record: record.clone(),
            };
            let (finished, waiting) = tokio::sync::watch::channel(());
            self.driver
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .phases
                .insert(id, (invocation.clone(), waiting));
            self.retain_context(invocation.record.conversation_id);
            let _phase = PhaseGuard {
                driver: &self.driver,
                id,
                _finished: finished,
            };
            let mut trace = continued
                .take()
                .unwrap_or_else(|| self.telemetry.phase(&record, checkpoint, phase, first));
            first = false;
            let decision_context = ctx.with_telemetry(trace.span.context.clone());
            let (phase_context, cancel) = decision_context.with_cancel();
            let mark = record.abort_requested;
            let _observe=self.session.follow_commits(move |publication,_|{
                if !mark
                    && let crate::session::ObservedCommit::Stored(commit) = publication
                    && commit.changes.iter().any(|change| matches!(
                        change, crate::CommitChange::Task { value } if value.id == id && value.abort_requested
                    ))
                {
                    cancel.cancel("Task aborted");
                }
                Ok(())
            })?;
            let current = self.session.task(id, ctx).await?;
            let failure = if current.is_some_and(|t| t.abort_requested != mark) {
                None
            } else {
                match handler {
                    Some(handler) => match phase_context
                        .await_with(handler(&record, &invocation, &phase_context))
                        .await
                    {
                        Ok(result) => result.err().map(|e| e.to_string()),
                        Err(_) => None,
                    },
                    None => Some(format!("Unknown phase {phase} of task {}", record.kind)),
                }
            };
            let (next, fault) = self
                .session
                .commit(
                    async |tx| {
                        let Some(current) = tx.task(id).await? else {
                            return Ok((None, None));
                        };
                        if current.abort_requested
                            && !record.abort_requested
                            && let TaskState::Running { checkpoint, wake } = &current.state
                        {
                            let mut pending = current.as_ref().clone();
                            pending.state = TaskState::Pending {
                                checkpoint: checkpoint.clone(),
                                wake: wake.clone(),
                            };
                            tx.set_task(pending.clone())?;
                            return Ok((Some(pending), None));
                        }
                        if current.state != record.state
                            || current.abort_requested != record.abort_requested
                        {
                            return Ok((Some(current.as_ref().clone()), None));
                        }
                        let message = failure.unwrap_or_else(|| {
                            format!(
                                "Task {} phase {phase} returned without durable progress",
                                record.kind
                            )
                        });
                        let mut current = current.as_ref().clone();
                        let outcome = TaskOutcome::Faulted {
                            error: TaskOutcomeError {
                                message: message.clone(),
                                detail: None,
                            },
                        };
                        current.state = if Ownership::read(tx).await?.has_children(id) {
                            TaskState::Completing { outcome }
                        } else {
                            super::live::settle_scheduler_outcome(tx, &current, &outcome).await?;
                            current.ended_at = Some(now);
                            TaskState::Terminal { outcome }
                        };
                        current.memos = None;
                        current.restarts = None;
                        tx.set_task(current.clone())?;
                        Ok((Some(current), Some(message)))
                    },
                    &decision_context,
                )
                .await?;
            if let Some(message) = fault {
                self.session
                    .report_task_fault(id, &message, &decision_context);
            }
            let Some(updated) = next else { break };
            if record.kind == "pi.tool"
                && phase == "call"
                && updated.abort_requested == record.abort_requested
                && matches!(&updated.state, TaskState::Running { checkpoint, .. } if checkpoint["phase"] == "execute")
            {
                continued = Some(trace);
            } else {
                trace.finish(&record, &updated);
            }
            record = updated;
        }
        Ok(true)
    }
}

impl Harness {
    pub(super) async fn load_telemetry_tasks(&self, ctx: &Context) -> Result<()> {
        for status in [
            crate::TaskStatus::Pending,
            crate::TaskStatus::Running,
            crate::TaskStatus::Waiting,
            crate::TaskStatus::Sleeping,
            crate::TaskStatus::Completing,
        ] {
            let query = crate::TaskQuery {
                status: Some(status),
                ..Default::default()
            };
            let mut cursor = None;
            loop {
                let page = self
                    .session
                    .scan_tasks(&query, 256, cursor.as_ref(), ctx)
                    .await?;
                for task in page.items {
                    self.telemetry.track(&task, true);
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn recover_running(&self, ctx: &Context) -> Result<()> {
        self.session
            .commit(
                async |tx| {
                    let graph = Ownership::read(tx).await?;
                    for prior in graph.tasks.values() {
                        let mut record = prior.clone();
                        if let TaskState::Running { checkpoint, wake } = &prior.state {
                            record.state = TaskState::Pending {
                                checkpoint: checkpoint.clone(),
                                wake: wake.clone(),
                            };
                            record.restarts = Some(record.restarts.unwrap_or(0).saturating_add(1));
                        }
                        if record.abandon_on_restart == Some(true) && !record.abort_requested {
                            let keeps = record
                                .owner
                                .and_then(|id| graph.tasks.get(&id))
                                .is_some_and(|owner| {
                                    if owner.kind != "pi.tool" {
                                        return false;
                                    }
                                    let checkpoint = match &owner.state {
                                        TaskState::Pending { checkpoint, .. }
                                        | TaskState::Sleeping { checkpoint, .. }
                                        | TaskState::Running { checkpoint, .. }
                                        | TaskState::Waiting { checkpoint, .. } => checkpoint,
                                        _ => return false,
                                    };
                                    matches!(checkpoint["phase"].as_str(), Some("call" | "parked"))
                                });
                            if !keeps {
                                record.abort_requested = true;
                                record.abort_reason = Some(crate::TaskAbortReason::Restart);
                            }
                        }
                        if record != *prior {
                            tx.set_task(record)?;
                        }
                    }
                    Ok(())
                },
                ctx,
            )
            .await
    }
}

#[derive(Default)]
pub(super) struct Driver {
    state: std::sync::Mutex<DriverState>,
    notify: Arc<tokio::sync::Notify>,
    pub(super) close_line: tokio::sync::Mutex<()>,
    pub(super) migration_changes: tokio::sync::watch::Sender<()>,
}
#[derive(Default)]
struct DriverState {
    phases: std::collections::HashMap<TaskId, (Invocation, tokio::sync::watch::Receiver<()>)>,
    closing_progress: Vec<Invocation>,
    failed_migrations: std::collections::HashMap<TaskId, std::sync::Weak<Task<Invocation>>>,
    closing: bool,
    last_wake: Option<Wake>,
    active: Option<(
        pi_rs_chord::context::Cancel,
        tokio::task::JoinHandle<Result<()>>,
    )>,
}

struct PhaseGuard<'a> {
    driver: &'a Driver,
    id: TaskId,
    _finished: tokio::sync::watch::Sender<()>,
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.driver.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((invocation, _)) = state.phases.remove(&self.id) {
            if state.closing {
                if invocation.record.kind == "pi.tool" {
                    state.closing_progress.push(invocation);
                }
            } else {
                drop(state);
                invocation
                    .harness
                    .retain_context(invocation.record.conversation_id);
            }
        }
    }
}

impl Harness {
    pub(super) fn conversation_active(&self, id: crate::ConversationId) -> bool {
        self.driver
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .phases
            .values()
            .any(|(inv, _)| inv.record.conversation_id == id)
    }

    pub(super) fn report_migration_failure(&self, id: TaskId, message: &str, ctx: &Context) {
        self.session
            .harness_report(crate::session::ReportKind::Extension, message);
        self.telemetry
            .report("migration_failed", Some(id), Some(ctx.telemetry()));
    }

    pub(super) fn migration_failed(&self, id: TaskId, task: &Arc<Task<Invocation>>) -> bool {
        self.driver
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed_migrations
            .get(&id)
            .is_some_and(|failed| failed.ptr_eq(&Arc::downgrade(task)))
    }

    pub(super) fn running_phase(&self, id: TaskId) -> Option<tokio::sync::watch::Receiver<()>> {
        self.driver
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .phases
            .get(&id)
            .filter(|(inv, _)| !inv.record.abort_requested)
            .map(|(_, waiting)| waiting.clone())
    }

    pub(super) fn blocked_reason(
        &self,
        record: &TaskRecord,
        definition: Option<&Arc<Task<Invocation>>>,
    ) -> Option<&'static str> {
        let Some(task) = definition else {
            return Some("missing_task");
        };
        if task.version() < record.version {
            return Some("task_too_old");
        }
        if task.version() > record.version
            && (task.definition.migrate.is_none() || self.migration_failed(record.id, task))
        {
            return Some("migration_failed");
        }
        None
    }

    pub(super) fn publish_wake(&self, callback: &Arc<dyn Fn(Wake) + Send + Sync>, value: Wake) {
        {
            let mut state = self.driver.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closing || state.last_wake.as_ref() == Some(&value) {
                return;
            }
            state.last_wake = Some(value.clone());
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(value))).is_err() {
            self.driver
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_wake = None;
            self.session
                .harness_report(crate::session::ReportKind::Host, "onWake panicked");
        }
    }
    pub fn resume(&self) -> Result<()> {
        let mut state = self.driver.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::Closed);
        }
        if self.read_only {
            return Err(crate::SessionError::ReadOnly("running tasks".into()).into());
        }
        if state.active.is_none() {
            let (ctx, cancel) = Context::background().with_cancel();
            let harness = self.clone();
            state.active = Some((
                cancel,
                tokio::spawn(async move { harness.run_driver(ctx).await }),
            ));
        }
        self.driver.notify.notify_one();
        Ok(())
    }

    pub(super) async fn stop_driver(&self) -> Result<()> {
        let active = {
            let mut state = self.driver.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closing = true;
            state.active.take()
        };
        if let Some((cancel, handle)) = active {
            cancel.cancel("Harness closing");
            match handle.await.map_err(|e| Error::Callback(Box::new(e)))? {
                Ok(()) | Err(Error::Closed | Error::Cancelled(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn run_driver(&self, ctx: Context) -> Result<()> {
        let mut active = std::collections::HashSet::new();
        let mut running = tokio::task::JoinSet::new();
        let result = self.drive(&ctx, &mut active, &mut running).await;
        running.shutdown().await;
        let progress = std::mem::take(
            &mut self
                .driver
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closing_progress,
        );
        for invocation in progress {
            invocation
                .write_tool_progress(&Context::background(), true)
                .await?;
        }
        if ctx.check().is_err() && !active.is_empty() {
            self.session
                .commit(
                    async |tx| {
                        let mut stopped = Vec::new();
                        for id in active {
                            if let Some(record) = tx.task(id).await?
                                && let TaskState::Running { checkpoint, wake } = &record.state
                            {
                                let mut pending = record.as_ref().clone();
                                pending.state = TaskState::Pending {
                                    checkpoint: checkpoint.clone(),
                                    wake: wake.clone(),
                                };
                                stopped.push(pending);
                            }
                        }
                        for record in stopped {
                            tx.set_task(record)?;
                        }
                        Ok(())
                    },
                    &Context::background(),
                )
                .await?;
        }
        result
    }

    async fn drive(
        &self,
        ctx: &Context,
        active: &mut std::collections::HashSet<TaskId>,
        running: &mut tokio::task::JoinSet<(TaskId, Result<bool>)>,
    ) -> Result<()> {
        let changed = self.driver.notify.clone();
        let _commits = self.session.follow_commits(move |publication, _| {
            if let crate::session::ObservedCommit::Stored(commit) = publication
                && commit
                    .changes
                    .iter()
                    .any(|c| matches!(c, crate::CommitChange::Task { .. }))
            {
                changed.notify_one();
            }
            Ok(())
        })?;
        let mut registry = self.registry.subscribe();
        loop {
            ctx.check()?;
            self.reconcile(ctx).await?;
            let snapshot = self.registry.snapshot();
            let now = (self.now)();
            let mut next_wake = None;
            for status in [
                crate::TaskStatus::Pending,
                crate::TaskStatus::Waiting,
                crate::TaskStatus::Sleeping,
            ] {
                let mut cursor = None;
                loop {
                    let page = self
                        .session
                        .scan_tasks(
                            &crate::TaskQuery {
                                status: Some(status),
                                ..Default::default()
                            },
                            256,
                            cursor.as_ref(),
                            ctx,
                        )
                        .await?;
                    for record in page.items {
                        let Some(task) = snapshot.task(&record.kind).cloned() else {
                            continue;
                        };
                        if self.blocked_reason(&record, Some(&task)).is_some()
                            || active.contains(&record.id)
                        {
                            continue;
                        }
                        if let TaskState::Sleeping { until, .. } = &record.state
                            && !record.abort_requested
                            && !crate::signals::sleep_due(&record.state, record.memos.as_ref(), now)
                        {
                            if let Some(until) = until {
                                next_wake = Some(
                                    next_wake.map_or(*until, |current: u64| current.min(*until)),
                                );
                            }
                            continue;
                        }
                        let id = record.id;
                        active.insert(id);
                        let context = ctx.clone();
                        let harness = self.clone();
                        running.spawn(async move {
                            (id, harness.dispatch(id, task, now, &context).await)
                        });
                    }
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            let mut alarm = None;
            if let Some(callback) = &self.on_wake {
                let value = self.wake(ctx).await?;
                self.publish_wake(callback, value);
            } else if let Some(at) = next_wake {
                let wake = self.driver.notify.clone();
                alarm = Some(
                    self.session
                        .timers()
                        .foreground(
                            Box::new(move || wake.notify_one()),
                            std::time::Duration::from_millis(at.saturating_sub((self.now)())),
                        )
                        .map_err(|e| Error::Callback(Box::new(e)))?,
                );
            }
            loop {
                let rescan = ctx
                    .await_with(async {
                        tokio::select! {
                            Some(joined) = running.join_next(), if !running.is_empty() => {
                            let (id,result)=joined.map_err(|e|Error::Callback(Box::new(e)))?;
                                if result.is_ok() { active.remove(&id); }
                                result
                            }
                            () = self.driver.notify.notified() => Ok(true),
                            _ = registry.changed() => Ok(true),

                        }
                    })
                    .await;
                if !matches!(rescan, Ok(Ok(false))) {
                    if let Some(alarm) = alarm.take() {
                        alarm.disarm();
                    }
                    rescan??;
                    break;
                }
            }
        }
    }
}

use super::ownership::{Ownership, Scope};

impl Harness {
    async fn reconcile(&self, ctx: &Context) -> Result<()> {
        let registry = self.registry.snapshot();
        self.session.commit(async |tx|{
            let graph = Ownership::read(tx).await?;
            let mut changes = std::collections::BTreeMap::new();
            for record in graph.tasks.values() {
                let cancelling = record.abort_requested
                    || matches!(&record.state,TaskState::Completing{outcome} if !matches!(outcome,TaskOutcome::Completed{..}));
                if cancelling {
                    for child in graph.tasks.values().filter(|child| {
                        child.id != record.id
                            && (!child.abort_requested || (child.abort_reason.is_some() && record.abort_reason.is_none()))
                            && graph.reaches(Some(Scope::Task(record.id)), Scope::Task(child.id), false)
                    }) {
                        let update = changes.entry(child.id).or_insert_with(|| child.clone());
                        // Explicit cancellation takes precedence over restart abandonment.
                        if !update.abort_requested || record.abort_reason.is_none() {
                            update.abort_requested = true;
                            update.abort_reason = record.abort_reason;
                        }
                    }
                }
                if let TaskState::Waiting {
                    on,
                    policy: crate::JoinPolicy::FailFast,
                    ..
                } = &record.state
                {
                    let mut failed = false;
                    for id in on {
                        if let Some(member) = tx.task(*id).await?
                            && matches!(
                                &member.state,
                                TaskState::Terminal { outcome } | TaskState::Completing { outcome }
                                    if !matches!(outcome, TaskOutcome::Completed { .. })
                            )
                        {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        for child in on
                            .iter()
                            .filter_map(|id| graph.tasks.get(id))
                            .filter(|c| !c.abort_requested || c.abort_reason.is_some())
                        {
                            let update = changes.entry(child.id).or_insert_with(|| child.clone());
                            update.abort_requested = true;
                            update.abort_reason = None;
                        }
                    }
                }
                if let TaskState::Completing { outcome } = &record.state
                    && !graph.has_children(record.id)
                {
                    changes
                        .entry(record.id)
                        .or_insert_with(|| record.clone())
                        .state = TaskState::Terminal {
                        outcome: outcome.clone(),
                    };
                } else if !matches!(
                    record.state,
                    TaskState::Completing { .. } | TaskState::Running { .. }
                ) {
                    let blocked = self.blocked_reason(record, registry.task(&record.kind));
                    let outcome = if let Some(n) = record.restarts.filter(|n| {
                        let limit = (self.settings)()["restartLimit"]
                            .as_u64()
                            .unwrap_or(super::DEFAULT_RESTART_LIMIT);
                        *n >= limit
                    })
                        && (record.abort_requested
                            || !matches!(
                                record.kind.as_str(),
                                "pi.tool" | "pi.generation" | "pi.compaction"
                            )) {
                        let message = if record.abort_requested {
                            format!(
                                "Abort handler of task {} ({}) was cut off {n} times",
                                record.id, record.kind
                            )
                        } else {
                            format!(
                                "Task {} ({}) was cut off {n} times without progress",
                                record.id, record.kind
                            )
                        };
                        Some(TaskOutcome::Faulted {
                            error: TaskOutcomeError {
                                message,
                                detail: Some(serde_json::json!({"reason":"restart_limit"})),
                            },
                        })
                    } else if record.abort_requested && record.abort_reason.is_none() {
                        blocked.map(|reason| TaskOutcome::Orphaned {
                            reason: reason.into(),
                        })
                    } else {
                        None
                    };
                    if let Some(outcome) = outcome {
                        changes
                            .entry(record.id)
                            .or_insert_with(|| record.clone())
                            .state = if graph.has_children(record.id) {
                            TaskState::Completing { outcome }
                        } else {
                            TaskState::Terminal { outcome }
                        };
                    }
                }
            }
            for update in changes.into_values() {
                if let TaskState::Terminal { outcome } = &update.state {
                    super::live::settle_scheduler_outcome(tx, &update, outcome).await?;
                }
                tx.set_task(update)?;
            }
            Ok(())
        },ctx).await
    }

    async fn idle_scope(&self, id: Option<crate::ConversationId>, ctx: &Context) -> Result<()> {
        self.resume()?;
        let notify = Arc::new(tokio::sync::Notify::new());
        let wake = notify.clone();
        let _commit = self.follow_commits(move |_, _| {
            wake.notify_one();
            Ok(())
        })?;
        let wake = notify.clone();
        let _close = self.subscribe_close(move || wake.notify_one())?;
        loop {
            let idle = self
                .session
                .commit(
                    async |tx| {
                        let graph = Ownership::read(tx).await?;
                        Ok(!graph.tasks.keys().any(|&task| {
                            graph.reaches(id.map(Scope::Conversation), Scope::Task(task), false)
                        }))
                    },
                    ctx,
                )
                .await?;
            if idle {
                return Ok(());
            }
            ctx.await_with(notify.notified()).await?;
        }
    }
    pub async fn wait_for_idle(&self, ctx: &Context) -> Result<()> {
        self.idle_scope(None, ctx).await
    }
}
impl super::Conversation {
    pub async fn wait_for_idle(&self, ctx: &Context) -> Result<()> {
        self.harness.idle_scope(Some(self.id), ctx).await
    }
    pub async fn abort(&self, background: bool, ctx: &Context) -> Result<()> {
        self.harness.resume()?;
        let marked = self
            .session
            .commit(
                async |tx| {
                    let graph = Ownership::read(tx).await?;
                    let mut cursor = None;
                    let mut queued = std::collections::BTreeSet::new();
                    loop {
                        let page = tx
                            .scan_submissions(
                                &crate::SubmissionQuery {
                                    status: Some(crate::SubmissionStatus::Queued),
                                    ..Default::default()
                                },
                                256,
                                cursor.as_ref(),
                            )
                            .await?;
                        queued.extend(page.items.into_iter().map(|s| s.conversation_id).filter(
                            |id| {
                                graph.reaches(
                                    Some(Scope::Conversation(self.id)),
                                    Scope::Conversation(*id),
                                    background,
                                )
                            },
                        ));
                        cursor = page.next;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    for conversation in queued {
                        super::inbox::withdraw_inputs(tx, conversation, "aborted", true).await?;
                    }
                    let mut ids = Vec::new();
                    for task in graph.tasks.values().filter(|r| {
                        graph.reaches(
                            Some(Scope::Conversation(self.id)),
                            Scope::Task(r.id),
                            background,
                        )
                    }) {
                        ids.push(task.id);
                        if !task.abort_requested || task.abort_reason.is_some() {
                            let mut record = task.clone();
                            record.abort_requested = true;
                            record.abort_reason = None;
                            tx.set_task(record)?;
                        }
                    }
                    Ok(ids)
                },
                ctx,
            )
            .await?;
        if background {
            for id in marked {
                self.harness.wait_for_task(id, ctx).await?;
            }
        }
        self.wait_for_idle(ctx).await
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Wake {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}
#[derive(Debug, serde::Serialize)]
pub struct TaskInspection {
    pub record: TaskRecord,
    pub state: Value,
}
#[derive(Debug, serde::Serialize)]
pub struct HarnessInspection {
    pub scheduling: &'static str,
    pub tasks: Vec<TaskInspection>,
    pub submissions: Vec<crate::SubmissionRecord>,
}

impl Harness {
    pub async fn inspect(&self, ctx: &Context) -> Result<HarnessInspection> {
        self.session
            .commit(
                async |tx| {
                    let scheduling = {
                        let driver = self.driver.state.lock().unwrap_or_else(|e| e.into_inner());
                        if driver.closing {
                            "closing"
                        } else if driver.active.is_some() {
                            "running"
                        } else {
                            "paused"
                        }
                    };
                    let graph = Ownership::read(tx).await?;
                    let registry = self.registry.snapshot();
                    let mut tasks = Vec::new();
                    let now = (self.now)();
                    for record in graph.tasks.values() {
                        let definition = registry.task(&record.kind);
                        let blocked = self.blocked_reason(record, definition);
                        let pending = || {
                            serde_json::json!({
                                "kind": "pending",
                                "migrates": definition.is_some_and(|t|t.version()>record.version)
                            })
                        };
                        let state = if let Some(reason) = blocked {
                            serde_json::json!({"kind":"blocked","reason":reason})
                        } else {
                            match &record.state {
                                TaskState::Running { .. } => serde_json::json!({"kind":"running"}),
                                TaskState::Completing { .. } => {
                                    serde_json::json!({"kind":"completing"})
                                }
                                TaskState::Waiting { on, policy, .. }
                                    if !record.abort_requested =>
                                {
                                    let live: Vec<_> = on
                                        .iter()
                                        .filter(|id| graph.tasks.contains_key(id))
                                        .collect();
                                    if live.is_empty()
                                        || (*policy == crate::JoinPolicy::Any
                                            && live.len() < on.len())
                                    {
                                        pending()
                                    } else {
                                        serde_json::json!({"kind":"waiting","on":live})
                                    }
                                }
                                TaskState::Sleeping { until, .. }
                                    if !record.abort_requested
                                        && !crate::signals::sleep_due(
                                            &record.state,
                                            record.memos.as_ref(),
                                            now,
                                        ) =>
                                {
                                    let mut s = serde_json::json!({"kind":"sleeping"});
                                    if let Some(at) = until {
                                        s["until"] = serde_json::json!(at);
                                    }
                                    s
                                }
                                _ => pending(),
                            }
                        };
                        tasks.push(TaskInspection {
                            record: record.clone(),
                            state,
                        });
                    }
                    let mut submissions = Vec::new();
                    for status in [
                        crate::SubmissionStatus::Queued,
                        crate::SubmissionStatus::Placed,
                    ] {
                        let mut cursor = None;
                        loop {
                            let page = tx
                                .scan_submissions(
                                    &crate::SubmissionQuery {
                                        status: Some(status),
                                        ..Default::default()
                                    },
                                    256,
                                    cursor.as_ref(),
                                )
                                .await?;
                            submissions.extend(page.items);
                            cursor = page.next;
                            if cursor.is_none() {
                                break;
                            }
                        }
                    }
                    submissions.sort_by_key(|s| s.id);
                    Ok(HarnessInspection {
                        scheduling,
                        tasks,
                        submissions,
                    })
                },
                ctx,
            )
            .await
    }
    pub async fn wake(&self, ctx: &Context) -> Result<Wake> {
        let mut wake = Wake::default();
        if !self.read_only {
            for task in self.inspect(ctx).await?.tasks {
                match task.state["kind"].as_str() {
                    Some("pending" | "running") => wake.active = true,
                    Some("sleeping") => {
                        if let Some(at) = task.state["until"].as_u64() {
                            wake.at = Some(wake.at.map_or(at, |t| t.min(at)));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(wake)
    }
}

impl Invocation {
    pub fn harness(&self) -> &Harness {
        &self.harness
    }
    pub async fn memo(
        &self,
        name: &str,
        candidate: Option<Value>,
        ctx: &Context,
    ) -> Result<Option<Value>> {
        let id = self.record.id;
        self.harness
            .session
            .commit(
                async |tx| {
                    let current = tx
                        .task(id)
                        .await?
                        .ok_or_else(|| Error::invalid(format!("Task {id} does not exist")))?;
                    if current.state != self.record.state
                        || current.abort_requested != self.record.abort_requested
                    {
                        return Err(Error::invalid(format!("Task {id} invocation ended")));
                    }
                    if let Some(value) = current.memos.as_ref().and_then(|m| m.get(name)) {
                        return Ok(Some(value.clone()));
                    }
                    if let Some(value) = candidate {
                        let mut record = current.as_ref().clone();
                        record
                            .memos
                            .get_or_insert_with(Default::default)
                            .insert(name.into(), value.clone());
                        tx.set_task(record)?;
                        return Ok(Some(value));
                    }
                    Ok(None)
                },
                ctx,
            )
            .await
    }
    pub async fn read_signal(
        &self,
        token: &crate::SignalToken,
        key: &str,
        ctx: &Context,
    ) -> Result<Option<crate::SignalState>> {
        let record = self.harness.get_task(self.record.id, ctx).await?;
        crate::signals::read_signal(record.as_ref().and_then(|r| r.memos.as_ref()), token, key)
            .map_err(|e| Error::invalid(e.to_string()))
    }
}
