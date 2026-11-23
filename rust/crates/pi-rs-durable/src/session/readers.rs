use std::{ops::AsyncFnOnce, sync::Arc};

use pi_rs_chord::{context::Context, watch::Watch};
use serde_json::Value;

use crate::{
    documents::{
        DocToken, address_id, check_record_scope, check_record_version, materialize_document_value,
    },
    ids::*,
    storage::{Storage, StorageResult},
    storage_types::*,
};

use super::{
    DocumentObserver, DocumentWatch, Error, HeadMarker, Inner, Key, Observation, ReportKind,
    Result, Session, Subscription,
    cached_storage::CachedStorage,
    document_cache::{LoadedDocument, validate_address},
    lock,
    observers::{DocumentDestination, DocumentFollower, Listener, detach_document, json},
};

impl Session {
    async fn read<T>(
        &self,
        context: &Context,
        read: impl AsyncFnOnce(&Inner) -> Result<T>,
    ) -> Result<T> {
        let session = &self.0;
        session.usable()?;
        let guard = context.await_with(session.line.lock()).await?;
        session.line_open()?;
        let earlier = session.writer.gate(&Key::All);
        let result = context.await_with(read(session)).await?;
        drop(guard);
        if let Some(earlier) = earlier {
            context.await_with(earlier.wait()).await?;
        }
        if result.is_ok() {
            session.healthy()?;
        }
        result
    }

    pub async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> Result<Option<Arc<ConversationRecord>>> {
        self.read(context, async |session| session.conversation(id).await)
            .await
    }

    pub async fn entry(&self, id: EntryId, context: &Context) -> Result<Option<StoredEntry>> {
        self.read(context, async |session| session.entry(id).await)
            .await
    }

    pub async fn visible_entry(
        &self,
        conversation: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> Result<Option<StoredEntry>> {
        self.read(context, async |session| {
            session.visible_entry(conversation, id).await
        })
        .await
    }

    pub async fn task(&self, id: TaskId, context: &Context) -> Result<Option<Arc<TaskRecord>>> {
        self.read(context, async |session| session.task(id).await)
            .await
    }

    pub async fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> Result<Option<Arc<SubmissionRecord>>> {
        self.read(context, async |session| session.submission(id).await)
            .await
    }

    pub async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &str,
        context: &Context,
    ) -> Result<Option<SubmissionRecord>> {
        self.read(context, async |session| {
            session.submission_by_request(conversation, request).await
        })
        .await
    }

    pub async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> Result<Page<ConversationRecord>> {
        self.read(context, async |session| {
            session.scan_conversations(query, limit, cursor).await
        })
        .await
    }

    pub async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> Result<Page<EntryRecord>> {
        self.read(context, async |session| {
            session.scan_entries(query, limit, cursor).await
        })
        .await
    }

    pub async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> Result<Page<TaskRecord>> {
        self.read(context, async |session| {
            session.scan_tasks(query, limit, cursor).await
        })
        .await
    }

    pub async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> Result<Page<SubmissionRecord>> {
        self.read(context, async |session| {
            session.touch(&Key::All).await?;
            session.read_storage(|storage| storage.scan_submissions(query, limit, cursor))
        })
        .await
    }

    pub async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> Result<Page<DocumentRecord>> {
        self.read(context, async |session| {
            session.scan_documents(query, limit, cursor).await
        })
        .await
    }

    pub async fn latest_entry_id(
        &self,
        conversation: ConversationId,
        context: &Context,
    ) -> Result<Option<EntryId>> {
        self.read(context, async |session| {
            session.latest_entry_id(conversation).await
        })
        .await
    }

    /// `None` is unknown; `Some(None)` is a known-empty transcript.
    pub fn known_latest_entry_id(&self, conversation: ConversationId) -> Option<Option<EntryId>> {
        if self.0.options.read_only {
            return None;
        }
        lock(&self.0.state).tails.peek(conversation)
    }

    pub async fn latest_head_marker(
        &self,
        conversation: ConversationId,
        cutoff: Option<EntryId>,
        context: &Context,
    ) -> Result<Option<HeadMarker>> {
        self.read(context, async |session| {
            session.latest_head_marker(conversation, cutoff).await
        })
        .await
    }

