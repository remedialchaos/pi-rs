use std::sync::Arc;

use async_trait::async_trait;
use pi_rs_ai::registry::Models;
use pi_rs_chord::{context::Context, delta::Op};

use super::{AGENT_DOC, EntryLoader, INBOX_DOC, LIVE_DOC, PROVIDER_DOC, USAGE_DOC};
use crate::{
    ConversationId, ConversationOwnership, EntryId, EntryQuery, EntryRecord, Page,
    ROOT_CONVERSATION_ID, Session, Storage,
    documents::resolve_address,
    session::{Result, SessionOptions},
};

pub struct EnvTarget {
    pub conversation_id: ConversationId,
    pub cwd: Option<String>,
    pub read: Session,
}
pub type BuildEnv = Arc<
    dyn for<'a> Fn(
            EnvTarget,
            &'a Context,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<Option<Arc<dyn crate::env::ExecutionEnv>>>,
        > + Send
        + Sync,
>;
/// Errors from one extension hook, with rejected requests in their original order.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HookError {
    #[error("{0}")]
    Single(String),
    #[error("{message}")]
    Aggregate {
        message: String,
        errors: Vec<String>,
    },
}

pub type HookReporter = Arc<dyn Fn(&str, &HookError) + Send + Sync>;

/// `HarnessOptions.models`: the host's model registry, plain or as a selector that the Harness
/// reads at each use, with the Harness identity applied.
#[derive(Clone)]
pub enum ModelsInput {
    Static(Models),
    Dynamic(Arc<dyn Fn() -> Models + Send + Sync>),
}
impl Default for ModelsInput {
    fn default() -> Self {
        Self::Static(Models::default())
    }
}
impl From<Models> for ModelsInput {
    fn from(models: Models) -> Self {
        Self::Static(models)
    }
}

/// One report of a host-clock failure: the message, and a clock handle that answers the system
/// fallback, so a handler that reads the clock again cannot report again.
pub type ClockReport = Arc<dyn Fn(&str, &dyn Fn() -> u64) + Send + Sync>;

/// The host clock, falling back to the system clock when it panics: a broken clock must not
/// stop commits. The first failure is reported; the clock is read in every task change, so
/// later ones are not. A panicking report handler is dropped, as reporting it would report
/// again.
pub fn safe_now(
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    report: ClockReport,
) -> Arc<dyn Fn() -> u64 + Send + Sync> {
    struct SafeClock {
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
        report: ClockReport,
        reported: std::sync::atomic::AtomicBool,
    }
    fn read(clock: &SafeClock) -> u64 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (clock.now)())) {
            Ok(value) => value,
            Err(error) => {
                // Set first: a report handler that reads the clock again must not report again.
                if !clock
                    .reported
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let message = format!(
                        "now callback panicked: {}",
                        crate::session::panic_message(&*error)
                    );
                    let reenter = || read(clock);
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (clock.report)(&message, &reenter)
                    }));
                }
                crate::session::wall_clock()
            }
        }
    }
    let handle = Arc::new(SafeClock {
        now,
        report,
        reported: std::sync::atomic::AtomicBool::new(false),
    });
    Arc::new(move || read(&handle))
}

pub struct HarnessOptions {
    /// Receives the owning extension name and one report per failed hook invocation, and for
    /// tool failures the Harness reports on an extension's behalf: invalid structured output
    /// and failed cancels. The session reporter also receives its existing text notification.
    pub report_hook: Option<HookReporter>,
    pub env: Option<BuildEnv>,
    pub on_wake: Option<Arc<dyn Fn(super::Wake) + Send + Sync>>,
    pub session: SessionOptions,
    pub telemetry: pi_rs_telemetry::Context,
    /// The host's model registry: a plain `Models` is wrapped with the Harness identity once,
    /// a selector is read at each use.
    pub models: ModelsInput,
    pub identity: pi_rs_ai::request::ClientIdentity,
    pub registry: Arc<super::HarnessRegistry>,
    pub settings: Arc<dyn Fn() -> serde_json::Value + Send + Sync>,
}
impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            report_hook: None,
            env: None,
            on_wake: None,
            session: Default::default(),
            telemetry: pi_rs_telemetry::Context::noop(),
            models: Default::default(),
            identity: Default::default(),
            registry: Arc::default(),
            settings: Arc::new(|| serde_json::json!({})),
        }
    }
}
#[derive(Clone)]
pub struct Harness {
    pub(super) report_hook: Option<HookReporter>,
    pub(super) telemetry: Arc<super::telemetry::HarnessTelemetry>,
    pub(super) env: Option<BuildEnv>,
    pub(super) on_wake: Option<Arc<dyn Fn(super::Wake) + Send + Sync>>,
    pub(super) driver: Arc<super::scheduler::Driver>,
    pub(super) read_only: bool,
    /// The host's model registry, plain (wrapped once) or a selector read at each use.
    pub models: ModelsInput,
    /// The merged request identity applied to each models read.
    pub(super) identity: pi_rs_ai::request::ClientIdentity,
    pub registry: Arc<super::HarnessRegistry>,
    pub settings: Arc<dyn Fn() -> serde_json::Value + Send + Sync>,
    pub(super) now: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub session: Session,
}
#[derive(Clone)]
pub struct Conversation {
    pub(super) harness: Harness,
    pub id: ConversationId,
    pub(super) session: Session,
}

