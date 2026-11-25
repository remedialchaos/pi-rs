use super::USAGE_DOC;
use crate::{
    ConversationId,
    documents::resolve_address,
    session::{Result, Transaction},
};
use indexmap::IndexMap;
use pi_rs_ai::types::Usage;
use pi_rs_chord::delta::Segment;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct UsageState {
    pub models: IndexMap<String, Usage>,
    pub tools: IndexMap<String, Usage>,
}
#[derive(Clone, Copy)]
pub enum UsageBucket {
    Models,
    Tools,
}

pub fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.total_tokens += usage.total_tokens;
    if let Some(value) = usage.reasoning {
        *total.reasoning.get_or_insert(0.) += value;
    }
    if let Some(value) = usage.cache_write_1h {
        *total.cache_write_1h.get_or_insert(0.) += value;
    }
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

impl UsageState {
    pub fn add(&mut self, state: &Self) {
        for (totals, values) in [
            (&mut self.models, &state.models),
            (&mut self.tools, &state.tools),
        ] {
            for (key, usage) in values {
                totals
                    .entry(key.clone())
                    .and_modify(|total| add_usage(total, usage))
                    .or_insert(*usage);
            }
        }
    }
}

pub async fn record_usage(
    tx: &mut Transaction<'_>,
    conversation: ConversationId,
    bucket: UsageBucket,
    key: &str,
    usage: &Usage,
) -> Result<()> {
    let address = resolve_address(&USAGE_DOC, Some(conversation), None)?;
    let draft = tx.doc(&USAGE_DOC, &address, None).await?;
    let name = match bucket {
        UsageBucket::Models => "models",
        UsageBucket::Tools => "tools",
    };
    let value = serde_json::to_value(draft.value()?.to_json())?;
    let mut entries = value[name]
        .as_object()
        .cloned()
        .ok_or_else(|| crate::session::Error::invalid("Invalid usage ledger"))?;
    let next = if let Some(current) = entries.get(key) {
        let mut total: Usage = serde_json::from_value(current.clone())?;
        add_usage(&mut total, usage);
        total
    } else {
        *usage
    };
    entries.insert(key.into(), serde_json::to_value(next)?);
    draft.set(
        &[Segment::Key(name.into())],
        serde_json::Value::Object(entries).into(),
    )?;
    Ok(())
}
