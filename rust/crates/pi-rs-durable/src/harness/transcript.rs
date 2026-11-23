//! Accumulate sparse, fork-aware entry windows over reconnects.
use crate::{
    ConversationId, ConversationRecord, EntryId, EntryQuery, EntryRecord, JsonObject, Page,
    ScanOrder,
    session::{Error, Result},
};
use async_trait::async_trait;
use pi_rs_chord::context::Context;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConversationWindow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<EntryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}
#[derive(Clone, Debug)]
pub struct ConversationView {
    pub conversation: Arc<ConversationRecord>,
    pub docs: Arc<JsonObject>,
    pub entries: Vec<Arc<EntryRecord>>,
    pub before: Option<EntryId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub after: EntryId,
    pub up_to: EntryId,
}
#[derive(Clone, Debug)]
pub enum TranscriptItem {
    Entry(Arc<EntryRecord>),
    Gap(Gap),
    Earlier,
}
#[derive(Clone, Debug)]
pub struct TranscriptView {
    pub conversation: Arc<ConversationRecord>,
    pub docs: Arc<JsonObject>,
    pub items: Arc<Vec<TranscriptItem>>,
}
#[async_trait]
pub trait EntryLoader: Send + Sync {
    async fn load(
        &self,
        query: EntryQuery,
        limit: usize,
        context: &Context,
    ) -> Result<Page<EntryRecord>>;
}

