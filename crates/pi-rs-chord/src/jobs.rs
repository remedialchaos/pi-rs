//! Owned jobs: committed intent, external work, retried finish and crash recovery.
use crate::{
    Error, Result,
    context::{Cancel, Context, lock},
    data::{ChordError, Validator},
    json::JsonValue,
    state::{Batch, NativeSource},
    sync::SyncFuture,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::watch;
mod persistence;
pub use persistence::JobStore;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Rerun,
    Abandon,
}
#[derive(Clone)]
pub struct JobOptions {
    pub version: u64,
    pub args: JsonValue,
    pub result: JsonValue,
    pub state: Option<Arc<crate::documents::Definition>>,
    pub concurrency: Option<usize>,
    pub persist: Option<Resume>,
}
impl JobOptions {
    pub fn new(version: u64, args: JsonValue, result: JsonValue) -> Self {
        Self {
            version,
            args,
            result,
            state: None,
            concurrency: None,
            persist: None,
        }
    }
}
#[derive(Clone)]
pub struct JobDefinition {
    pub name: String,
    pub version: u64,
    pub fingerprint: String,
    pub args: crate::contracts::Snapshot,
    pub result: crate::contracts::Snapshot,
    pub state: Option<Arc<crate::documents::Definition>>,
    pub concurrency: Option<usize>,
    pub persist: Option<Resume>,
}
pub fn define_job(name: impl Into<String>, options: JobOptions) -> Result<JobDefinition> {
    let name = name.into();
    crate::contracts::check_name(&name, "A job name")?;
    if options.version == 0
        || options.version > 9_007_199_254_740_991
        || options.concurrency == Some(0)
    {
        return Err(Error::Invalid(format!(
            "{name}: version and concurrency are positive integers"
        )));
    }
    let args = crate::contracts::snapshot_arguments(&options.args, &format!("{name}.args"))?;
    let result = crate::contracts::snapshot_schema(&options.result, &format!("{name}.result"))?;
    if args.bytes || (options.persist.is_some() && result.bytes) {
        return Err(Error::Invalid(format!(
            "{name}: stored job arguments and results are JSON, no bytes"
        )));
    }
    if let Some(state) = &options.state
        && (!state.is_singleton() || options.persist.is_some() && !state.is_persisted())
    {
        return Err(Error::Invalid(format!(
            "{name}.state: expected a singleton document, persisted with its job"
        )));
    }
    let normal = serde_json::json!({"args":args.normal,"result":result.normal,"state":options.state.as_ref().map(|s|s.fingerprint())});
    Ok(JobDefinition {
        name,
        version: options.version,
        fingerprint: crate::contracts::hash64(&normal.to_string()),
        args,
        result,
        state: options.state,
        concurrency: options.concurrency,
        persist: options.persist,
    })
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobError {
    Threw { error: serde_json::Value },
    Aborted { reason: String },
    RestartLimit { restarts: u32 },
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum JobResult {
    Done { result: JsonValue },
    Failed { error: JobError },
}
impl<'de> Deserialize<'de> for JobResult {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value.get("kind").and_then(serde_json::Value::as_str) {
            Some("done") => value
                .get("result")
                .cloned()
                .map(|result| Self::Done {
                    result: JsonValue::from_serde(result),
                })
                .ok_or_else(|| serde::de::Error::custom("done outcome needs result")),
            Some("failed") => serde_json::from_value(
                value
                    .get("error")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::custom("failed outcome needs error"))?,
            )
            .map(|error| Self::Failed { error })
            .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("unknown job outcome kind")),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub args: JsonValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<JsonValue>,
    #[serde(flatten)]
    pub outcome: JobResult,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Pending,
    Running,
    Stopped,
    Finishing,
    Blocked,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunningEntry {
    pub id: String,
    pub version: u64,
    pub key: Option<String>,
    pub args: JsonValue,
    pub state: Option<JsonValue>,
    pub phase: Phase,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredRun {
    pub version: u64,
    pub fingerprint: String,
    pub args: JsonValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub phase: Phase,
    pub restarts: u32,
    pub finish_failures: u32,
    #[serde(rename = "abort", default, skip_serializing_if = "std::ops::Not::not")]
    pub abort_marked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<JobResult>,
}
#[derive(Clone)]
pub struct Run {
    pub id: String,
    answer: watch::Receiver<Option<std::result::Result<Outcome, ChordError>>>,
}
impl Run {
    pub async fn outcome(&self, context: &Context) -> Result<Outcome> {
        let mut answer = self.answer.clone();
        context
            .await_with(async move {
                loop {
                    if let Some(answer) = answer.borrow_and_update().clone() {
                        return answer.map_err(Error::from);
                    }
                    answer.changed().await.map_err(|_| Error::Closed)?;
                }
            })
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))?
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortStatus {
    Marked,
    Terminal,
}
type RunFn = Arc<dyn Fn(JsonValue, JobContext) -> SyncFuture<JsonValue> + Send + Sync>;
type FinishStep = Arc<dyn Fn(&Outcome, &mut crate::store::Step) -> Result<()> + Send + Sync>;
type FinishFn = Arc<dyn Fn(Outcome) -> SyncFuture<()> + Send + Sync>;
#[derive(Clone)]
pub struct JobImplementation {
    run: RunFn,
    finish: Option<FinishFn>,
    finish_step: Option<FinishStep>,
}
impl JobImplementation {
    pub fn new(
        run: impl Fn(JsonValue, JobContext) -> SyncFuture<JsonValue> + Send + Sync + 'static,
    ) -> Self {
        Self {
            run: Arc::new(run),
            finish: None,
            finish_step: None,
        }
    }
    /// Stage document or table writes in the same SQLite transaction as retiring the run.
    pub fn with_transactional_finish(
        mut self,
        finish: impl Fn(&Outcome, &mut crate::store::Step) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.finish_step = Some(Arc::new(finish));
        self.finish = None;
        self
    }
    pub fn with_finish(
        mut self,
        finish: impl Fn(Outcome) -> SyncFuture<()> + Send + Sync + 'static,
    ) -> Self {
        self.finish_step = None;
        self.finish = Some(Arc::new(finish));
        self
    }
}
struct Record {
    stored: StoredRun,
    state: Option<JsonValue>,
    answer: watch::Sender<Option<std::result::Result<Outcome, ChordError>>>,
    cancel: Option<Cancel>,
    generation: u64,
    over: watch::Sender<bool>,
}
struct State {
    live: bool,
    closed: bool,
    generation: u64,
    runs: IndexMap<String, Record>,
}
struct Inner {
    definition: JobDefinition,
    implementation: JobImplementation,
    validator: Validator,
    state: Mutex<State>,
    line: tokio::sync::Mutex<()>,
    store: Option<JobStore>,
    running: NativeSource<Vec<RunningEntry>>,
    stop: Context,
    cancel: Cancel,
    adapters: BTreeMap<u64, JobAdapter>,
}
#[derive(Clone)]
pub struct Job(Arc<Inner>);
pub type JobMapping = Arc<dyn Fn(&JsonValue) -> Result<JsonValue> + Send + Sync>;
#[derive(Clone)]
pub struct JobAdapter {
    pub args: JobMapping,
    pub result: JobMapping,
}
#[derive(Clone)]
pub struct JobContext {
    pub context: Context,
    pub job_id: String,
    owner: Weak<Inner>,
    generation: u64,
}
impl JobContext {
    pub fn state(&self) -> Result<Option<JsonValue>> {
        let inner = self.owner.upgrade().ok_or(Error::Closed)?;
        let state = lock(&inner.state);
        let record = state
            .runs
            .get(&self.job_id)
            .filter(|r| r.generation == self.generation && r.stored.phase == Phase::Running)
            .ok_or_else(settled)?;
        Ok(record.state.clone())
    }
    pub async fn change_state<T>(
        &self,
        change: impl FnOnce(&mut JsonValue) -> Result<T>,
    ) -> Result<T> {
        let inner = self.owner.upgrade().ok_or(Error::Closed)?;
        let job = Job(inner);
        let _line = job.0.line.lock().await;
        let (mut value, mut stored) = {
            let state = lock(&job.0.state);
            let run = state
                .runs
                .get(&self.job_id)
                .filter(|r| {
                    r.generation == self.generation
                        && r.stored.phase == Phase::Running
                        && !state.closed
                })
                .ok_or_else(settled)?;
            (
                run.state
                    .clone()
                    .ok_or_else(|| Error::Invalid("job has no state document".into()))?,
                run.stored.clone(),
            )
        };
        let result = change(&mut value)?;
        if let Some(definition) = &job.0.definition.state {
            definition.validate(&value)?;
        }
        stored.restarts = 0;
        job.persist(&self.job_id, &stored, Some(&value)).await?;
        if let Some(run) = lock(&job.0.state).runs.get_mut(&self.job_id) {
            run.state = Some(value);
            run.stored = stored;
        }
        job.publish()?;
        Ok(result)
    }
}
impl Job {
    pub async fn new(
        definition: JobDefinition,
        implementation: JobImplementation,
        batch: Batch,
        store: Option<JobStore>,
    ) -> Result<Self> {
        Self::with_adapters(definition, implementation, batch, store, BTreeMap::new()).await
    }
    pub async fn with_adapters(
        definition: JobDefinition,
        implementation: JobImplementation,
        batch: Batch,
        store: Option<JobStore>,
        adapters: BTreeMap<u64, JobAdapter>,
    ) -> Result<Self> {
        if definition.persist.is_some() && store.is_none() {
            return Err(Error::Invalid(
                "persisted job requires an owner store".into(),
            ));
        }
        if implementation.finish_step.is_some() && definition.persist.is_none() {
            return Err(Error::Invalid(
                "transactional finish requires a persisted job".into(),
            ));
        }
        if adapters.keys().any(|v| *v == 0 || *v >= definition.version) {
            return Err(Error::Invalid("job accepts only older versions".into()));
        }
        let validator = Validator::compile(&serde_json::to_value(&definition.args.schema)?)?;
        let (stop, cancel) = Context::background().with_cancel();
        let loaded = if definition.persist.is_some() {
            match &store {
                Some(s) => {
                    let version = definition.version;
                    let fingerprint = definition.fingerprint.clone();
                    let accepted = adapters.keys().copied().collect::<Vec<_>>();
                    s.load_versions(&definition.name, move |record| {
                        (record.version == version && record.fingerprint == fingerprint)
                            || accepted.contains(&record.version)
                    })
                    .await?
                }
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let running = NativeSource::new(batch, Vec::new(), |a, b| a == b);
        let job = Self(Arc::new(Inner {
            definition,
            implementation,
            validator,
            state: Mutex::new(State {
                live: false,
                closed: false,
                generation: 1,
                runs: IndexMap::new(),
            }),
            line: tokio::sync::Mutex::new(()),
            store,
            running,
            stop,
            cancel,
            adapters,
        }));
        for (id, mut stored, state) in loaded {
            let compatible = stored.version == job.0.definition.version
                && stored.fingerprint == job.0.definition.fingerprint;
            if !compatible {
                if let Some(adapter) = job.0.adapters.get(&stored.version) {
                    stored.args = (adapter.args)(&stored.args)?;
                    if let Some(JobResult::Done { result }) = &mut stored.outcome {
                        *result = (adapter.result)(result)?;
                    }
                    stored.version = job.0.definition.version;
                    stored.fingerprint = job.0.definition.fingerprint.clone();
                } else {
                    stored.phase = Phase::Blocked;
                    stored.blocked = Some("version".into());
                }
            }
            if stored.blocked.as_deref() == Some("version")
                && stored.version == job.0.definition.version
                && stored.fingerprint == job.0.definition.fingerprint
            {
                stored.blocked = None;
                stored.phase = if stored.outcome.is_some() {
                    Phase::Finishing
                } else {
                    Phase::Pending
                };
            }
            let state = match (state, job.0.definition.state.as_ref()) {
                (Some((version, fingerprint, value)), Some(definition))
                    if stored.blocked.as_deref() != Some("version") =>
                {
                    if version > definition.version()
                        || (version == definition.version()
                            && fingerprint != definition.fingerprint())
                    {
                        return Err(Error::Invalid(format!(
                            "{}: incompatible stored state version {version}",
                            definition.name()
                        )));
                    }
                    let value = if version < definition.version() {
                        definition.migrate(value, version)?
                    } else {
                        definition.validate(&value)?;
                        value
                    };
                    Some(value)
                }
                (other, _) => other.map(|(_, _, value)| value),
            };
            if stored.phase == Phase::Running {
                stored.restarts += 1;
            }
            if stored.phase != Phase::Blocked && stored.phase != Phase::Finishing {
                if stored.restarts >= 5 {
                    stored.outcome = Some(JobResult::Failed {
                        error: JobError::RestartLimit {
                            restarts: stored.restarts,
                        },
                    });
                    stored.phase = Phase::Finishing;
                } else if stored.abort_marked || job.0.definition.persist == Some(Resume::Abandon) {
                    stored.outcome = Some(JobResult::Failed {
                        error: JobError::Aborted {
                            reason: if stored.abort_marked {
                                "abort"
                            } else {
                                "restart"
                            }
                            .into(),
                        },
                    });
                    stored.phase = Phase::Finishing;
                } else {
                    stored.phase = Phase::Pending;
                }
            }
            let (answer, _) = watch::channel(None);
            let (over, _) = watch::channel(true);
            lock(&job.0.state).runs.insert(
                id,
                Record {
                    stored,
                    state,
                    answer,
                    cancel: None,
                    generation: 1,
                    over,
                },
            );
        }
        job.publish()?;
        Ok(job)
    }
    pub fn running_source(&self) -> NativeSource<Vec<RunningEntry>> {
        self.0.running.clone()
    }
    pub fn running(&self) -> Vec<RunningEntry> {
        self.0.running.value()
    }
    fn publish(&self) -> Result<()> {
        let entries = lock(&self.0.state)
            .runs
            .iter()
            .map(|(id, r)| RunningEntry {
                id: id.clone(),
                version: r.stored.version,
                key: r.stored.key.clone(),
                args: r.stored.args.clone(),
                state: r.state.clone(),
                phase: r.stored.phase,
                reason: r.stored.blocked.clone(),
            })
            .collect();
        self.0.running.set(entries, &Context::background())
    }
    pub fn live(&self) -> Result<()> {
        {
            let mut state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            if state.live {
                return Ok(());
            }
            state.live = true;
        }
        let ids = lock(&self.0.state).runs.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.spawn(id)?;
        }
        Ok(())
    }
    /// Retry settled work whose finish callback failed, without rerunning the job body.
    pub async fn retry_blocked(&self, context: &Context) -> Result<()> {
        let _line = self.0.line.lock().await;
        let retry = {
            let state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            state
                .runs
                .iter()
                .filter(|(_, r)| {
                    r.stored.phase == Phase::Blocked
                        && r.stored.blocked.as_deref() == Some("finish_failed")
                })
                .map(|(id, r)| (id.clone(), r.stored.clone(), r.state.clone()))
                .collect::<Vec<_>>()
        };
        for (id, mut stored, value) in retry {
            context
                .check()
                .map_err(|e| ChordError::new("aborted", e.to_string()))?;
            stored.phase = Phase::Finishing;
            stored.finish_failures = 0;
            stored.blocked = None;
            self.persist(&id, &stored, value.as_ref()).await?;
            let live = {
                let mut state = lock(&self.0.state);
                if let Some(run) = state.runs.get_mut(&id) {
                    run.stored = stored;
                }
                state.live
            };
            if live {
                self.spawn(id)?;
            }
        }
        self.publish()
    }

    pub async fn start(
        &self,
        args: JsonValue,
        key: Option<String>,
        context: &Context,
    ) -> Result<Run> {
        context
            .check()
            .map_err(|e| ChordError::new("aborted", e.to_string()))?;
        self.0
            .validator
            .check(&serde_json::to_value(&args)?, "args")?;
        let _line = self.0.line.lock().await;
        {
            let state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            if !state.live {
                return Err(ChordError::new("not_ready", "job's object is not live yet").into());
            }
            if let Some(key) = &key
                && let Some((id, run)) = state
                    .runs
                    .iter()
                    .find(|(_, r)| r.stored.key.as_ref() == Some(key))
            {
                return Ok(Run {
                    id: id.clone(),
                    answer: run.answer.subscribe(),
                });
            }
            let count = state
                .runs
                .values()
                .filter(|r| r.stored.phase != Phase::Blocked)
                .count();
            if self
                .0
                .definition
                .concurrency
                .is_some_and(|limit| count >= limit)
            {
                let mut e = ChordError::new("unavailable", "job concurrency limit reached");
                e.reason = Some("busy".into());
                return Err(e.into());
            }
        }
        let id = uuid_v7()?;
        let stored = StoredRun {
            version: self.0.definition.version,
            fingerprint: self.0.definition.fingerprint.clone(),
            args,
            key,
            phase: Phase::Pending,
            restarts: 0,
            finish_failures: 0,
            abort_marked: false,
            blocked: None,
            outcome: None,
        };
        let value = self
            .0
            .definition
            .state
            .as_ref()
            .map(|s| s.initial())
            .transpose()?;
        self.persist(&id, &stored, value.as_ref()).await?;
        let (answer, receiver) = watch::channel(None);
        let (over, _) = watch::channel(true);
        let generation = lock(&self.0.state).generation;
        lock(&self.0.state).runs.insert(
            id.clone(),
            Record {
                stored,
                state: value,
                answer,
                cancel: None,
                generation,
                over,
            },
        );
        self.publish()?;
        self.spawn(id.clone())?;
        Ok(Run {
            id,
            answer: receiver,
        })
    }
    pub async fn outcome(&self, id: &str, context: &Context) -> Result<Outcome> {
        let answer = lock(&self.0.state)
            .runs
            .get(id)
            .map(|r| r.answer.subscribe())
            .ok_or_else(|| ChordError::new("gone", "unknown or settled job run"))?;
        Run {
            id: id.into(),
            answer,
        }
        .outcome(context)
        .await
    }
    pub async fn abort(&self, id: &str, context: &Context) -> Result<AbortStatus> {
        let line = self.0.line.lock().await;
        let Some((mut stored, value, mut over, cancel)) =
            lock(&self.0.state).runs.get(id).map(|r| {
                (
                    r.stored.clone(),
                    r.state.clone(),
                    r.over.subscribe(),
                    r.cancel.clone(),
                )
            })
        else {
            return Ok(AbortStatus::Terminal);
        };
        if stored.phase == Phase::Blocked {
            if let Some(store) = &self.0.store {
                store.remove(&self.0.definition.name, id).await?;
            }
            if let Some(run) = lock(&self.0.state).runs.shift_remove(id) {
                run.answer.send_replace(Some(Err(ChordError::new(
                    "aborted",
                    "blocked job was dropped",
                ))));
            }
            self.publish()?;
            return Ok(AbortStatus::Marked);
        }
        stored.abort_marked = true;
        self.persist(id, &stored, value.as_ref()).await?;
        if let Some(run) = lock(&self.0.state).runs.get_mut(id) {
            run.stored = stored;
        }
        if let Some(cancel) = cancel {
            cancel.cancel("job aborted");
        }
        drop(line);
        context
            .await_with(async move {
                while !*over.borrow_and_update() {
                    if over.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))?;
        Ok(AbortStatus::Marked)
    }
    fn spawn(&self, id: String) -> Result<()> {
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|e| Error::Invalid(e.to_string()))?;
        let job = self.clone();
        runtime.spawn(async move {
            if let Err(error) = job.execute(&id).await
                && let Some(run) = lock(&job.0.state).runs.get(id.as_str())
            {
                run.answer.send_replace(Some(Err(as_chord(error))));
                run.over.send_replace(true);
            }
        });
        Ok(())
    }
    async fn persist(&self, id: &str, run: &StoredRun, state: Option<&JsonValue>) -> Result<()> {
        if self.0.definition.persist.is_some()
            && let Some(store) = &self.0.store
        {
            store
                .save_defined(
                    &self.0.definition.name,
                    id,
                    run,
                    state,
                    self.0.definition.state.as_deref(),
                )
                .await?;
        }
        Ok(())
    }
    async fn execute(&self, id: &str) -> Result<()> {
        let line = self.0.line.lock().await;
        let Some((mut stored, value, generation)) = lock(&self.0.state)
            .runs
            .get(id)
            .map(|r| (r.stored.clone(), r.state.clone(), r.generation))
        else {
            return Ok(());
        };
        if stored.phase == Phase::Blocked || lock(&self.0.state).closed {
            return Ok(());
        }
        let mut outcome = stored.outcome.clone();
        if outcome.is_none() {
            stored.phase = Phase::Running;
            self.persist(id, &stored, value.as_ref()).await?;
            let (context, cancel) = self.0.stop.with_cancel();
            if stored.abort_marked {
                cancel.cancel("job aborted");
            }
            if let Some(run) = lock(&self.0.state).runs.get_mut(id) {
                run.stored = stored.clone();
                run.cancel = Some(cancel);
                run.over.send_replace(false);
            }
            self.publish()?;
            drop(line);
            let ctx = JobContext {
                context: context.clone(),
                job_id: id.into(),
                owner: Arc::downgrade(&self.0),
                generation,
            };
            let work = (self.0.implementation.run)(stored.args.clone(), ctx);
            let result = tokio::select! {biased;_=self.0.stop_cancelled()=>return Ok(()), result=work=>result};
            outcome = Some(match result {
                Ok(result) => JobResult::Done { result },
                Err(error) => {
                    if context.is_aborted() {
                        JobResult::Failed {
                            error: JobError::Aborted {
                                reason: "abort".into(),
                            },
                        }
                    } else {
                        JobResult::Failed {
                            error: JobError::Threw {
                                error: wire_error(error),
                            },
                        }
                    }
                }
            });
        } else {
            drop(line);
        }
        let Some(outcome) = outcome else {
            return Ok(());
        };
        let line = self.0.line.lock().await;
        let value = {
            let state = lock(&self.0.state);
            if state.closed {
                return Ok(());
            }
            let Some(run) = state.runs.get(id).filter(|r| r.generation == generation) else {
                return Ok(());
            };
            run.state.clone()
        };
        stored.phase = Phase::Finishing;
        stored.outcome = Some(outcome.clone());
        self.persist(id, &stored, value.as_ref()).await?;
        if let Some(run) = lock(&self.0.state).runs.get_mut(id) {
            run.stored = stored.clone();
            run.cancel = None;
            run.over.send_replace(true);
        }
        self.publish()?;
        drop(line);
        let answer = Outcome {
            id: id.into(),
            args: stored.args.clone(),
            state: value,
            outcome,
        };
        loop {
            if lock(&self.0.state).closed {
                return Ok(());
            }
            let finished = match &self.0.implementation.finish {
                Some(finish) => {
                    tokio::select! {biased;_=self.0.stop_cancelled()=>return Ok(()),result=finish(answer.clone())=>result}
                }
                None => Ok(()),
            };
            let line = self.0.line.lock().await;
            if lock(&self.0.state).closed {
                return Ok(());
            }
            let finished = match (finished, &self.0.store) {
                (Ok(()), Some(store)) if self.0.definition.persist.is_some() => {
                    store
                        .retire(
                            &self.0.definition.name,
                            id,
                            self.0
                                .implementation
                                .finish_step
                                .clone()
                                .map(|finish| (finish, answer.clone())),
                        )
                        .await
                }
                (other, _) => other,
            };
            match finished {
                Ok(()) => {
                    if let Some(record) = lock(&self.0.state).runs.shift_remove(id) {
                        record.answer.send_replace(Some(Ok(answer)));
                    }
                    self.publish()?;
                    return Ok(());
                }
                Err(_) => {
                    stored.finish_failures += 1;
                    if stored.finish_failures >= 5 {
                        stored.phase = Phase::Blocked;
                        stored.blocked = Some("finish_failed".into());
                    }
                    self.persist(id, &stored, answer.state.as_ref()).await?;
                    if let Some(run) = lock(&self.0.state).runs.get_mut(id) {
                        run.stored = stored.clone();
                    }
                    self.publish()?;
                    if stored.phase == Phase::Blocked {
                        return Ok(());
                    }
                    drop(line);
                    let delay = Duration::from_millis(next_delay(Some(
                        500_u64.saturating_mul(1_u64 << (stored.finish_failures - 1).min(10)),
                    )));
                    tokio::select! {biased;_=self.0.stop_cancelled()=>return Ok(()),_=tokio::time::sleep(delay)=>{}}
                }
            }
        }
    }
    pub async fn close(&self, reason: &str) -> Result<()> {
        let _line = self.0.line.lock().await;
        {
            let mut state = lock(&self.0.state);
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            state.live = false;
            state.generation += 1;
        }
        self.0.cancel.cancel(reason);
        let records = lock(&self.0.state)
            .runs
            .iter()
            .map(|(id, r)| (id.clone(), r.stored.clone(), r.state.clone()))
            .collect::<Vec<_>>();
        for (id, mut run, state) in records {
            if self.0.definition.persist.is_some() && run.blocked.as_deref() != Some("version") {
                if run.phase == Phase::Running || run.phase == Phase::Pending {
                    run.phase = Phase::Stopped;
                }
                self.persist(&id, &run, state.as_ref()).await?;
            }
        }
        for record in lock(&self.0.state).runs.drain(..).map(|(_, r)| r) {
            if let Some(cancel) = record.cancel {
                cancel.cancel(reason);
            }
            record.over.send_replace(true);
            record
                .answer
                .send_replace(Some(Err(ChordError::new("released", reason))));
        }
        self.0.running.detach("released/closed");
        Ok(())
    }
}
impl Inner {
    async fn stop_cancelled(&self) {
        if let Some(signal) = self.stop.abort_signal() {
            signal.cancelled().await;
        }
    }
}
fn settled() -> Error {
    let mut error = ChordError::new("released", "job run settled; its state is closed");
    error.reason = Some("settled".into());
    error.into()
}
pub fn next_delay(previous: Option<u64>) -> u64 {
    previous.map_or(1_000, |delay| delay.saturating_mul(2).min(60_000))
}
fn as_chord(error: Error) -> ChordError {
    match error {
        Error::Chord(e) => *e,
        other => ChordError::new("unavailable", other.to_string()),
    }
}
fn wire_error(error: Error) -> serde_json::Value {
    match error {
        Error::Chord(e) => serde_json::json!({"kind":"chord","error":e}),
        other => serde_json::json!({"kind":"plugin","code":"internal","message":other.to_string()}),
    }
}

/// Time-ordered identities remain monotonic when the wall clock moves backwards.
pub fn uuid_v7() -> Result<String> {
    static CLOCK: std::sync::OnceLock<Mutex<(u64, Option<u64>)>> = std::sync::OnceLock::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| Error::Invalid(e.to_string()))?
        .as_millis();
    let mut bytes = rand::random::<[u8; 16]>();
    let mut clock = lock(CLOCK.get_or_init(|| Mutex::new((0, None))));
    clock.0 = clock
        .0
        .max(u64::try_from(now).map_err(|_| Error::Invalid("UUID time overflow".into()))?);
    let sequence = match clock.1 {
        Some(previous) => previous
            .checked_add(1)
            .filter(|v| *v < (1 << 41))
            .ok_or_else(|| Error::Invalid("UUIDv7 generator sequence exhausted".into()))?,
        None => bytes[1..6]
            .iter()
            .fold(0_u64, |a, b| (a << 8) | u64::from(*b)),
    };
    clock.1 = Some(sequence);
    bytes[..6].copy_from_slice(&clock.0.to_be_bytes()[2..]);
    bytes[6] = 0x70 | ((sequence >> 37) & 15) as u8;
    bytes[7] = (sequence >> 29) as u8;
    bytes[8] = 0x80 | ((sequence >> 23) & 63) as u8;
    bytes[9] = (sequence >> 15) as u8;
    bytes[10] = (sequence >> 7) as u8;
    bytes[11] = ((sequence & 127) << 1) as u8 | (bytes[11] & 1);
    Ok(uuid::Uuid::from_bytes(bytes).to_string())
}
