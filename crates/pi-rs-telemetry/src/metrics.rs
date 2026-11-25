//! Synchronous instruments and schema-bound metadata factories.
use crate::{
    memory::{State, record},
    *,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    #[serde(rename = "counter")]
    Counter,
    #[serde(rename = "upDownCounter")]
    UpDownCounter,
    #[serde(rename = "histogram")]
    Histogram,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InstrumentOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HistogramOptions {
    #[serde(flatten)]
    pub instrument: InstrumentOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<Vec<f64>>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryMeasurement {
    pub kind: MetricKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<Vec<f64>>,
    pub value: f64,
    pub attributes: SpanAttributes,
    pub context: Option<TelemetryCarrier>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMetric {
    pub interval_ms: f64,
    pub name: String,
    pub kind: MetricKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub attributes: SpanAttributes,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}
#[derive(Clone)]
pub struct Metrics {
    pub(crate) state: Option<Arc<Mutex<State>>>,
}
impl Metrics {
    pub const fn noop() -> Self {
        Self { state: None }
    }
    pub fn same_instance(&self, other: &Self) -> bool {
        match (&self.state, &other.state) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
    fn instrument(&self, kind: MetricKind, name: &str, options: HistogramOptions) -> Instrument {
        Instrument {
            recorder: self.state.as_ref().map(|state| {
                Arc::new(Recorder {
                    state: state.clone(),
                    metadata: RecordedTelemetryMeasurement {
                        kind,
                        name: name.into(),
                        description: options.instrument.description,
                        unit: options.instrument.unit,
                        boundaries: if kind == MetricKind::Histogram {
                            options.boundaries
                        } else {
                            None
                        },
                        value: 0.0,
                        attributes: SpanAttributes::new(),
                        context: None,
                    },
                })
            }),
        }
    }
}
struct Recorder {
    state: Arc<Mutex<State>>,
    metadata: RecordedTelemetryMeasurement,
}
#[derive(Clone)]
struct Instrument {
    recorder: Option<Arc<Recorder>>,
}
impl Instrument {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<&Context>) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let mut measurement = recorder.metadata.clone();
        measurement.value = value;
        measurement.attributes = copy_attributes(attributes);
        measurement.context = context.and_then(|c| c.propagation().inject(c));
        record(&recorder.state, measurement);
    }
    fn same_instance(&self, other: &Self) -> bool {
        match (&self.recorder, &other.recorder) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}
#[derive(Clone)]
pub struct Counter(Instrument);
#[derive(Clone)]
pub struct UpDownCounter(Instrument);
#[derive(Clone)]
pub struct Histogram(Instrument);
pub trait TelemetryCounter {
    fn add(&self, value: f64, attributes: SpanAttributes);
}
pub trait TelemetryUpDownCounter {
    fn add(&self, value: f64, attributes: SpanAttributes);
}
pub trait TelemetryHistogram {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<&Context>);
}
impl TelemetryCounter for Counter {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.0.record(value, attributes, None);
    }
}
impl TelemetryUpDownCounter for UpDownCounter {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.0.record(value, attributes, None);
    }
}
impl TelemetryHistogram for Histogram {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<&Context>) {
        self.0.record(value, attributes, context);
    }
}
impl Counter {
    pub fn same_instance(&self, other: &Self) -> bool {
        self.0.same_instance(&other.0)
    }
}
impl UpDownCounter {
    pub fn same_instance(&self, other: &Self) -> bool {
        self.0.same_instance(&other.0)
    }
}
impl Histogram {
    pub fn same_instance(&self, other: &Self) -> bool {
        self.0.same_instance(&other.0)
    }
}
pub trait TelemetryMetrics {
    fn counter(&self, name: &str, options: InstrumentOptions) -> Counter;
    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> UpDownCounter;
    fn histogram(&self, name: &str, options: HistogramOptions) -> Histogram;
}
impl TelemetryMetrics for Metrics {
    fn counter(&self, name: &str, options: InstrumentOptions) -> Counter {
        Counter(self.instrument(
            MetricKind::Counter,
            name,
            HistogramOptions {
                instrument: options,
                boundaries: None,
            },
        ))
    }
    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> UpDownCounter {
        UpDownCounter(self.instrument(
            MetricKind::UpDownCounter,
            name,
            HistogramOptions {
                instrument: options,
                boundaries: None,
            },
        ))
    }
    fn histogram(&self, name: &str, options: HistogramOptions) -> Histogram {
        Histogram(self.instrument(MetricKind::Histogram, name, options))
    }
}
impl TelemetryMetrics for Context {
    fn counter(&self, name: &str, options: InstrumentOptions) -> Counter {
        self.metrics().counter(name, options)
    }
    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> UpDownCounter {
        self.metrics().up_down_counter(name, options)
    }
    fn histogram(&self, name: &str, options: HistogramOptions) -> Histogram {
        self.metrics().histogram(name, options)
    }
}
#[derive(Clone)]
pub struct TypedTelemetryMetrics {
    metrics: Metrics,
    definitions: IndexMap<String, HistogramOptions>,
}
pub fn create_typed_metrics(
    metrics: Metrics,
    schemas: &[TelemetrySchemaDefinition],
) -> TypedTelemetryMetrics {
    let mut definitions = IndexMap::new();
    for schema in schemas {
        if let Some(map) = schema.get("metrics").and_then(serde_json::Value::as_object) {
            for (name, definition) in map {
                let instrument = InstrumentOptions {
                    description: definition["description"].as_str().map(str::to_string),
                    unit: definition["unit"].as_str().map(str::to_string),
                };
                let boundaries = definition["boundaries"]
                    .as_array()
                    .map(|items| items.iter().filter_map(serde_json::Value::as_f64).collect());
                definitions.insert(
                    name.clone(),
                    HistogramOptions {
                        instrument,
                        boundaries,
                    },
                );
            }
        }
    }
    TypedTelemetryMetrics {
        metrics,
        definitions,
    }
}
impl TypedTelemetryMetrics {
    pub fn counter(&self, name: &str) -> Counter {
        self.metrics.counter(
            name,
            self.definitions
                .get(name)
                .cloned()
                .unwrap_or_default()
                .instrument,
        )
    }
    pub fn up_down_counter(&self, name: &str) -> UpDownCounter {
        self.metrics.up_down_counter(
            name,
            self.definitions
                .get(name)
                .cloned()
                .unwrap_or_default()
                .instrument,
        )
    }
    pub fn histogram(&self, name: &str) -> Histogram {
        self.metrics.histogram(
            name,
            self.definitions.get(name).cloned().unwrap_or_default(),
        )
    }
}
