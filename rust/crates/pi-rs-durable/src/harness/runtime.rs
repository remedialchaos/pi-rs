use std::sync::Arc;

use async_trait::async_trait;
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
pub struct HarnessOptions {
    pub env: Option<BuildEnv>,
    pub on_wake: Option<Arc<dyn Fn(super::Wake) + Send + Sync>>,
    pub session: SessionOptions,
    pub models: pi_rs_ai::registry::Models,
    pub identity: pi_rs_ai::request::ClientIdentity,
    pub registry: Arc<super::HarnessRegistry>,
    pub settings: Arc<dyn Fn() -> serde_json::Value + Send + Sync>,
}
impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            env: None,
            on_wake: None,
            session: Default::default(),
            models: Default::default(),
            identity: Default::default(),
            registry: Arc::default(),
            settings: Arc::new(|| serde_json::json!({})),
        }
    }
}
#[derive(Clone)]
pub struct Harness {
    pub(super) env: Option<BuildEnv>,
    pub(super) on_wake: Option<Arc<dyn Fn(super::Wake) + Send + Sync>>,
    pub(super) driver: Arc<super::scheduler::Driver>,
    pub(super) read_only: bool,
    pub models: pi_rs_ai::registry::Models,
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
        let harness = Self {
            env: host_options.env,
            on_wake: host_options.on_wake,
            driver: Arc::default(),
            read_only,
            models: host_options
                .models
                .with_identity(pi_rs_ai::registry::merge_identity(
                    &DEFAULT_HARNESS_IDENTITY,
                    host_options.identity,
                )),
            registry: host_options.registry,
            settings: Arc::new(move || super::resolve_settings(&(host_options.settings)())),
            now: options.now.clone(),
            session: Session::new(storage, options),
        };
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
        self.stop_driver().await?;
        self.session.close(ctx).await
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
