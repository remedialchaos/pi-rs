use super::{
    AGENT_DOC, Conversation, ConversationView, ConversationWindow, INBOX_DOC, LIVE_DOC,
    PROVIDER_DOC, USAGE_DOC,
};
use crate::{
    EntryQuery, EntryRecord, JsonObject, ScanOrder,
    documents::resolve_address,
    session::{Error, Result, Transaction},
};
use pi_rs_chord::context::Context;
use serde_json::Value;
use std::sync::Arc;

async fn scan_window(
    tx: &Transaction<'_>,
    query: EntryQuery,
    limit: usize,
) -> Result<Vec<Arc<EntryRecord>>> {
    let mut result = Vec::new();
    let mut cursor = None;
    while result.len() < limit {
        let page = tx
            .scan_entries(&query, 256.min(limit - result.len()), cursor.as_ref())
            .await?;
        result.extend(page.items.into_iter().map(Arc::new));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(result)
}
impl Conversation {
    pub async fn view(
        &self,
        window: &ConversationWindow,
        ctx: &Context,
    ) -> Result<ConversationView> {
        let min = window.min.unwrap_or(50);
        let max = window.max.unwrap_or_else(|| min.saturating_mul(2));
        if min == 0 || min as u64 > crate::MAX_SAFE_ID {
            return Err(Error::invalid(format!(
                "Window min must be an integer >= 1, got {min}"
            )));
        }
        if max < min || max as u64 > crate::MAX_SAFE_ID {
            return Err(Error::invalid(format!(
                "Window max must be an integer >= min ({min}), got {max}"
            )));
        }
        if window.from.is_some_and(|id| id > crate::MAX_SAFE_ID) {
            return Err(Error::invalid("Window from must be an entry ID"));
        }
        self.session
            .commit(
                async |tx| {
                    let conversation = tx.conversation(self.id).await?.ok_or_else(|| {
                        Error::invalid(format!("Conversation {} does not exist", self.id))
                    })?;
                    let mut docs = JsonObject::new();
                    for token in [
                        &*AGENT_DOC,
                        &*LIVE_DOC,
                        &*INBOX_DOC,
                        &*PROVIDER_DOC,
                        &*USAGE_DOC,
                    ] {
                        let address = resolve_address(token, Some(self.id), None)?;
                        if let Some(value) = tx.document_snapshot(token, &address).await? {
                            docs.insert(token.kind.clone(), Value::Object((*value).clone()));
                        }
                    }
                    let docs = Arc::new(docs);
                    let query = EntryQuery {
                        conversation_id: self.id,
                        ..Default::default()
                    };
                    if let Some(from) = window.from {
                        let entries = scan_window(
                            tx,
                            EntryQuery {
                                min_entry_id: Some(from),
                                order: Some(ScanOrder::Ascending),
                                ..query.clone()
                            },
                            max + 1,
                        )
                        .await?;
                        if entries.len() <= max {
                            let before = if let Some(upper) = from.checked_sub(1) {
                                tx.scan_entries(
                                    &EntryQuery {
                                        max_entry_id: Some(upper),
                                        order: Some(ScanOrder::Descending),
                                        ..query
                                    },
                                    1,
                                    None,
                                )
                                .await?
                                .items
                                .first()
                                .map(|e| e.id)
                            } else {
                                None
                            };
                            return Ok(ConversationView {
                                conversation,
                                docs,
                                entries,
                                before,
                            });
                        }
                    }
                    let mut entries = scan_window(
                        tx,
                        EntryQuery {
                            order: Some(ScanOrder::Descending),
                            ..query
                        },
                        min + 1,
                    )
                    .await?;
                    let before = if entries.len() > min {
                        entries.pop().map(|e| e.id)
                    } else {
                        None
                    };
                    entries.reverse();
                    Ok(ConversationView {
                        conversation,
                        docs,
                        entries,
                        before,
                    })
                },
                ctx,
            )
            .await
    }
}

use futures_util::future::BoxFuture;
use pi_rs_chord::watch::{NativeWatch, WatchEnd};
use tokio::sync::watch;

async fn observe<T: Clone + Send + Sync + 'static>(
    session: &crate::Session,
    ctx: &Context,
    mut registry: Option<watch::Receiver<Arc<super::RegistrySnapshot>>>,
    read: impl Fn(Context) -> BoxFuture<'static, Result<T>> + Send + 'static,
) -> Result<NativeWatch<T>> {
    let (changes, mut changed) = watch::channel(());
    let commit = session.follow_commits(move |_, _| {
        changes.send_replace(());
        Ok(())
    })?;
    let (closed, mut closing) = tokio::sync::oneshot::channel();
    let closer = std::sync::Mutex::new(Some(closed));
    let close = session.subscribe_close(move || {
        if let Some(tx) = closer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
    })?;
    let initial = read(ctx.clone()).await?;
    let (context, cancel) = ctx.with_cancel();
    let view = NativeWatch::new(
        initial,
        move || {
            cancel.cancel("Observation ended");
        },
        |_| {},
    );
    let output = view.clone();
    tokio::spawn(async move {
        let _subscriptions = (commit, close);
        loop {
            let refresh=context.await_with(async{
                tokio::select!{
                    _=&mut closing=>false,
                    changed=changed.changed()=>changed.is_ok(),
                    changed=async {match &mut registry{Some(r)=>r.changed().await.is_ok(),None=>std::future::pending().await}}=>changed
                }
            }).await;
            match refresh {
                Ok(true) => match read(context.clone()).await {
                    Ok(value) => output.advance(value, context.clone()),
                    Err(error) => {
                        output.end(WatchEnd::Failed(error.to_string()));
                        break;
                    }
                },
                Ok(false) => {
                    output.end(WatchEnd::Ended);
                    break;
                }
                Err(_) => {
                    output.end(WatchEnd::Aborted);
                    break;
                }
            }
        }
    });
    Ok(view)
}

impl Conversation {
    pub async fn watch(
        &self,
        window: ConversationWindow,
        ctx: &Context,
    ) -> Result<NativeWatch<ConversationView>> {
        let conv = self.clone();
        let window = Arc::new(std::sync::Mutex::new(window));
        observe(&self.session, ctx, None, move |ctx| {
            let conv = conv.clone();
            let window = window.clone();
            Box::pin(async move {
                let prior = window.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let value = conv.view(&prior, &ctx).await?;
                window.lock().unwrap_or_else(|e| e.into_inner()).from =
                    value.entries.first().map(|e| e.id);
                Ok(value)
            })
        })
        .await
    }
}
impl super::Harness {
    pub async fn watch_task_graph(
        &self,
        ctx: &Context,
    ) -> Result<NativeWatch<Arc<super::TaskGraph>>> {
        let harness = self.clone();
        observe(
            &self.session,
            ctx,
            Some(self.registry.subscribe()),
            move |ctx| {
                let harness = harness.clone();
                Box::pin(async move {
                    let registry = harness.registry.snapshot();
                    let graph = harness
                        .task_graph(
                            |r| match registry.task(&r.kind) {
                                None => Some(super::BlockedReason::MissingTask),
                                Some(t) if t.version() < r.version => {
                                    Some(super::BlockedReason::TaskTooOld)
                                }
                                _ => None,
                            },
                            &ctx,
                        )
                        .await?;
                    Ok(Arc::new(graph))
                })
            },
        )
        .await
    }
    pub async fn observe_conversation(
        &self,
        id: crate::ConversationId,
        window: ConversationWindow,
        ctx: &Context,
    ) -> Result<NativeWatch<ConversationView>> {
        self.handle(id).watch(window, ctx).await
    }
}