pub struct ConversationTranscript {
    loader: Arc<dyn EntryLoader>,
    size: usize,
    entries: BTreeMap<EntryId, Arc<EntryRecord>>,
    gaps: Vec<Gap>,
    low: Option<EntryId>,
    tail: Option<EntryId>,
    view: Option<TranscriptView>,
    items: Arc<Vec<TranscriptItem>>,
}
impl ConversationTranscript {
    pub fn new(loader: Arc<dyn EntryLoader>, page_size: Option<usize>) -> Result<Self> {
        let size = page_size.unwrap_or(50);
        if size == 0 || size as u64 > crate::MAX_SAFE_ID {
            return Err(Error::invalid(format!(
                "Page size must be an integer >= 1, got {size}"
            )));
        }
        Ok(Self {
            loader,
            size,
            entries: BTreeMap::new(),
            gaps: Vec::new(),
            low: None,
            tail: None,
            view: None,
            items: Arc::default(),
        })
    }
    pub fn newest(&self) -> Option<EntryId> {
        self.tail
    }
    pub fn resume(&self) -> Option<ConversationWindow> {
        self.view.as_ref().map(|_| ConversationWindow {
            from: self.tail.map(|id| id + 1),
            ..Default::default()
        })
    }
    pub fn merge(&mut self, view: ConversationView) -> TranscriptView {
        let hi = view.entries.last().map(|e| e.id).or(view.before);
        self.cover(view.before, hi, view.entries);
        let result = TranscriptView {
            conversation: view.conversation,
            docs: view.docs,
            items: self.items.clone(),
        };
        self.view = Some(result.clone());
        result
    }
    fn current(&self) -> Result<TranscriptView> {
        self.view
            .clone()
            .ok_or_else(|| Error::invalid("ConversationTranscript: merge a view first"))
    }
    pub async fn load_earlier(&mut self, context: &Context) -> Result<TranscriptView> {
        let view = self.current()?;
        let Some(low) = self.low else { return Ok(view) };
        let mut page = self
            .page(
                view.conversation.id,
                None,
                Some(low),
                ScanOrder::Descending,
                context,
            )
            .await?;
        page.items.reverse();
        let lo = page
            .next
            .and_then(|_| page.items.first()?.id.checked_sub(1));
        self.cover(
            lo,
            Some(low),
            page.items.into_iter().map(Arc::new).collect(),
        );
        self.current()
    }
    pub async fn fill_gap(&mut self, gap: Gap, context: &Context) -> Result<TranscriptView> {
        let view = self.current()?;
        let Some(current) = self.gaps.iter().copied().find(|g| *g == gap).or_else(|| {
            self.gaps
                .iter()
                .copied()
                .find(|g| g.after < gap.up_to && gap.after < g.up_to)
        }) else {
            return Ok(view);
        };
        let page = self
            .page(
                view.conversation.id,
                Some(current.after + 1),
                Some(current.up_to - 1),
                ScanOrder::Ascending,
                context,
            )
            .await?;
        let hi = page
            .next
            .and_then(|_| page.items.last().map(|e| e.id))
            .unwrap_or(current.up_to - 1);
        self.cover(
            Some(current.after),
            Some(hi),
            page.items.into_iter().map(Arc::new).collect(),
        );
        self.current()
    }
    async fn page(
        &self,
        conversation_id: ConversationId,
        min_entry_id: Option<EntryId>,
        max_entry_id: Option<EntryId>,
        order: ScanOrder,
        ctx: &Context,
    ) -> Result<Page<EntryRecord>> {
        self.loader
            .load(
                EntryQuery {
                    conversation_id,
                    min_entry_id,
                    max_entry_id,
                    order: Some(order),
                },
                self.size,
                ctx,
            )
            .await
    }
    fn cover(&mut self, lo: Option<EntryId>, hi: Option<EntryId>, added: Vec<Arc<EntryRecord>>) {
        let empty = self.entries.is_empty();
        let first = added.first().map(|e| e.id);
        let count = self.entries.len();
        for entry in added {
            self.entries.entry(entry.id).or_insert(entry);
        }
        let old_low = self.low;
        let old_gaps = self.gaps.clone();
        if self.view.is_none() || (empty && self.gaps.is_empty() && lo >= self.tail) {
            self.low = lo;
            self.tail = self.tail.max(hi);
        } else if lo > self.tail {
            if let (Some(after), Some(lower)) = (self.tail, lo) {
                self.gaps.push(Gap {
                    after,
                    up_to: first.unwrap_or(lower + 1),
                });
            }
            self.tail = hi;
        } else if let (Some(high), Some(low)) = (hi, self.low)
            && high < low
        {
            let up_to = self
                .entries
                .range(high + 1..)
                .next()
                .map(|(id, _)| *id)
                .unwrap_or(low + 1);
            self.gaps.insert(0, Gap { after: high, up_to });
            self.low = lo;
        } else {
            if let Some(high) = hi
                && hi > lo
            {
                let mut remaining = Vec::new();
                for gap in self.gaps.drain(..) {
                    if high <= gap.after || lo.is_some_and(|id| id + 1 >= gap.up_to) {
                        remaining.push(gap);
                        continue;
                    }
                    if let Some(lower) = lo
                        && lower > gap.after
                    {
                        let next = self
                            .entries
                            .range(lower + 1..)
                            .next()
                            .map(|(id, _)| *id)
                            .unwrap_or(lower + 1);
                        remaining.push(Gap {
                            after: gap.after,
                            up_to: gap.up_to.min(next),
                        });
                    }
                    if high + 1 < gap.up_to {
                        remaining.push(Gap {
                            after: high,
                            up_to: gap.up_to,
                        });
                    }
                }
                self.gaps = remaining;
            }
            self.low = self.low.min(lo);
            self.tail = self.tail.max(hi);
        }
        if count != self.entries.len() || self.gaps != old_gaps || self.low != old_low {
            let mut items = Vec::new();
            if self.low.is_some() {
                items.push(TranscriptItem::Earlier);
            }
            let mut gaps = self.gaps.iter().peekable();
            for entry in self.entries.values() {
                while gaps.peek().is_some_and(|g| g.after < entry.id) {
                    if let Some(gap) = gaps.next() {
                        items.push(TranscriptItem::Gap(*gap));
                    }
                }
                items.push(TranscriptItem::Entry(entry.clone()));
            }
            items.extend(gaps.copied().map(TranscriptItem::Gap));
            self.items = Arc::new(items);
            if let Some(view) = &mut self.view {
                view.items = self.items.clone();
            }
        }
    }
}