    pub async fn snapshot(
        &self,
        definition: &DocToken,
        address: &DocumentAddress,
        context: &Context,
    ) -> Result<Option<Arc<JsonObject>>> {
        self.read(context, async |session| {
            Ok(session
                .load_document(definition, address)
                .await?
                .map(|loaded| loaded.value.clone()))
        })
        .await
    }

    /// The historical address uses the actual conversation that wrote the visible cutoff entry.
    pub async fn snapshot_as_of(
        &self,
        definition: &DocToken,
        address: &DocumentAddress,
        at: EntryId,
        context: &Context,
    ) -> Result<Option<Arc<JsonObject>>> {
        validate_address(definition, address)?;
        let DocumentScope::Conversation { conversation_id } = address.scope else {
            return Err(Error::invalid(
                "Session.snapshotAsOf() requires a conversation document",
            ));
        };
        if definition.semantics.history_and_fork().0 != Some(DocumentHistory::Rewindable) {
            return Err(Error::invalid(
                "Session.snapshotAsOf() requires a rewindable conversation document",
            ));
        }
        self.read(context, async |session| {
            let stored = session
                .visible_entry(conversation_id, at)
                .await?
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "Entry {at} is not visible from conversation {conversation_id}"
                    ))
                })?;
            let address = DocumentAddress {
                scope: DocumentScope::Conversation {
                    conversation_id: stored.entry.conversation_id,
                },
                ..address.clone()
            };
            let point = DocumentPoint::Seq(stored.commit_seq);
            let Some(record) = session.find_document(&address, point).await? else {
                return Ok(None);
            };
            let data = session.document(record.id, point).await?.ok_or_else(|| {
                Error::invalid(format!(
                    "Historical document {} ({}) cannot be read",
                    record.id, record.kind
                ))
            })?;
            Ok(Some(Arc::new(crate::documents::materialize_document(
                definition, data,
            )?)))
        })
        .await
    }

    pub async fn observe_doc(
        &self,
        definition: &DocToken,
        address: &DocumentAddress,
        observer: DocumentObserver,
        context: &Context,
    ) -> Result<Option<Observation<Arc<JsonObject>>>> {
        self.read(context, async move |session| {
            let Some(document) = session.load_document(definition, address).await? else {
                return Ok(None);
            };
            let mut observers = lock(&session.observers);
            session.usable()?;
            let id = observers.allocate();
            let follower = DocumentFollower::new(
                address.clone(),
                &document,
                DocumentDestination::Sync(Listener::new(observer)),
            );
            observers.insert_document(id, Arc::new(follower));
            let weak = session.weak.clone();
            Ok(Some(Observation {
                value: document.value.clone(),
                subscription: Subscription::new(move || detach_document(&weak, id)),
            }))
        })
        .await
    }

    pub async fn watch_doc(
        &self,
        definition: &DocToken,
        address: &DocumentAddress,
        context: &Context,
    ) -> Result<Option<DocumentWatch>> {
        // Use the synchronous observation as the attachment authority. If cancellation wins
        // after attachment, dropping the observation undoes it immediately.
        let attached = self
            .read(context, async |session| {
                let Some(document) = session.load_document(definition, address).await? else {
                    return Ok(None);
                };
                let mut observers = lock(&session.observers);
                session.usable()?;
                let id = observers.allocate();
                let weak = session.weak.clone();
                let report = weak.clone();
                let watch = Watch::new(
                    json(&document.value),
                    move || detach_document(&weak, id),
                    move |error| {
                        if let Some(session) = report.upgrade() {
                            session.report(ReportKind::Listener, &error);
                        }
                    },
                );
                let follower = DocumentFollower::new(
                    address.clone(),
                    &document,
                    DocumentDestination::Watch(watch.clone()),
                );
                observers.insert_document(id, Arc::new(follower));
                drop(observers);
                let weak = session.weak.clone();
                let guard = Subscription::new(move || detach_document(&weak, id));
                Ok(Some((watch, guard)))
            })
            .await?;
        let Some((watch, guard)) = attached else {
            return Ok(None);
        };
        if let Some(signal) = context.abort_signal() {
            watch.observe_abort(signal)?;
        }
        context.check()?;
        guard.keep();
        Ok(Some(watch))
    }

    /// Case-insensitive text matching over model messages in a fork-aware history range.
    /// Tool arguments and results are not indexed. The caller controls the order and cutoff.
    pub async fn search_entries(
        &self,
        query: &EntryQuery,
        text: &str,
        limit: usize,
        context: &Context,
    ) -> Result<Vec<EntryRecord>> {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.read(context, async |session| {
            let mut results = Vec::new();
            let mut cursor = None;
            loop {
                context.check()?;
                let page = session.scan_entries(query, 256, cursor.as_ref()).await?;
                for entry in page
                    .items
                    .into_iter()
                    .filter(|entry| entry_text_matches(entry, &needle))
                {
                    results.push(entry);
                    if results.len() == limit {
                        return Ok(results);
                    }
                }
                cursor = page.next;
                if cursor.is_none() {
                    return Ok(results);
                }
            }
        })
        .await
    }
}

