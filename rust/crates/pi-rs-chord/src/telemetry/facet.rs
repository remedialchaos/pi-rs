use super::Recorder;
use crate::{
    Result,
    context::lock,
    data::{ChordError, Validator},
    telemetry_ingest::{PageBudgets, TelemetryBatch},
};
use pi_rs_telemetry::SpanAttributes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn text(max: usize, min: usize) -> Value {
    json!({"type":"string","minLength":min,"maxLength":max,"pattern":"^[^\\u0000-\\u001f\\u007f-\\u009f]*$"})
}
fn id(max: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max,"pattern":"^[\\x21-\\x7e]+$"})
}
fn attrs(count: usize, value: Value) -> Value {
    json!({"type":"object","maxProperties":count,"patternProperties":{"^[^\\u0000-\\u001f\\u007f-\\u009f]{1,64}$":value},"additionalProperties":false})
}
fn span(worker: bool) -> Value {
    let id = id(if worker { 256 } else { 128 });
    let name = text(if worker { 1024 } else { 128 }, 1);
    let value = if worker {
        json!({"anyOf":[{"type":"string","maxLength":65536},{"type":"number"},{"type":"boolean"},{"type":"array","items":{"anyOf":[{"type":"string"},{"type":"number"}]}}]})
    } else {
        json!({"anyOf":[text(256,0),{"type":"number"},{"type":"boolean"}]})
    };
    let attrs = attrs(if worker { 256 } else { 32 }, value);
    let time = json!({"type":"number","minimum":0});
    let error = object(
        json!({"name":text(64,0),"message":text(500,0)}),
        &["name", "message"],
    );
    object(
        json!({"id":id,"traceId":id,"parentId":id,"name":name,"startedMs":time,"endedMs":time,"attributes":attrs,
        "events":{"type":"array","maxItems":if worker{1024}else{16},"items":object(json!({"name":name,"timeMs":time,"attributes":attrs}),&["name","timeMs","attributes"])},
        "links":{"type":"array","maxItems":if worker{256}else{8},"items":object(json!({"traceId":id,"spanId":id,"attributes":attrs}),&["traceId","spanId"])},
        "status":{"anyOf":[object(json!({"status":{"const":"ok"}}),&["status"]),object(json!({"status":{"const":"error"},"error":error}),&["status"])]}}),
        &[
            "id",
            "traceId",
            "name",
            "startedMs",
            "endedMs",
            "attributes",
            "events",
            "links",
            "status",
        ],
    )
}
pub fn page_batch_schema() -> Value {
    object(
        json!({"spans":{"type":"array","maxItems":100,"items":span(false)}}),
        &["spans"],
    )
}
pub fn worker_batch_schema() -> Value {
    let metric = object(
        json!({"intervalMs":{"type":"integer","minimum":0},"name":text(256,1),"kind":{"enum":["counter","upDownCounter","histogram"]},"unit":text(64,0),"attributes":attrs(64,json!({"anyOf":[{"type":"string","maxLength":4096},{"type":"number"},{"type":"boolean"}]})),"count":{"type":"integer","minimum":0},"sum":{"type":"number"},"min":{"type":"number"},"max":{"type":"number"}}),
        &[
            "intervalMs",
            "name",
            "kind",
            "attributes",
            "count",
            "sum",
            "min",
            "max",
        ],
    );
    object(
        json!({"spans":{"type":"array","maxItems":1000,"items":span(true)},"metrics":{"type":"array","maxItems":1000,"items":metric}}),
        &["spans", "metrics"],
    )
}
pub type Target = Arc<dyn Fn(TelemetryBatch) -> Result<usize> + Send + Sync>;
pub type WorkerHostResolver = Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>;
pub struct IngestOptions {
    pub target: Target,
    pub admit: Arc<dyn Fn(&Value) -> bool + Send + Sync>,
    pub worker_host: WorkerHostResolver,
    pub principal_attributes: Arc<dyn Fn(&Value) -> SpanAttributes + Send + Sync>,
}
#[derive(Clone)]
pub struct IngestFacet {
    options: Arc<IngestOptions>,
    page: Arc<Validator>,
    worker: Arc<Validator>,
}
/// The host bridge installs these two untraced version-1 methods using this crate's checked handlers.
pub trait IngestRegistry {
    fn register(
        &mut self,
        name: &str,
        variant: &str,
        batch_schema: Value,
        facet: IngestFacet,
    ) -> Result<()>;
}
impl IngestFacet {
    pub fn new(options: IngestOptions) -> Result<Self> {
        Ok(Self {
            options: Arc::new(options),
            page: Arc::new(Validator::compile(&page_batch_schema())?),
            worker: Arc::new(Validator::compile(&worker_batch_schema())?),
        })
    }
    pub fn for_recorder(
        recorder: Recorder,
        admit: impl Fn(&Value) -> bool + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::new(IngestOptions {
            target: Arc::new(move |b| recorder.ingest(b)),
            admit: Arc::new(admit),
            worker_host: Arc::new(|_| None),
            principal_attributes: Arc::new(|_| SpanAttributes::new()),
        })
    }
    pub fn register(&self, registry: &mut dyn IngestRegistry) -> Result<()> {
        registry.register(
            "chord.telemetry.page",
            "main",
            page_batch_schema(),
            self.clone(),
        )?;
        registry.register(
            "chord.telemetry.worker",
            "worker",
            worker_batch_schema(),
            self.clone(),
        )
    }
    pub fn page(&self, principal: Value) -> Result<PageIngest> {
        if !(self.options.admit)(&principal) {
            return Err(ChordError::new("forbidden", "chord.telemetry.page: not admitted").into());
        }
        Ok(PageIngest {
            facet: self.clone(),
            principal,
            budgets: Mutex::new(PageBudgets::default()),
        })
    }
    pub fn worker(&self, principal: &Value) -> Option<WorkerIngest> {
        (self.options.worker_host)(principal).map(|host| WorkerIngest {
            facet: self.clone(),
            host,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub written: usize,
    pub dropped: usize,
}
pub struct PageIngest {
    facet: IngestFacet,
    principal: Value,
    budgets: Mutex<PageBudgets>,
}
impl PageIngest {
    pub fn ingest(&self, incarnation: &str, now: f64, raw: Value) -> Result<Receipt> {
        self.facet.page.check(&raw, "args")?;
        let spans = serde_json::from_value(raw["spans"].clone())?;
        let total = raw["spans"].as_array().map_or(0, Vec::len);
        let accepted = lock(&self.budgets).filter(incarnation, now, spans, || {
            (self.facet.options.principal_attributes)(&self.principal)
        });
        let written = (self.facet.options.target)(TelemetryBatch {
            spans: accepted,
            metrics: Vec::new(),
        })?;
        Ok(Receipt {
            written,
            dropped: total.saturating_sub(written),
        })
    }
}
pub struct WorkerIngest {
    facet: IngestFacet,
    host: String,
}
impl WorkerIngest {
    pub fn ingest(&self, raw: Value) -> Result<Receipt> {
        self.facet.worker.check(&raw, "args")?;
        let mut batch: TelemetryBatch = serde_json::from_value(raw)?;
        let total = batch.spans.len();
        for span in &mut batch.spans {
            span.attributes
                .insert("chord.host".into(), self.host.clone().into());
        }
        let written = (self.facet.options.target)(batch)?;
        Ok(Receipt {
            written,
            dropped: total.saturating_sub(written),
        })
    }
}