impl Harness {
    pub async fn open(
        storage: Arc<dyn Storage>,
        options: SessionOptions,
        context: &Context,
    ) -> Result<Self> {
        Self::open_with(
            storage,
            HarnessOptions {
                session: options,
                ..Default::default()
            },
            context,
        )
        .await
    }
    pub async fn open_with(
        storage: Arc<dyn Storage>,
        host_options: HarnessOptions,
        context: &Context,
    ) -> Result<Self> {
        let mut options = host_options.session;
        context.check()?;
        let host = options.conversation_created.take();
        options.conversation_created = Some(Arc::new(move |tx, record| {
            let host = host.clone();
            Box::pin(async move {
                for token in [&*LIVE_DOC, &*INBOX_DOC, &*USAGE_DOC, &*PROVIDER_DOC] {
                    let address = resolve_address(token, Some(record.id), None)?;
                    tx.doc(token, &address, None).await?;
                }
                if record.parent.is_none() {
                    let address = resolve_address(&AGENT_DOC, Some(record.id), None)?;
                    tx.doc(&AGENT_DOC, &address, None).await?;
                    if let Some(owner) = record.owner {
                        let parent =
                            resolve_address(&AGENT_DOC, Some(owner.conversation_id), None)?;
                        let value = tx.doc(&AGENT_DOC, &parent, None).await?.value()?.to_json();
                        tx.doc(&AGENT_DOC, &address, None)
                            .await?
                            .apply(Op::Replace(value))?;
                    }
                }
                if let Some(host) = host {
                    host(tx, record).await?;
                }
                Ok(())
            })
        }));
        let read_only = options.read_only;
        let telemetry = Arc::new(super::telemetry::HarnessTelemetry::new(
            host_options.telemetry,
        ));
        let identity = pi_rs_ai::registry::merge_identity(
            &DEFAULT_HARNESS_IDENTITY,
            host_options.identity.clone(),
        );
        // A plain registry is wrapped once; a selector is wrapped at each use.
        let models = match host_options.models {
            ModelsInput::Static(models) => {
                ModelsInput::Static(models.with_identity(identity.clone()))
            }
            ModelsInput::Dynamic(select) => ModelsInput::Dynamic(select),
        };
        // The host clock, wrapped once: every harness read and every Session read go through it.
        let clock_report = {
            let reporter = options.report.clone();
            let telemetry = telemetry.clone();
            Arc::new(move |message: &str, _: &dyn Fn() -> u64| {
                if let Some(reporter) = &reporter {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        reporter(crate::session::ReportKind::Host, message)
                    }));
                }
                telemetry.report("host_error", None, None);
            }) as ClockReport
        };
        let now = safe_now(options.now.clone(), clock_report);
        options.now = now.clone();
        let harness = Self {
            report_hook: host_options.report_hook,
            env: host_options.env,
            on_wake: host_options.on_wake,
            driver: Arc::default(),
            read_only,
            models,
            identity,
            registry: host_options.registry,
            settings: Arc::new(move || super::resolve_settings(&(host_options.settings)())),
            now,
            session: Session::new_tracked(storage, options, Some(telemetry.clone())),
            telemetry,
        };
        if harness.telemetry.enabled() {
            harness.load_telemetry_tasks(context).await?;
        }
        if !read_only {
            harness.recover_running(context).await?;
            if let Some(callback) = &harness.on_wake {
                let value = harness.wake(context).await?;
                harness.publish_wake(callback, value);
            }
        }
        Ok(harness)
    }
    pub async fn build_env(
        &self,
        id: ConversationId,
        ctx: &Context,
    ) -> Result<Option<Arc<dyn crate::env::ExecutionEnv>>> {
        let Some(build) = &self.env else {
            return Ok(None);
        };
        let address = resolve_address(&AGENT_DOC, Some(id), None)?;
        let state = self.session.snapshot(&AGENT_DOC, &address, ctx).await?;
        let cwd = state
            .as_ref()
            .and_then(|s| s.get("cwd"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        build(
            EnvTarget {
                conversation_id: id,
                cwd,
                read: self.session.clone(),
            },
            ctx,
        )
        .await
    }
    pub(super) fn handle(&self, id: ConversationId) -> Conversation {
        Conversation {
            id,
            harness: self.clone(),
            session: self.session.clone(),
        }
    }
    pub async fn root(&self, ctx: &Context) -> Result<Conversation> {
        self.session
            .commit(
                async |tx| {
                    if tx.conversation(ROOT_CONVERSATION_ID).await?.is_none() {
                        tx.create_root_conversation().await?;
                    }
                    Ok(())
                },
                ctx,
            )
            .await?;
        Ok(self.handle(ROOT_CONVERSATION_ID))
    }
    pub async fn conversation(
        &self,
        id: ConversationId,
        ctx: &Context,
    ) -> Result<Option<Conversation>> {
        Ok(self
            .session
            .conversation(id, ctx)
            .await?
            .map(|_| self.handle(id)))
    }
    pub async fn create_conversation(
        &self,
        ownership: ConversationOwnership,
        ctx: &Context,
    ) -> Result<Conversation> {
        let record = self
            .session
            .commit(async |tx| tx.create_conversation(ownership).await, ctx)
            .await?;
        Ok(self.handle(record.id))
    }
    pub async fn close(&self, ctx: &Context) -> Result<()> {
        let driver = self.stop_driver().await;
        let session = self.session.close(ctx).await;
        driver.and(session)
    }
}
impl Conversation {
    pub async fn fork(
        &self,
        at: EntryId,
        ownership: ConversationOwnership,
        ctx: &Context,
    ) -> Result<Self> {
        let record = self
            .session
            .commit(
                async |tx| tx.fork_conversation(self.id, at, ownership).await,
                ctx,
            )
            .await?;
        Ok(Self {
            id: record.id,
            harness: self.harness.clone(),
            session: self.session.clone(),
        })
    }
    pub async fn entries(
        &self,
        mut query: EntryQuery,
        limit: usize,
        cursor: Option<&crate::Cursor>,
        ctx: &Context,
    ) -> Result<Page<EntryRecord>> {
        query.conversation_id = self.id;
        self.session.scan_entries(&query, limit, cursor, ctx).await
    }
}
#[async_trait]
impl EntryLoader for Conversation {
    async fn load(
        &self,
        query: EntryQuery,
        limit: usize,
        ctx: &Context,
    ) -> Result<Page<EntryRecord>> {
        self.entries(query, limit, None, ctx).await
    }
}

impl std::ops::Deref for Harness {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}

impl Harness {
    /// The host's current `Models` with the Harness identity applied, read at each use: a plain
    /// registry is wrapped once, a selector at each call.
    pub fn models(&self) -> Models {
        match &self.models {
            ModelsInput::Static(models) => models.clone(),
            ModelsInput::Dynamic(select) => select().with_identity(self.identity.clone()),
        }
    }

    pub async fn get_task(
        &self,
        id: crate::TaskId,
        ctx: &Context,
    ) -> Result<Option<Arc<crate::TaskRecord>>> {
        self.session.task(id, ctx).await
    }
    pub async fn usage(&self, ctx: &Context) -> Result<super::UsageState> {
        let ids = self
            .session
            .commit(
                async |tx| {
                    let mut ids = Vec::new();
                    let mut cursor = None;
                    loop {
                        let page = tx
                            .scan_conversations(&Default::default(), 256, cursor.as_ref())
                            .await?;
                        ids.extend(page.items.into_iter().map(|r| r.id));
                        cursor = page.next;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Ok(ids)
                },
                ctx,
            )
            .await?;
        let mut total = super::UsageState::default();
        for id in ids {
            let address = resolve_address(&USAGE_DOC, Some(id), None)?;
            if let Some(state) = self.session.snapshot(&USAGE_DOC, &address, ctx).await? {
                total.add(&serde_json::from_value(serde_json::Value::Object(
                    (*state).clone(),
                ))?);
            }
        }
        Ok(total)
    }
}

impl Conversation {
    pub async fn configure(&self, change: &crate::JsonObject, ctx: &Context) -> Result<()> {
        self.session
            .commit(async |tx| super::configure(tx, self.id, change).await, ctx)
            .await
    }
    pub async fn commit<T>(
        &self,
        change: impl for<'a, 'tx> AsyncFnOnce(&'a mut crate::session::Transaction<'tx>) -> Result<T>,
        ctx: &Context,
        options: crate::session::CommitOptions,
    ) -> Result<T> {
        self.session
            .commit_with(
                change,
                ctx,
                crate::session::TransactionScope {
                    conversation_id: Some(self.id),
                    task_id: None,
                },
                options,
            )
            .await
    }
}

pub static DEFAULT_HARNESS_IDENTITY: std::sync::LazyLock<pi_rs_ai::request::ClientIdentity> =
    std::sync::LazyLock::new(|| pi_rs_ai::request::ClientIdentity {
        attribution: Some(Some(pi_rs_ai::request::Attribution {
            name: "pi".into(),
            url: "https://pi.dev".into(),
            categories: Some(vec!["cli-agent".into()]),
        })),
        ..Default::default()
    });
impl Conversation {
    pub async fn agent(&self, ctx: &Context) -> Result<super::Agent> {
        let address = resolve_address(&AGENT_DOC, Some(self.id), None)?;
        let state = self.session.snapshot(&AGENT_DOC, &address, ctx).await?;
        let defaults: Option<Vec<String>> =
            serde_json::from_value((self.harness.settings)()["extensions"].clone()).ok();
        Ok(super::resolve_agent(
            serde_json::Value::Object(state.as_deref().cloned().unwrap_or_default()),
            &self.harness.registry.snapshot(),
            defaults.as_deref(),
            |_| {},
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbortTaskResult {
    Marked,
    Terminal,
}

impl Harness {
    pub async fn signal(
        &self,
        id: crate::TaskId,
        token: &crate::signals::SignalToken,
        key: &str,
        value: serde_json::Value,
        ctx: &Context,
    ) -> Result<crate::signals::SignalResult> {
        self.resume()?;
        self.session
            .commit(async |tx| tx.signal(id, token, key, value).await, ctx)
            .await
    }

    pub async fn abort_task(&self, id: crate::TaskId, ctx: &Context) -> Result<AbortTaskResult> {
        self.resume()?;
        self.session
            .commit(
                async |tx| {
                    let record = tx.task(id).await?.ok_or_else(|| {
                        crate::session::Error::invalid(format!("Task {id} does not exist"))
                    })?;
                    if matches!(record.state, crate::TaskState::Terminal { .. }) {
                        return Ok(AbortTaskResult::Terminal);
                    }
                    if !record.abort_requested || record.abort_reason.is_some() {
                        let mut record = record.as_ref().clone();
                        record.abort_requested = true;
                        record.abort_reason = None;
                        tx.set_task(record)?;
                    }
                    Ok(AbortTaskResult::Marked)
                },
                ctx,
            )
            .await
    }

    pub async fn wait_for_task(
        &self,
        id: crate::TaskId,
        ctx: &Context,
    ) -> Result<Arc<crate::TaskRecord>> {
        self.resume()?;
        let signal = Arc::new(tokio::sync::Notify::new());
        let notify = signal.clone();
        let _commits = self.session.follow_commits(move |change, _| {
            if let crate::session::ObservedCommit::Stored(commit) = change
                && commit
                    .changes
                    .iter()
                    .any(|c| matches!(c,crate::CommitChange::Task{value} if value.id==id))
            {
                notify.notify_one();
            }
            Ok(())
        })?;
        let notify = signal.clone();
        let _close = self.session.subscribe_close(move || notify.notify_one())?;
        loop {
            let record = self.get_task(id, ctx).await?.ok_or_else(|| {
                crate::session::Error::invalid(format!("Task {id} does not exist"))
            })?;
            if matches!(record.state, crate::TaskState::Terminal { .. }) {
                return Ok(record);
            }
            ctx.await_with(signal.notified()).await?;
        }
    }
}

impl Conversation {
    pub async fn reset(&self, handoff: Option<String>, ctx: &Context) -> Result<()> {
        self.submit(super::SubmissionDraft{
            request_id:None,notify:None,
            body:super::SubmissionBody::Write{entry:crate::EntryDraft{
                kind:"pi.reset".into(),head:Some(crate::EntryHead::SelfEntry),
                model:handoff.map(|content|vec![serde_json::json!({"role":"user","content":content,"timestamp":(self.harness.now)()})]),
                ..Default::default()
            }}
        },ctx).await?;
        Ok(())
    }
}