fn entry_text_matches(entry: &EntryRecord, needle: &str) -> bool {
    entry.model.as_ref().is_some_and(|messages| {
        messages.iter().any(|message| {
            if !matches!(
                message.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) {
                return false;
            }
            match message.get("content") {
                Some(Value::String(text)) => text.to_lowercase().contains(needle),
                Some(Value::Array(parts)) => parts.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("text")
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.to_lowercase().contains(needle))
                }),
                _ => false,
            }
        })
    })
}

impl Inner {
    fn hot<T>(
        &self,
        id: u64,
        get: impl FnOnce(&mut CachedStorage) -> Option<Arc<T>>,
        read: impl FnOnce(&dyn Storage) -> StorageResult<Option<T>>,
        keep: impl FnOnce(&mut CachedStorage, Arc<T>),
    ) -> Result<Option<Arc<T>>> {
        if self.options.read_only {
            return Ok(self.read_storage(read)?.map(Arc::new));
        }
        let version = {
            let mut state = lock(&self.state);
            if let Some(cached) = get(&mut state.records) {
                return Ok(Some(cached));
            }
            state.records.start_read(id)
        };
        let read = self.read_storage(read).map(|result| result.map(Arc::new));
        let mut state = lock(&self.state);
        if state.records.end_read(id, version)
            && let Ok(Some(value)) = &read
        {
            keep(&mut state.records, value.clone());
        }
        read
    }

    pub(super) async fn conversation(
        &self,
        id: ConversationId,
    ) -> Result<Option<Arc<ConversationRecord>>> {
        self.touch(&Key::Conversation(id)).await?;
        self.hot(
            id,
            |cache| cache.conversation(id),
            |storage| storage.conversation(id),
            CachedStorage::keep_conversation,
        )
    }

    pub(super) async fn task(&self, id: TaskId) -> Result<Option<Arc<TaskRecord>>> {
        self.touch(&Key::Task(id)).await?;
        self.hot(
            id,
            |cache| cache.task(id),
            |storage| storage.task(id),
            CachedStorage::keep_task,
        )
    }

    pub(super) async fn submission(
        &self,
        id: SubmissionId,
    ) -> Result<Option<Arc<SubmissionRecord>>> {
        self.touch(&Key::Submission(id)).await?;
        if !self.options.read_only
            && let Some(value) = lock(&self.state).records.submission(id)
        {
            return Ok(Some(value));
        }
        Ok(self
            .read_storage(|storage| storage.submission(id))?
            .map(Arc::new))
    }

