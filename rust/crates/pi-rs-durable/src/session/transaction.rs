use std::{collections::HashSet, sync::Arc};

use indexmap::IndexMap;
use pi_rs_chord::{context::Context, delta::Change};
use pi_rs_telemetry::TelemetryCarrier;
use serde_json::Value;

use crate::{
    documents::{DocToken, address_id, check_record_scope},
    entries::Entry,
    errors::SessionError,
    ids::*,
    signals::{SignalResult, SignalToken, deliver_signal, is_settled, submission_settled},
    storage_types::*,
    tasks::Task,
};

use super::{
    Error, HeadMarker, Inner, Result,
    document_cache::{DocumentDraft, DocumentPlan, Origin, PreparedBatch},
    forks::prepare_fork_document_copies,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct TransactionScope {
    pub conversation_id: Option<ConversationId>,
    pub task_id: Option<TaskId>,
}

#[derive(Clone, Debug, Default)]
pub enum TaskTrace {
    #[default]
    Inherit,
    Root,
    Carrier(TelemetryCarrier),
}

#[derive(Clone, Debug)]
pub struct TaskOptions {
    pub ownership: TaskOwnership,
    pub conversation_id: Option<ConversationId>,
    pub background: bool,
    pub abandon_on_restart: bool,
    pub trace: TaskTrace,
    pub links: Option<Vec<TelemetryCarrier>>,
    pub submission_ids: Option<Vec<SubmissionId>>,
}

impl TaskOptions {
    pub fn new(ownership: TaskOwnership) -> Self {
        Self {
            ownership,
            conversation_id: None,
            background: false,
            abandon_on_restart: false,
            trace: TaskTrace::Inherit,
            links: None,
            submission_ids: None,
        }
    }
}

#[derive(Default)]
struct TaskDraft {
    committed: Option<Option<Arc<TaskRecord>>>,
    staged: Option<TaskRecord>,
    created: bool,
}

enum SubmissionChange {
    Place(EntryId),
    Settle(SubmissionSettlement),
}

/// The mutation line lends this transaction to a callback. Drafts and candidates cannot
/// outlive it, and only a receipt allows its prepared state to enter the Session's caches.
pub struct Transaction<'a> {
    session: &'a Arc<Inner>,
    context: &'a Context,
    scope: TransactionScope,
    has_table_write: bool,
    writes: Vec<StorageWrite>,
    conversations: HashSet<ConversationId>,
    fork_parents: HashSet<ConversationId>,
    fork_sources: HashSet<DocumentId>,
    tasks: IndexMap<TaskId, TaskDraft>,
    submissions: IndexMap<SubmissionId, SubmissionRecord>,
    submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    documents: Vec<DocumentDraft>,
    addresses: IndexMap<String, usize>,
}

impl<'a> Transaction<'a> {
    pub(super) fn new(
        session: &'a Arc<Inner>,
        context: &'a Context,
        scope: TransactionScope,
    ) -> Self {
        Self {
            session,
            context,
            scope,
            has_table_write: false,
            writes: Vec::new(),
            conversations: HashSet::new(),
            fork_parents: HashSet::new(),
            fork_sources: HashSet::new(),
            tasks: IndexMap::new(),
            submissions: IndexMap::new(),
            submission_changes: Vec::new(),
            documents: Vec::new(),
            addresses: IndexMap::new(),
        }
    }

    pub fn context(&self) -> &Context {
        self.context
    }
    pub fn has_table_write(&self) -> bool {
        self.has_table_write
    }

    fn read(&self, operation: &'static str) -> Result<()> {
        if self.has_table_write {
            return Err(SessionError::ReadAfterWrite(operation).into());
        }
        self.context.check()?;
        self.session.healthy()
    }

    fn write(&mut self) -> Result<()> {
        self.context.check()?;
        self.session.healthy()?;
        self.has_table_write = true;
        Ok(())
    }

