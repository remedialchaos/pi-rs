use super::Recorder;
use crate::{
    Error, Result,
    context::lock,
    sqlite::SqliteDatabase,
    sync::SyncFuture,
    telemetry_ingest::{PAGE_BATCH_SPANS, TelemetryBatch, WORKER_BATCH_ROWS},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, watch};
/// None means currently unbound. A failed accepted send is best effort, not retried with duplicate spans.
pub trait TelemetrySink: Send + Sync {
    fn send(&self, batch: &TelemetryBatch) -> Option<SyncFuture<()>>;
    fn release(&self);
}
#[derive(Clone, Copy)]
pub enum SendKind {
    Page,
    Worker,
}
#[derive(Clone)]
pub struct TelemetrySender(Arc<SenderInner>);
struct Reservation(Recorder);
impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct SenderInner {
    recorder: Recorder,
    sink: Arc<dyn TelemetrySink>,
    kind: SendKind,
    serial: AsyncMutex<()>,
    stopped: AtomicBool,
    timer_started: AtomicBool,
    stop: watch::Sender<bool>,
}
impl TelemetrySender {
    pub fn new(recorder: Recorder, sink: Arc<dyn TelemetrySink>, kind: SendKind) -> Self {
        let (stop, _) = watch::channel(false);
        Self(Arc::new(SenderInner {
            recorder,
            sink,
            kind,
            serial: AsyncMutex::new(()),
            stopped: AtomicBool::new(false),
            timer_started: AtomicBool::new(false),
            stop,
        }))
    }
    pub fn start_timer(&self, interval: Duration) -> Result<()> {
        if interval.is_zero() {
            return Err(Error::Invalid("telemetry: zero send interval".into()));
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|e| Error::Invalid(e.to_string()))?;
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if self.0.timer_started.swap(true, Ordering::AcqRel) {
            return Err(Error::Invalid("telemetry: timer already started".into()));
        }
        let weak = Arc::downgrade(&self.0);
        let mut stop = self.0.stop.subscribe();
        runtime.spawn(async move {
            loop {
                if *stop.borrow() { return; }
                tokio::select! {biased;_=stop.changed()=>return,_=tokio::time::sleep(interval)=>{let Some(inner)=weak.upgrade()else{return;};let sender=Self(inner);let _=sender.send(false).await;}}
            }
        });
        Ok(())
    }
    pub async fn send(&self, partial: bool) -> Result<()> {
        let _serial = self.0.serial.lock().await;
        if self.0.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let page = matches!(self.0.kind, SendKind::Page);
        if if page {
            self.0.recorder.queued_spans() == 0
        } else {
            !self.0.recorder.pending(partial)
        } {
            return Ok(());
        }
        let batch = self.0.recorder.take(
            partial,
            if page {
                PAGE_BATCH_SPANS
            } else {
                WORKER_BATCH_ROWS
            },
        );
        let mut sent = batch.clone();
        if page {
            sent.metrics.clear();
        } else {
            sent.metrics.truncate(WORKER_BATCH_ROWS);
        }
        match self.0.sink.send(&sent) {
            None => {
                self.0.recorder.release();
                self.0.recorder.ingest(batch)?;
            }
            Some(sending) => {
                let _reservation = Reservation(self.0.recorder.clone());
                let _ = sending.await;
            }
        }
        Ok(())
    }
    pub fn stop(&self) {
        if !self.0.stopped.swap(true, Ordering::AcqRel) {
            self.0.stop.send_replace(true);
            self.0.sink.release();
        }
    }
}
impl Drop for SenderInner {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.stop.send_replace(true);
            self.sink.release();
        }
    }
}
#[derive(Clone)]
pub struct SqliteTelemetry(Arc<Store>);
struct Store {
    db: SqliteDatabase,
    recorder: Recorder,
    retention: f64,
    serial: AsyncMutex<()>,
    failure: Mutex<Option<String>>,
    closing: AtomicBool,
    closed: AtomicBool,
    stop: watch::Sender<bool>,
}
impl SqliteTelemetry {
    pub async fn open(db: SqliteDatabase, retention_ms: f64, capacity: usize) -> Result<Self> {
        if !retention_ms.is_finite() || retention_ms < 0.0 {
            return Err(Error::Invalid("telemetry: invalid retention".into()));
        }
        crate::telemetry_sqlite::migrate(&db).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let recorder = Recorder::new(move || id.clone(), capacity);
        let (stop, _) = watch::channel(false);
        let store = Self(Arc::new(Store {
            db,
            recorder,
            retention: retention_ms,
            serial: AsyncMutex::new(()),
            failure: Mutex::new(None),
            closing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            stop,
        }));
        let weak = Arc::downgrade(&store.0);
        let mut stop = store.0.stop.subscribe();
        tokio::spawn(async move {
            loop {
                if *stop.borrow() {
                    return;
                }
                tokio::select! {biased;_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_secs(1))=>{let Some(inner)=weak.upgrade()else{return;};let store=Self(inner);if let Ok(guard)=store.0.serial.try_lock(){drop(guard);let _=store.flush().await;}}}
            }
        });
        Ok(store)
    }
    pub fn recorder(&self) -> &Recorder {
        &self.0.recorder
    }
    pub fn database(&self) -> &SqliteDatabase {
        &self.0.db
    }
    /// Returns a `pi-rs-telemetry` `Context` wired directly to this SQLite
    /// telemetry store through the additive `TelemetryRecorderSink` bridge.
    pub fn telemetry_context(&self) -> pi_rs_telemetry::Context {
        pi_rs_telemetry::Context::with_sink(Arc::new(self.clone()))
    }
    pub fn ingest(&self, batch: TelemetryBatch) -> Result<usize> {
        self.0.recorder.ingest(batch)
    }
    pub async fn flush(&self) -> Result<()> {
        self.drain(false).await
    }
    pub async fn close(&self) -> Result<()> {
        self.0.closing.store(true, Ordering::Release);
        self.0.recorder.stop(false);
        self.0.stop.send_replace(true);
        self.drain(true).await
    }
    async fn drain(&self, partial: bool) -> Result<()> {
        let store = self.clone();
        // Once started, the flush owns both its reservation and finalization even
        // if its caller is cancelled while SQLite is queued behind another job.
        tokio::spawn(async move { store.drain_owned(partial).await })
            .await
            .map_err(|e| Error::Invalid(format!("telemetry flush task: {e}")))?
    }
    async fn drain_owned(&self, partial: bool) -> Result<()> {
        let _serial = self.0.serial.lock().await;
        if let Some(failure) = lock(&self.0.failure).clone() {
            return Err(Error::Invalid(failure));
        }
        if self.0.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let partial = partial || self.0.closing.load(Ordering::Acquire);
        let batch = self.0.recorder.take(partial, usize::MAX);
        let result = crate::telemetry_sqlite::write_batch(
            &self.0.db,
            batch,
            self.0.recorder.now() - self.0.retention,
        )
        .await;
        self.0.recorder.release();
        if let Err(error) = result {
            *lock(&self.0.failure) = Some(error.to_string());
            self.0.recorder.stop(true);
            self.0.stop.send_replace(true);
            return Err(error);
        }
        if partial {
            self.0.closed.store(true, Ordering::Release);
        }
        Ok(())
    }
}
impl pi_rs_telemetry::TelemetryRecorderSink for SqliteTelemetry {
    fn record_span(&self, span: pi_rs_telemetry::StoredSpan) {
        let _ = self.ingest(TelemetryBatch {
            spans: vec![span],
            metrics: Vec::new(),
        });
    }
    fn record_measurement(
        &self,
        interval_ms: f64,
        measurement: pi_rs_telemetry::RecordedTelemetryMeasurement,
    ) {
        if !measurement.value.is_finite() || !interval_ms.is_finite() {
            return;
        }
        let metric = pi_rs_telemetry::StoredMetric {
            interval_ms,
            name: measurement.name,
            kind: measurement.kind,
            unit: measurement.unit,
            attributes: crate::telemetry_ingest::sorted_attributes(&measurement.attributes),
            count: 1,
            sum: measurement.value,
            min: measurement.value,
            max: measurement.value,
        };
        let _ = self.ingest(TelemetryBatch {
            spans: Vec::new(),
            metrics: vec![metric],
        });
    }
}
impl pi_rs_telemetry::FallibleTelemetryStore for SqliteTelemetry {
    type Error = Error;
    async fn find_traces_fallible(
        &self,
        query: pi_rs_telemetry::TraceQuery,
    ) -> Result<pi_rs_telemetry::TracePage> {
        crate::telemetry_sqlite::find_traces(&self.0.db, query).await
    }
    async fn get_trace_fallible(
        &self,
        trace_id: &str,
    ) -> Result<Option<pi_rs_telemetry::StoredTrace>> {
        crate::telemetry_sqlite::get_trace(&self.0.db, trace_id.to_owned()).await
    }
    async fn find_metrics_fallible(
        &self,
        query: pi_rs_telemetry::MetricQuery,
    ) -> Result<Vec<pi_rs_telemetry::StoredMetric>> {
        crate::telemetry_sqlite::find_metrics(&self.0.db, query).await
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        self.recorder.stop(true);
    }
}