    pub(super) async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.touch(&Key::Request(conversation, request.to_owned()))
            .await?;
        self.read_storage(|storage| storage.submission_by_request(conversation, request))
    }

    pub(super) async fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.touch(&Key::Entry(id)).await?;
        self.read_storage(|storage| storage.entry(id))
    }

    pub(super) async fn visible_entry(
        &self,
        conversation: ConversationId,
        id: EntryId,
    ) -> Result<Option<StoredEntry>> {
        self.touch(&Key::Conversation(conversation)).await?;
        self.touch(&Key::Entry(id)).await?;
        self.read_storage(|storage| storage.visible_entry(conversation, id))
    }

    pub(super) async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.touch(&Key::All).await?;
        self.read_storage(|storage| storage.scan_conversations(query, limit, cursor))
    }

    pub(super) async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<EntryRecord>> {
        self.touch_entries(query.conversation_id).await?;
        self.read_storage(|storage| storage.scan_entries(query, limit, cursor))
    }

    pub(super) async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<TaskRecord>> {
        self.touch(&Key::All).await?;
        self.read_storage(|storage| storage.scan_tasks(query, limit, cursor))
    }

    async fn touch_entries(&self, conversation: ConversationId) -> Result<()> {
        self.touch(&Key::Conversation(conversation)).await?;
        self.touch(&Key::Append(conversation)).await
    }

    pub(super) async fn latest_entry_id(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<EntryId>> {
        self.touch_entries(conversation).await?;
        if !self.options.read_only
            && let Some(id) = lock(&self.state).tails.latest(conversation)
        {
            return Ok(id);
        }
        let query = EntryQuery {
            conversation_id: conversation,
            ..EntryQuery::default()
        };
        let id = self
            .read_storage(|storage| storage.scan_entries(&query, 1, None))?
            .items
            .first()
            .map(|entry| entry.id);
        if !self.options.read_only {
            lock(&self.state).tails.keep_latest(conversation, id);
        }
        Ok(id)
    }

    pub(super) async fn latest_head_marker(
        &self,
        conversation: ConversationId,
        cutoff: Option<EntryId>,
    ) -> Result<Option<HeadMarker>> {
        self.touch_entries(conversation).await?;
        if !self.options.read_only
            && let Some(marker) = lock(&self.state).tails.marker(conversation, cutoff)
        {
            return Ok(marker);
        }
        let found = self
            .read_storage(|storage| storage.find_latest_head_marker(conversation, cutoff))?
            .map(Arc::new);
        if !self.options.read_only {
            lock(&self.state)
                .tails
                .keep_marker(conversation, cutoff, found.clone());
        }
        Ok(found)
    }

    pub(super) async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
    ) -> Result<Option<DocumentRecord>> {
        self.touch(&Key::Document(address_id(address))).await?;
        self.read_storage(|storage| storage.find_document(address, at))
    }

    pub(super) async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> Result<Option<StoredDocument>> {
        self.touch(&Key::Incarnation(id)).await?;
        self.read_storage(|storage| storage.document(id, at))
    }

    pub(super) async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>> {
        self.touch(&Key::Scope(query.scope)).await?;
        self.read_storage(|storage| storage.scan_documents(query, limit, cursor))
    }

    pub(super) async fn task_documents(&self, id: TaskId) -> Result<Vec<DocumentRecord>> {
        let scope = DocumentScope::Task { task_id: id };
        self.touch(&Key::Scope(scope)).await?;
        if let Some(known) = lock(&self.state).documents.knows_task(id) {
            return Ok(known);
        }
        let query = DocumentQuery {
            scope,
            at: DocumentPoint::Current,
            kind: None,
        };
        let mut cursor = None;
        let mut documents = Vec::new();
        loop {
            let page =
                self.read_storage(|storage| storage.scan_documents(&query, 256, cursor.as_ref()))?;
            documents.extend(page.items);
            cursor = page.next;
            if cursor.is_none() {
                return Ok(documents);
            }
        }
    }

    pub(super) async fn load_document(
        &self,
        definition: &DocToken,
        address: &DocumentAddress,
    ) -> Result<Option<Arc<LoadedDocument>>> {
        validate_address(definition, address)?;
        let key = address_id(address);
        self.touch(&Key::Document(key.clone())).await?;
        let now = self.now();
        if !self.options.read_only {
            let cached = lock(&self.state).documents.get(&key, now);
            if let Some(cached) = cached {
                check_record_scope(definition, &cached.record)?;
                check_record_version(definition, &cached.record, cached.stored_version)?;
                if cached.version == definition.version {
                    self.schedule_maintenance();
                    return Ok(Some(cached));
                }
                let held = lock(&self.state).documents.has_held(cached.record.id);
                if held {
                    self.flush_holds(Some(cached.record.id), &Context::background())
                        .await?;
                }
            }
        }
        let Some(record) =
            self.read_storage(|storage| storage.find_document(address, DocumentPoint::Current))?
        else {
            return Ok(None);
        };
        let StoredDocument {
            record,
            version,
            value,
            deltas_since_base,
        } = self
            .document(record.id, DocumentPoint::Current)
            .await?
            .ok_or_else(|| {
                Error::invalid(format!(
                    "Current document {} ({}) cannot be read",
                    record.id, record.kind
                ))
            })?;
        let value = materialize_document_value(definition, &record, version, value)?;
        let loaded = Arc::new(LoadedDocument::new(
            record,
            definition.version,
            version,
            deltas_since_base,
            value,
        )?);
        if !self.options.read_only {
            lock(&self.state).documents.put(key, loaded.clone(), now);
            self.schedule_maintenance();
        }
        Ok(Some(loaded))
    }
}
