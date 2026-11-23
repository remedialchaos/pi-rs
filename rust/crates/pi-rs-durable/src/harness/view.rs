pub(super) mod delta;
use delta::key;
use pi_rs_chord::{
    delta::Op,
    json::JsonValue,
    watch::{Watch, WatchCall},
};
use std::collections::HashMap;
type Incarnations = HashMap<String, (crate::DocumentId, u64)>;
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

fn wire(view: &ConversationView) -> Result<JsonValue> {
    let mut object = JsonObject::new();
    object.insert(
        "conversation".into(),
        serde_json::to_value(view.conversation.as_ref())?,
    );
    let entries = view
        .entries
        .iter()
        .map(|entry| serde_json::to_value(entry.as_ref()))
        .collect::<serde_json::Result<Vec<_>>>()?;
    object.insert("entries".into(), Value::Array(entries));
    if let Some(before) = view.before {
        object.insert("before".into(), before.into());
    }
    object.insert("docs".into(), Value::Object((*view.docs).clone()));
    Ok(Value::Object(object).into())
}

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
        self.attach_view(window, ctx, |view, _| Ok(view)).await
    }

    async fn attach_view<T>(
        &self,
        window: &ConversationWindow,
        ctx: &Context,
        attach: impl FnOnce(ConversationView, Incarnations) -> Result<T>,
    ) -> Result<T> {
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
                    let mut incarnations = Incarnations::new();
                    let mut docs = JsonObject::new();
                    for token in [
                        &*AGENT_DOC,
                        &*LIVE_DOC,
                        &*INBOX_DOC,
                        &*PROVIDER_DOC,
                        &*USAGE_DOC,
                    ] {
                        let address = resolve_address(token, Some(self.id), None)?;
                        if let Some((id, version, value)) =
                            tx.document_snapshot(token, &address).await?
                        {
                            incarnations.insert(token.kind.clone(), (id, version));
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
                            return attach(
                                ConversationView {
                                    conversation,
                                    docs,
                                    entries,
                                    before,
                                },
                                incarnations,
                            );
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
                    attach(
                        ConversationView {
                            conversation,
                            docs,
                            entries,
                            before,
                        },
                        incarnations,
                    )
                },
                ctx,
            )
            .await
    }
}

use crate::session::{Observation, Observer, Subscription};
use pi_rs_chord::watch::WatchEnd;
use std::sync::atomic::Ordering;
impl Conversation {
    pub async fn watch(&self, window: ConversationWindow, ctx: &Context) -> Result<Watch> {
        let watch = self
            .attach_view(&window, ctx, |initial, incarnations| {
                let holder = Arc::new(std::sync::Mutex::new(None));
                let release = holder.clone();
                let watch = Watch::new(
                    wire(&initial)?,
                    move || {
                        release.lock().unwrap_or_else(|e| e.into_inner()).take();
                    },
                    |_| {},
                );
                let output = watch.clone();
                let ending = watch.clone();
                let observer = Observer::new(
                    move |call: &WatchCall<ConversationView>, context| {
                        if let WatchCall::Value {
                            value,
                            ops,
                            retired,
                        } = call
                        {
                            output.advance(
                                WatchCall::Value {
                                    value: wire(value)?,
                                    ops: ops.clone(),
                                    retired: *retired,
                                },
                                context.clone(),
                            );
                        }
                        Ok(())
                    },
                    move |end| {
                        ending.end(end);
                    },
                );
                *holder.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(self.listen_view(initial, incarnations, &window, observer)?);
                if let Some(signal) = ctx.abort_signal()
                    && let Err(error) = watch.observe_abort(signal)
                {
                    watch.stop();
                    return Err(Error::invalid(error.to_string()));
                }
                Ok(watch)
            })
            .await?;
        if let Err(error) = ctx.check() {
            watch.stop();
            return Err(error.into());
        }
        Ok(watch)
    }

