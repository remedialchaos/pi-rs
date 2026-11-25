use crate::{
    Error, Result,
    context::lock,
    telemetry_ingest::{INTERVAL_MS, TelemetryBatch, TelemetryRecorder, sorted_attributes},
};
use pi_rs_telemetry::{
    AttributeValue, ErrorDetails, MetricKind, SpanAttributes, SpanStatus, StoredEvent, StoredLink,
    StoredMetric, StoredSpan, TelemetryCarrier,
};
use std::{
    future::Future,
    sync::{Arc, Mutex, Weak},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanIdentity {
    pub id: String,
    pub trace_id: String,
}
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Inner>,
}
struct Inner {
    queue: Mutex<TelemetryRecorder>,
    identity: Mutex<(Option<String>, u64)>,
    scope: Arc<dyn Fn() -> String + Send + Sync>,
    clock: Arc<dyn Fn() -> f64 + Send + Sync>,
}
impl Recorder {
    pub fn new(scope: impl Fn() -> String + Send + Sync + 'static, capacity: usize) -> Self {
        Self::with_clock(scope, capacity, || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
        })
    }
    pub fn with_clock(
        scope: impl Fn() -> String + Send + Sync + 'static,
        capacity: usize,
        clock: impl Fn() -> f64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(TelemetryRecorder::new(capacity)),
                identity: Mutex::new((None, 0)),
                scope: Arc::new(scope),
                clock: Arc::new(clock),
            }),
        }
    }
    pub fn context(&self) -> RecordingContext {
        RecordingContext {
            recorder: self.clone(),
            identity: None,
            parent: None,
            inert: false,
        }
    }
    pub fn extract(&self, carrier: Option<&TelemetryCarrier>) -> RecordingContext {
        let identity = carrier.and_then(|c| {
            Some(SpanIdentity {
                id: c.get("pi-sqlite-span")?.clone(),
                trace_id: c.get("pi-sqlite-trace")?.clone(),
            })
        });
        RecordingContext {
            recorder: self.clone(),
            identity,
            parent: None,
            inert: false,
        }
    }
    pub fn ingest(&self, batch: TelemetryBatch) -> Result<usize> {
        lock(&self.inner.queue).ingest(batch)
    }
    pub fn take(&self, partial: bool, limit: usize) -> TelemetryBatch {
        lock(&self.inner.queue).take(partial, limit, self.now())
    }
    pub fn release(&self) {
        lock(&self.inner.queue).release();
    }
    pub fn pending(&self, partial: bool) -> bool {
        lock(&self.inner.queue).pending(partial, self.now())
    }
    pub fn queued_spans(&self) -> usize {
        lock(&self.inner.queue).queued_spans()
    }
    pub fn stop(&self, discard: bool) {
        lock(&self.inner.queue).stop(discard);
    }
    pub fn stopped(&self) -> bool {
        lock(&self.inner.queue).stopped()
    }
    pub fn dropped(&self) -> (u64, u64) {
        let q = lock(&self.inner.queue);
        (q.dropped_spans, q.dropped_measurements)
    }
    pub fn now(&self) -> f64 {
        (self.inner.clock)()
    }
    pub fn measure(
        &self,
        name: String,
        kind: MetricKind,
        unit: Option<String>,
        value: f64,
        attributes: SpanAttributes,
    ) -> Result<()> {
        if !value.is_finite() {
            return Err(Error::Invalid("telemetry: non-finite measurement".into()));
        }
        let metric = StoredMetric {
            interval_ms: (self.now() / INTERVAL_MS).floor() * INTERVAL_MS,
            name,
            kind,
            unit,
            attributes: sorted_attributes(&attributes),
            count: 1,
            sum: value,
            min: value,
            max: value,
        };
        self.ingest(TelemetryBatch {
            spans: Vec::new(),
            metrics: vec![metric],
        })?;
        Ok(())
    }
}
#[derive(Clone)]
pub struct RecordingContext {
    recorder: Recorder,
    identity: Option<SpanIdentity>,
    parent: Option<Weak<Mutex<SpanData>>>,
    inert: bool,
}
impl RecordingContext {
    pub fn identity(&self) -> Option<&SpanIdentity> {
        self.identity.as_ref()
    }
    pub fn inject(&self) -> Option<TelemetryCarrier> {
        self.identity.as_ref().map(|id| {
            TelemetryCarrier::from([
                ("pi-sqlite-span".into(), id.id.clone()),
                ("pi-sqlite-trace".into(), id.trace_id.clone()),
            ])
        })
    }
    pub fn open(
        &self,
        name: impl Into<String>,
        attributes: SpanAttributes,
    ) -> Result<RecordingSpan> {
        let inert = self.inert
            || self.recorder.stopped()
            || self
                .parent
                .as_ref()
                .is_some_and(|p| p.upgrade().is_none_or(|p| lock(&p).settled));
        if inert {
            return Ok(RecordingSpan {
                context: RecordingContext {
                    inert: true,
                    identity: None,
                    parent: None,
                    ..self.clone()
                },
                data: None,
            });
        }
        let started = self.recorder.now();
        if !started.is_finite() {
            return Err(Error::Invalid("telemetry: invalid clock".into()));
        }
        let id = {
            let mut ids = lock(&self.recorder.inner.identity);
            if ids.0.is_none() {
                ids.0 = Some((self.recorder.inner.scope)());
            }
            ids.1 = ids
                .1
                .checked_add(1)
                .ok_or_else(|| Error::Invalid("telemetry: sequence exhausted".into()))?;
            format!("{}:{}", ids.0.as_deref().unwrap_or(""), ids.1)
        };
        let identity = SpanIdentity {
            trace_id: self
                .identity
                .as_ref()
                .map_or_else(|| id.clone(), |p| p.trace_id.clone()),
            id: id.clone(),
        };
        let data = Arc::new(Mutex::new(SpanData {
            span: StoredSpan {
                id,
                trace_id: identity.trace_id.clone(),
                parent_id: self.identity.as_ref().map(|p| p.id.clone()),
                name: name.into(),
                started_ms: started,
                ended_ms: started,
                attributes: copy_attributes(attributes),
                events: Vec::new(),
                links: Vec::new(),
                status: SpanStatus::Ok,
            },
            clock: Instant::now(),
            settled: false,
            explicit: false,
        }));
        Ok(RecordingSpan {
            context: RecordingContext {
                recorder: self.recorder.clone(),
                identity: Some(identity),
                parent: Some(Arc::downgrade(&data)),
                inert: false,
            },
            data: Some(data),
        })
    }
    pub async fn trace<T, F, Fut>(
        &self,
        name: impl Into<String>,
        attributes: SpanAttributes,
        run: F,
    ) -> Result<T>
    where
        F: FnOnce(RecordingSpan) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let span = self.open(name, attributes)?;
        let result = run(span.clone()).await;
        span.finish(result.as_ref().err().map(|e| ErrorDetails {
            name: "Error".into(),
            message: e.to_string(),
        }))?;
        result
    }
}
struct SpanData {
    span: StoredSpan,
    clock: Instant,
    settled: bool,
    explicit: bool,
}
#[derive(Clone)]
pub struct RecordingSpan {
    context: RecordingContext,
    data: Option<Arc<Mutex<SpanData>>>,
}
impl RecordingSpan {
    pub fn context(&self) -> &RecordingContext {
        &self.context
    }
    pub fn add_event(&self, name: impl Into<String>, attributes: SpanAttributes) {
        self.record(|data| {
            data.span.events.push(StoredEvent {
                name: name.into(),
                time_ms: micros(data.clock.elapsed().as_secs_f64() * 1000.0),
                attributes: copy_attributes(attributes),
            })
        });
    }
    pub fn attributes(&self, attributes: SpanAttributes) {
        self.record(|data| data.span.attributes.extend(copy_attributes(attributes)));
    }
    pub fn status(&self, status: SpanStatus) {
        self.record(|data| {
            data.span.status = status;
            data.explicit = true;
        });
    }
    pub fn add_link(&self, context: &RecordingContext, attributes: SpanAttributes) {
        if let Some(id) = context.identity() {
            self.record(|data| {
                data.span.links.push(StoredLink {
                    trace_id: id.trace_id.clone(),
                    span_id: id.id.clone(),
                    attributes: copy_attributes(attributes),
                })
            });
        }
    }
    fn record(&self, run: impl FnOnce(&mut SpanData)) {
        if self.context.recorder.stopped() {
            return;
        }
        if let Some(data) = &self.data {
            let mut data = lock(data);
            if !data.settled {
                run(&mut data);
            }
        }
    }
    pub fn finish(&self, error: Option<ErrorDetails>) -> Result<()> {
        let Some(data) = &self.data else {
            return Ok(());
        };
        let span = {
            let mut data = lock(data);
            if data.settled {
                return Ok(());
            }
            data.settled = true;
            if !data.explicit && error.is_some() {
                data.span.status = SpanStatus::Error { error };
            }
            data.span.ended_ms =
                micros(data.span.started_ms + data.clock.elapsed().as_secs_f64() * 1000.0);
            data.span.clone()
        };
        self.context.recorder.ingest(TelemetryBatch {
            spans: vec![span],
            metrics: Vec::new(),
        })?;
        Ok(())
    }
}
fn copy_attributes(attributes: SpanAttributes) -> SpanAttributes {
    attributes
        .into_iter()
        .filter(|(_, v)| !matches!(v, AttributeValue::Undefined))
        .collect()
}
fn micros(ms: f64) -> f64 {
    (ms * 1000.0).round() / 1000.0
}
