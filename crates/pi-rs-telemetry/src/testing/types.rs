use crate::{Context, RecordedTelemetryMeasurement, RecordedTelemetrySpan};
use std::future::Future;

/// A fresh adapter and detached normalized readers owned by one conformance case.
pub trait TelemetryAdapterFixture {
    fn context(&self) -> Context;
    fn get_spans(&self) -> impl Future<Output = Vec<RecordedTelemetrySpan>>;
    fn get_metrics(&self) -> impl Future<Output = Vec<RecordedTelemetryMeasurement>>;
    fn dispose(self) -> impl Future<Output = ()>;
}
impl TelemetryAdapterFixture for Context {
    fn context(&self) -> Context {
        self.clone()
    }
    async fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        Context::get_spans(self)
    }
    async fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement> {
        Context::get_metrics(self)
    }
    async fn dispose(self) {}
}
#[derive(Clone, Copy, Debug)]
pub struct TelemetryAdapterConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    pub(crate) kind: usize,
}
#[derive(Debug)]
pub struct ConformanceError(pub String);
impl std::fmt::Display for ConformanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConformanceError {}