    fn listen_view(
        &self,
        initial: ConversationView,
        incarnations: Incarnations,
        window: &ConversationWindow,
        observer: Observer<WatchCall<ConversationView>>,
    ) -> Result<Subscription> {
        let state = std::sync::Mutex::new((initial, incarnations));
        let observer = Arc::new(observer);
        let listener = observer.clone();
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let enabled = active.clone();
        let min = window.min.unwrap_or(50);
        let max = window.max.unwrap_or(2 * min);
        let id = self.id;
        let commits = self.session.follow_commits(move |publication, context| {
            use crate::CommitChange;
            use crate::session::ObservedCommit;
            let changes = match publication {
                ObservedCommit::Stored(commit) => &commit.changes,
                ObservedCommit::Memory(changes) => changes,
            };
            if !enabled.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let (previous, incarnations) = &mut *state;
            let mut view = previous.clone();
            let mut ops = Vec::new();
            let mut inserted = Vec::new();
            for change in changes {
                match change {
                    CommitChange::Entry { value } if value.conversation_id == id => {
                        view.entries.push(Arc::new(value.clone()));
                        inserted.push(JsonValue::from_serde(serde_json::to_value(value)?));
                    }
                    CommitChange::Document(doc) => {
                        delta::advance_document(id, doc, incarnations, &mut view.docs, &mut ops);
                    }
                    _ => {}
                }
            }
            if !inserted.is_empty() {
                ops.push(Op::Splice(
                    vec![key("entries")],
                    previous.entries.len(),
                    0,
                    inserted,
                ));
            }
            if !ops.is_empty() {
                if view.entries.len() > max {
                    let drop_count = view.entries.len() - min;
                    let before = view.entries[drop_count - 1].id;
                    view.before = Some(before);
                    ops.push(Op::Splice(vec![key("entries")], 0, drop_count, vec![]));
                    ops.push(Op::Set(
                        vec![key("before")],
                        JsonValue::Number(before as f64),
                    ));
                    view.entries.drain(..drop_count);
                }
                *previous = view.clone();
                drop(state);
                (listener.advance)(
                    &WatchCall::Value {
                        value: view,
                        ops,
                        retired: false,
                    },
                    &context.without_abort_signal(),
                )
                .map_err(|e| Error::invalid(e.to_string()))?;
            }
            Ok(())
        })?;
        let session = self.session.clone();
        let closing = active.clone();
        let close = self.session.subscribe_close(move || {
            if closing.swap(false, Ordering::AcqRel) {
                (observer.end)(session.failure().map_or(WatchEnd::Ended, WatchEnd::Failed));
            }
        })?;
        Ok(Subscription::new(move || {
            active.store(false, Ordering::Release);
            drop((commits, close));
        }))
    }
}
impl super::Harness {
    pub async fn observe_conversation(
        &self,
        id: crate::ConversationId,
        window: ConversationWindow,
        observer: Observer<WatchCall<ConversationView>>,
        ctx: &Context,
    ) -> Result<Observation<ConversationView>> {
        let conversation = self.handle(id);
        let observation = conversation
            .attach_view(&window, ctx, |value, incarnations| {
                ctx.check()?;
                let subscription = conversation.listen_view(
                    value.clone(),
                    incarnations,
                    &window,
                    host_observer(observer),
                )?;
                Ok(Observation {
                    value,
                    subscription,
                })
            })
            .await?;
        ctx.check()?;
        Ok(observation)
    }
}

/// Adapters reopen a closed session. App watches instead finish normally.
pub(super) fn host_observer<T: 'static>(observer: Observer<T>) -> Observer<T> {
    let current = Arc::new(std::sync::Mutex::new(Some(Arc::new(observer))));
    let advancing = current.clone();
    Observer::new(
        move |value, ctx| {
            let target = advancing.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(target) = target {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (target.advance)(value, ctx)
                }));
                let error = match result {
                    Ok(Ok(())) => return Ok(()),
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "observer panicked".to_owned(),
                };
                let failed = advancing.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(failed) = failed {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (failed.end)(WatchEnd::ListenerError(error))
                    }));
                }
            }
            Ok(())
        },
        move |end| {
            let target = current.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(target) = target {
                (target.end)(match end {
                    WatchEnd::Ended => WatchEnd::Failed("session closed".into()),
                    other => other,
                });
            }
        },
    )
}