    pub async fn conversation(
        &self,
        id: ConversationId,
    ) -> Result<Option<Arc<ConversationRecord>>> {
        self.read("Tx.conversation()")?;
        self.session.conversation(id).await
    }

    pub async fn entry(&self, id: EntryId) -> Result<Option<EntryRecord>> {
        self.read("Tx.entry()")?;
        Ok(self.session.entry(id).await?.map(|stored| stored.entry))
    }

    pub async fn typed_entry<D>(
        &self,
        token: &Entry<D>,
        id: EntryId,
    ) -> Result<Option<EntryRecord>> {
        Ok(self
            .entry(id)
            .await?
            .filter(|entry| token.matches(Some(entry))))
    }

    pub async fn task(&mut self, id: TaskId) -> Result<Option<Arc<TaskRecord>>> {
        self.read("Tx.task()")?;
        self.committed_task(id).await
    }

    pub async fn submission(&self, id: SubmissionId) -> Result<Option<Arc<SubmissionRecord>>> {
        self.read("Tx.submission()")?;
        self.session.submission(id).await
    }

    pub async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.read("Tx.submissionByRequest()")?;
        self.session
            .submission_by_request(conversation, request)
            .await
    }

    pub async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.read("Tx.scanConversations()")?;
        self.session.scan_conversations(query, limit, cursor).await
    }

    pub async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<EntryRecord>> {
        self.read("Tx.scanEntries()")?;
        self.session.scan_entries(query, limit, cursor).await
    }

    pub async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<TaskRecord>> {
        self.read("Tx.scanTasks()")?;
        self.session.scan_tasks(query, limit, cursor).await
    }

    pub async fn latest_entry_id(&self, conversation: ConversationId) -> Result<Option<EntryId>> {
        self.read("Tx.latestEntryId()")?;
        self.session.latest_entry_id(conversation).await
    }

    pub async fn latest_head_marker(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<HeadMarker>> {
        self.read("Tx.latestHeadMarker()")?;
        self.session.latest_head_marker(conversation, None).await
    }

    pub async fn create_conversation(
        &mut self,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord> {
        self.stage_conversation(None, ownership, None).await
    }

    pub async fn create_root_conversation(&mut self) -> Result<ConversationRecord> {
        self.stage_conversation(
            None,
            ConversationOwnership::Ownerless,
            Some(ROOT_CONVERSATION_ID),
        )
        .await
    }

    pub async fn fork_conversation(
        &mut self,
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord> {
        self.stage_conversation(
            Some(ConversationParent {
                conversation_id: parent,
                at,
            }),
            ownership,
            None,
        )
        .await
    }

    async fn stage_conversation(
        &mut self,
        parent: Option<ConversationParent>,
        ownership: ConversationOwnership,
        reserved: Option<ConversationId>,
    ) -> Result<ConversationRecord> {
        self.write()?;
        let id = match reserved {
            Some(id) => id,
            None => self.session.mint_id()?,
        };
        let owner = match ownership {
            ConversationOwnership::Ownerless => None,
            ConversationOwnership::Task { task_id } => {
                let task = self.current_task(task_id).await?.ok_or_else(|| {
                    Error::invalid(format!("Conversation owner task {task_id} does not exist"))
                })?;
                Some(ConversationOwner {
                    conversation_id: task.conversation_id,
                    task_id,
                })
            }
        };
        if let Some(parent) = parent {
            let copies =
                prepare_fork_document_copies(self.session, parent.conversation_id, parent.at, id)
                    .await?;
            self.fork_sources.extend(copies.source_ids);
            self.fork_parents.insert(parent.conversation_id);
            for (record, source) in copies.copies {
                let address = record.address();
                self.stage_document(
                    address_id(&address),
                    DocumentDraft::new(address, Origin::Copy(record, source)),
                );
            }
        }
        let record = ConversationRecord { id, parent, owner };
        self.conversations.insert(id);
        self.writes.push(StorageWrite::Conversation {
            value: record.clone(),
        });
        if let Some(hook) = self.session.options.conversation_created.clone() {
            hook(self, &record).await?;
        }
        Ok(record)
    }

    pub async fn append_entry(
        &mut self,
        conversation_id: ConversationId,
        draft: EntryDraft,
    ) -> Result<EntryRecord> {
        self.write()?;
        self.require_conversation(conversation_id).await?;
        let id = self.session.mint_id()?;
        let record = EntryRecord {
            id,
            conversation_id,
            kind: draft.kind,
            model: draft.model,
            data: draft.data,
            head: draft.head.map(|head| match head {
                EntryHead::Id(id) => id,
                EntryHead::SelfEntry => id,
            }),
            edits: draft.edits,
            by_task_id: self.scope.task_id,
        };
        self.writes.push(StorageWrite::Entry {
            value: record.clone(),
        });
        Ok(record)
    }

    pub async fn append_typed_entry<D>(
        &mut self,
        token: &Entry<D>,
        conversation_id: ConversationId,
        mut draft: EntryDraft,
    ) -> Result<EntryRecord> {
        draft.kind = token.kind().to_owned();
        self.append_entry(conversation_id, draft).await
    }

    pub async fn create_task<R: ?Sized, H>(
        &mut self,
        task: &Task<R, H>,
        input: Value,
        mut options: TaskOptions,
    ) -> Result<TaskId> {
        self.write()?;
        if let Some(ids) = options.submission_ids.take() {
            options.trace = TaskTrace::Inherit;
            options.links = None;
            for (index, id) in ids.into_iter().enumerate() {
                let carrier = match self.submissions.get(&id) {
                    Some(record) => record.trace.clone(),
                    None => self
                        .session
                        .submission(id)
                        .await?
                        .and_then(|record| record.trace.clone()),
                };
                if index == 0 {
                    options.trace = carrier.map_or(TaskTrace::Root, TaskTrace::Carrier);
                } else if let Some(carrier) = carrier {
                    options.links.get_or_insert_with(Vec::new).push(carrier);
                }
            }
        }
        let mut conversation_id = options.conversation_id.or(self.scope.conversation_id);
        let owner = match options.ownership {
            TaskOwnership::Conversation => None,
            TaskOwnership::Task { task_id } => {
                let owner = self.current_task(task_id).await?.ok_or_else(|| {
                    Error::invalid(format!("Task owner {task_id} does not exist"))
                })?;
                if options.background {
                    return Err(Error::invalid("A child task cannot be background"));
                }
                if options
                    .conversation_id
                    .is_some_and(|id| id != owner.conversation_id)
                {
                    return Err(Error::invalid(format!(
                        "A child task lives in its owner's conversation {}",
                        owner.conversation_id
                    )));
                }
                conversation_id = Some(owner.conversation_id);
                Some(task_id)
            }
        };
        let conversation_id = conversation_id
            .ok_or_else(|| Error::invalid("Tx.createTask() requires options.conversationId"))?;
        self.require_conversation(conversation_id).await?;
        let checkpoint = task.initial(&input).map_err(Error::Callback)?;
        let id = self.session.mint_id()?;
        let trace = match options.trace {
            TaskTrace::Root => None,
            TaskTrace::Carrier(carrier) => Some(carrier),
            TaskTrace::Inherit => {
                let telemetry = self.context.telemetry();
                telemetry.inject(telemetry)
            }
        };
        let record = TaskRecord {
            trace,
            links: options.links,
            id,
            conversation_id,
            kind: task.name().to_owned(),
            version: task.version(),
            input,
            owner,
            background: options.background,
            abort_requested: false,
            abort_reason: None,
            abandon_on_restart: options.abandon_on_restart.then_some(true),
            started_at: None,
            ended_at: None,
            restarts: None,
            state: TaskState::Pending {
                checkpoint,
                wake: None,
            },
            memos: None,
        };
        self.tasks.insert(
            id,
            TaskDraft {
                committed: None,
                staged: Some(record),
                created: true,
            },
        );
        Ok(id)
    }

    pub async fn create_submission(
        &mut self,
        create: SubmissionCreate,
    ) -> Result<SubmissionRecord> {
        self.write()?;
        self.require_conversation(create.conversation_id).await?;
        let record = create.with_id(self.session.mint_id()?);
        self.submissions.insert(record.id, record.clone());
        Ok(record)
    }

    pub fn settle_submission(
        &mut self,
        id: SubmissionId,
        settlement: SubmissionSettlement,
    ) -> Result<()> {
        self.write()?;
        self.submission_changes
            .push((id, SubmissionChange::Settle(settlement)));
        Ok(())
    }

    pub fn place_submission(&mut self, id: SubmissionId, entry: EntryId) -> Result<()> {
        self.write()?;
        self.submission_changes
            .push((id, SubmissionChange::Place(entry)));
        Ok(())
    }

    pub async fn signal(
        &mut self,
        id: TaskId,
        token: &SignalToken,
        key: &str,
        value: Value,
    ) -> Result<SignalResult> {
        self.read("Tx.signal()")?;
        let now = self.session.now();
        let current = self.current_task(id).await?;
        let delivery = deliver_signal(current, token, key, value, now);
        if let Some(record) = delivery.record {
            self.set_task(record)?;
        }
        Ok(delivery.result)
    }

    pub fn set_task(&mut self, record: TaskRecord) -> Result<()> {
        self.write()?;
        self.stage_task(record)
    }

    fn stage_task(&mut self, mut record: TaskRecord) -> Result<()> {
        let state = self.tasks.entry(record.id).or_default();
        if let Some(current) = &state.staged {
            if matches!(current.state, TaskState::Terminal { .. }) {
                return Err(Error::invalid(format!(
                    "Task {} already has a terminal candidate",
                    record.id
                )));
            }
            if current.conversation_id != record.conversation_id {
                return Err(Error::invalid(format!(
                    "Task {} cannot change conversations",
                    record.id
                )));
            }
            if !matches!(
                record.state,
                TaskState::Terminal { .. } | TaskState::Completing { .. }
            ) && let Some(memos) = &current.memos
            {
                let mut merged = memos.clone();
                merged.extend(record.memos.take().into_iter().flatten());
                record.memos = Some(merged);
            }
            record.started_at = current.started_at.or(record.started_at);
            record.ended_at = current.ended_at.or(record.ended_at);
        }
        if matches!(record.state, TaskState::Running { .. }) {
            record.started_at.get_or_insert_with(|| self.session.now());
        }
        if matches!(record.state, TaskState::Terminal { .. }) {
            record.ended_at.get_or_insert_with(|| self.session.now());
        }
        if matches!(
            record.state,
            TaskState::Terminal { .. } | TaskState::Completing { .. }
        ) {
            record.memos = None;
        }
        state.staged = Some(record);
        Ok(())
    }

    pub fn staged_tasks(&self) -> impl Iterator<Item = &TaskRecord> {
        self.tasks.values().filter_map(|task| task.staged.as_ref())
    }
    pub fn created_task_ids(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.tasks
            .iter()
            .filter_map(|(id, draft)| draft.created.then_some(*id))
    }
    pub fn staged_conversations(&self) -> impl Iterator<Item = &ConversationRecord> {
        self.writes.iter().filter_map(|write| {
            if let StorageWrite::Conversation { value } = write {
                Some(value)
            } else {
                None
            }
        })
    }

    async fn committed_task(&mut self, id: TaskId) -> Result<Option<Arc<TaskRecord>>> {
        if let Some(Some(value)) = self.tasks.get(&id).map(|task| &task.committed) {
            return Ok(value.clone());
        }
        let value = self.session.task(id).await?;
        self.tasks.entry(id).or_default().committed = Some(value.clone());
        Ok(value)
    }

    async fn current_task(&mut self, id: TaskId) -> Result<Option<&TaskRecord>> {
        if self
            .tasks
            .get(&id)
            .is_none_or(|task| task.staged.is_none() && task.committed.is_none())
        {
            self.committed_task(id).await?;
        }
        Ok(self.tasks.get(&id).and_then(|task| {
            task.staged
                .as_ref()
                .or_else(|| task.committed.as_ref().and_then(Option::as_deref))
        }))
    }

    async fn require_conversation(&self, id: ConversationId) -> Result<()> {
        if !self.conversations.contains(&id) && self.session.conversation(id).await?.is_none() {
            return Err(Error::invalid(format!("Conversation {id} does not exist")));
        }
        Ok(())
    }

    pub async fn doc(
        &mut self,
        definition: &DocToken,
        address: &DocumentAddress,
        seed: Option<&Value>,
    ) -> Result<&mut Change> {
        self.context.check()?;
        self.session.healthy()?;
        super::document_cache::validate_address(definition, address)?;
        if let DocumentScope::Task { task_id } = address.scope
            && self
                .tasks
                .get(&task_id)
                .and_then(|task| task.staged.as_ref())
                .is_some_and(|task| matches!(task.state, TaskState::Terminal { .. }))
        {
            return Err(Error::invalid(format!("Task {task_id} is terminal")));
        }
        let key = address_id(address);
        let latest = self.addresses.get(&key).copied();
        let previous = latest.and_then(|index| self.documents.get(index));
        if let Some(previous) = previous
            && !previous.retire
        {
            let index =
                latest.ok_or_else(|| Error::invalid("Document draft missing its address"))?;
            if let Origin::Copy(record, source) = &previous.origin {
                let record = record.clone();
                let source = *source;
                let stored = self
                    .session
                    .document(source.id, source.at)
                    .await?
                    .ok_or_else(|| {
                        Error::invalid(format!("Fork source document {} cannot be read", source.id))
                    })?;
                if !matches!(stored.record.scope, DocumentScope::Conversation { .. })
                    || stored.record.kind != record.kind
                    || stored.record.key != record.key
                    || stored.record.history != record.history
                    || stored.record.fork != record.fork
                {
                    return Err(Error::invalid(format!(
                        "Fork source document {} does not match the copied record",
                        source.id
                    )));
                }
                let value = crate::documents::materialize_document_value(
                    definition,
                    &record,
                    stored.version,
                    stored.value,
                )?;
                self.documents[index] =
                    DocumentDraft::create(address.clone(), record, definition.clone(), value)?;
            }
            return self.documents[index].change();
        }
        let loaded = if previous.is_some_and(|draft| draft.retire) {
            None
        } else {
            self.session.load_document(definition, address).await?
        };
        let draft = if let Some(loaded) = loaded {
            DocumentDraft::loaded(address.clone(), definition.clone(), loaded)?
        } else {
            match address.scope {
                DocumentScope::Session => {}
                DocumentScope::Conversation { conversation_id } => {
                    self.require_conversation(conversation_id).await?
                }
                DocumentScope::Task { task_id } => {
                    let task = self
                        .current_task(task_id)
                        .await?
                        .ok_or_else(|| Error::invalid(format!("Task {task_id} does not exist")))?;
                    if matches!(task.state, TaskState::Terminal { .. }) {
                        return Err(Error::invalid(format!("Task {task_id} is terminal")));
                    }
                }
            }
            let value = definition.initial(seed)?;
            let record =
                crate::documents::document_create(definition, address, self.session.mint_id()?)?;
            DocumentDraft::create(address.clone(), record, definition.clone(), value)?
        };
        let index = self.stage_document(key, draft);
        self.documents[index].change()
    }

    pub async fn retire_doc(
        &mut self,
        definition: &DocToken,
        address: &DocumentAddress,
    ) -> Result<()> {
        self.context.check()?;
        super::document_cache::validate_address(definition, address)?;
        let key = address_id(address);
        if let Some(&index) = self.addresses.get(&key) {
            self.documents[index].check_scope(definition)?;
            self.documents[index].retire = true;
        } else if let Some(record) = self
            .session
            .find_document(address, DocumentPoint::Current)
            .await?
        {
            check_record_scope(definition, &record)?;
            let mut draft = DocumentDraft::new(address.clone(), Origin::Existing(record));
            draft.retire = true;
            self.stage_document(key, draft);
        }
        Ok(())
    }

    fn stage_document(&mut self, address: String, draft: DocumentDraft) -> usize {
        let index = self.documents.len();
        self.documents.push(draft);
        self.addresses.insert(address, index);
        index
    }

    pub(super) async fn prepare(mut self) -> Result<PreparedBatch> {
        let mut plans = Vec::with_capacity(self.documents.len());
        for draft in std::mem::take(&mut self.documents) {
            let plan = draft.prepare()?;
            if plan.writes() {
                if self.fork_sources.contains(&plan.id()) {
                    return Err(Error::invalid(format!(
                        "Cannot change fork source document {} in the fork transaction",
                        plan.id()
                    )));
                }
                if let DocumentScope::Conversation { conversation_id } = plan.address.scope
                    && self.fork_parents.contains(&conversation_id)
                    && plan.fork() == Some(DocumentFork::Current)
                {
                    return Err(Error::invalid(format!(
                        "Cannot fork conversation {conversation_id} while changing its current-policy documents"
                    )));
                }
            }
            plans.push(plan);
        }
        let owners: Vec<_> = self
            .staged_conversations()
            .filter_map(|conversation| {
                conversation
                    .owner
                    .map(|owner| ("Conversation owner task", owner.task_id))
            })
            .chain(
                self.tasks
                    .values()
                    .filter(|draft| draft.created)
                    .filter_map(|draft| draft.staged.as_ref()?.owner.map(|id| ("Task owner", id))),
            )
            .collect();
        for (what, id) in owners {
            let task = self
                .current_task(id)
                .await?
                .ok_or_else(|| Error::invalid(format!("{what} {id} does not exist")))?;
            let status = task.state.status();
            if matches!(status, TaskStatus::Terminal | TaskStatus::Completing) {
                return Err(Error::invalid(format!(
                    "{what} {id} is {}",
                    status.as_str()
                )));
            }
            if task.abort_requested {
                return Err(Error::invalid(format!("{what} {id} is abort-marked")));
            }
        }
        let replacements: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(&id, draft)| {
                (!draft.created)
                    .then_some(draft.staged.as_ref())
                    .flatten()
                    .map(|record| (id, record.conversation_id))
            })
            .collect();
        for (id, conversation_id) in replacements {
            let committed = self
                .committed_task(id)
                .await?
                .ok_or_else(|| Error::invalid(format!("Task {id} does not exist")))?;
            if matches!(committed.state, TaskState::Terminal { .. }) {
                return Err(Error::invalid(format!("Task {id} is already terminal")));
            }
            if committed.conversation_id != conversation_id {
                return Err(Error::invalid(format!(
                    "Task {id} cannot change conversations"
                )));
            }
            if let Some(staged) = self
                .tasks
                .get_mut(&id)
                .and_then(|task| task.staged.as_mut())
            {
                staged.started_at = committed.started_at.or(staged.started_at);
                staged.ended_at = committed.ended_at.or(staged.ended_at);
            }
        }
        self.retire_terminal_documents(&mut plans).await?;
        for plan in &mut plans {
            plan.conversation_id = match plan.address.scope {
                DocumentScope::Session => None,
                DocumentScope::Conversation { conversation_id } => Some(conversation_id),
                DocumentScope::Task { task_id } if plan.writes() => self
                    .current_task(task_id)
                    .await?
                    .map(|task| task.conversation_id),
                DocumentScope::Task { .. } => None,
            };
        }
        self.settle_submissions().await?;
        for plan in &mut plans {
            plan.checkpoint();
        }
        let created_tasks = self.created_task_ids().collect();
        self.writes.extend(
            self.submissions
                .into_values()
                .map(|value| StorageWrite::Submission { value }),
        );
        self.writes.extend(
            self.tasks
                .into_values()
                .filter_map(|draft| draft.staged)
                .map(|value| StorageWrite::Task { value }),
        );
        Ok(PreparedBatch {
            writes: self.writes,
            documents: plans,
            created_tasks,
            silent: false,
        })
    }

    async fn retire_terminal_documents(&self, plans: &mut Vec<DocumentPlan>) -> Result<()> {
        for task in self
            .staged_tasks()
            .filter(|task| matches!(task.state, TaskState::Terminal { .. }))
        {
            let scope = DocumentScope::Task { task_id: task.id };
            let mut known = HashSet::new();
            for plan in plans.iter_mut().filter(|plan| plan.address.scope == scope) {
                plan.retire = true;
                known.insert(plan.id());
            }
            if self.tasks.get(&task.id).is_some_and(|draft| draft.created) {
                continue;
            }
            for record in self.session.task_documents(task.id).await? {
                if known.insert(record.id) {
                    plans.push(DocumentPlan::retire(record));
                }
            }
        }
        Ok(())
    }

    async fn settle_submissions(&mut self) -> Result<()> {
        let mut notify: Vec<_> = self
            .submissions
            .values()
            .filter(|record| is_settled(record) && record.notify.is_some())
            .map(|record| record.id)
            .collect();
        for (id, change) in std::mem::take(&mut self.submission_changes) {
            if !self.submissions.contains_key(&id) {
                let found = self
                    .session
                    .submission(id)
                    .await?
                    .ok_or_else(|| Error::invalid(format!("Submission {id} does not exist")))?;
                if is_settled(&found) {
                    continue;
                }
                self.submissions.insert(id, (*found).clone());
            }
            let Some(record) = self.submissions.get_mut(&id) else {
                continue;
            };
            if is_settled(record) {
                continue;
            }
            match change {
                SubmissionChange::Place(entry) => {
                    if record.status != SubmissionStatus::Queued {
                        return Err(Error::invalid(format!("Submission {id} is not queued")));
                    }
                    record.status = if record.kind == SubmissionKind::Input {
                        SubmissionStatus::Placed
                    } else {
                        SubmissionStatus::Done
                    };
                    record.entry = Some(entry);
                }
                SubmissionChange::Settle(SubmissionSettlement::Done { answer }) => {
                    if record.status != SubmissionStatus::Placed {
                        return Err(Error::invalid(format!(
                            "Submission {id} is not a placed input or continue"
                        )));
                    }
                    record.status = SubmissionStatus::Done;
                    record.answer = Some(answer);
                }
                SubmissionChange::Settle(SubmissionSettlement::Unanswered { reason, detail }) => {
                    record.status = SubmissionStatus::Unanswered;
                    record.reason = Some(reason);
                    record.detail = detail;
                }
            }
            if is_settled(record) && record.notify.is_some() {
                notify.push(id);
            }
        }
        for id in notify {
            if let Some(record) = self.submissions.get(&id)
                && let Some(notify) = &record.notify
            {
                let value = serde_json::to_value(record)?;
                let notify = notify.clone();
                let token =
                    submission_settled().map_err(|error| Error::invalid(error.to_string()))?;
                let now = self.session.now();
                let target = self.current_task(notify.task_id).await?;
                let delivered = deliver_signal(target, token, &notify.key, value, now);
                if let Some(record) = delivered.record {
                    self.stage_task(record)?;
                }
            }
        }
        Ok(())
    }
}
