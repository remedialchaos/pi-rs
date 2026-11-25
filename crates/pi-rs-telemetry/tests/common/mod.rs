use pi_rs_telemetry::*;
use std::sync::{Arc, Mutex};
pub type TestResult = Result<(), Box<dyn std::error::Error>>;
pub fn attributes(value: serde_json::Value) -> Result<SpanAttributes, serde_json::Error> {
    serde_json::from_value(value)
}
#[derive(Default)]
pub struct FakeClock {
    values: Mutex<(f64, f64)>,
}
impl FakeClock {
    pub fn at(wall: f64, monotonic: f64) -> Self {
        Self {
            values: Mutex::new((wall, monotonic)),
        }
    }
    pub fn set(&self, wall: f64, monotonic: f64) {
        *self
            .values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (wall, monotonic);
    }
}
impl Clock for FakeClock {
    fn wall_ms(&self) -> f64 {
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
    }
    fn monotonic_ms(&self) -> f64 {
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }
}
pub fn timed(wall: f64) -> (Context, Arc<FakeClock>) {
    let source = Arc::new(FakeClock::at(wall, 100.0));
    (
        Context::with_clock(Arc::new(TelemetryClock::new(source.clone()))),
        source,
    )
}
pub fn saved(context: &Context, name: &str) -> Result<Context, ErrorDetails> {
    futures::executor::block_on(context.start_span(SpanOptions::new(name), |span| {
        std::future::ready(Ok::<_, ErrorDetails>(span))
    }))
}
