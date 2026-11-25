//! Compile-time schema vocabulary for Rust callers.
//!
//! Rust cannot infer types from runtime JSON values. A schema marker declares its exact attribute
//! structs and event enum. This is the Rust counterpart of upstream's TypeScript literal inference.
//! The dynamic helpers remain available for data-driven callers and perform no runtime validation.
use crate::*;
use std::{any::Any, future::Future, marker::PhantomData};

/// Implement with distinct attribute structs and an event enum to enforce the schema at compile time.
pub trait TelemetrySpanSchema {
    const NAME: &'static str;
    type StartAttributes: Into<SpanAttributes>;
    type EndAttributes: Into<SpanAttributes>;
    type Event: Into<(String, SpanAttributes)>;
}
pub struct SchemaTelemetrySpan<S: TelemetrySpanSchema> {
    context: Context,
    schema: PhantomData<S>,
}
impl<S: TelemetrySpanSchema> SchemaTelemetrySpan<S> {
    pub fn context(&self) -> &Context {
        &self.context
    }
    pub fn set_attributes(&self, attributes: S::EndAttributes) {
        self.context.set_attributes(attributes.into());
    }
    pub fn add_event(&self, event: S::Event) {
        let (name, attributes) = event.into();
        self.context.add_event(name, attributes);
    }
    pub fn set_status(&self, status: SpanStatus) {
        self.context.set_status(status);
    }
    pub fn add_link(&self, link: SpanLink) {
        self.context.add_link(link);
    }
}
impl TypedSpanStarter {
    /// The schema selects the name and exact start, end, and event attribute types.
    ///
    /// ```compile_fail
    /// use pi_rs_telemetry::*;
    /// struct Operation;
    /// struct RequiredStart { kind: String }
    /// impl From<RequiredStart> for SpanAttributes {
    ///     fn from(start: RequiredStart) -> Self { [("kind".into(), start.kind.into())].into_iter().collect() }
    /// }
    /// impl TelemetrySpanSchema for Operation {
    ///     const NAME: &'static str = "operation";
    ///     type StartAttributes = RequiredStart;
    ///     type EndAttributes = SpanAttributes;
    ///     type Event = (String, SpanAttributes);
    /// }
    /// let starter = create_typed_span_starter(Context::new(), &[]);
    /// // A map cannot replace the declared RequiredStart type.
    /// let future = starter.start_schema::<Operation, _, (), _, _>(SpanAttributes::new(), |_, _| std::future::ready(Ok(())));
    /// ```
    pub fn start_schema<S, T, E: Any, F, Fut>(
        &self,
        attributes: S::StartAttributes,
        callback: F,
    ) -> SpanFuture<Fut>
    where
        S: TelemetrySpanSchema,
        F: FnOnce(SchemaTelemetrySpan<S>, TypedSpanStarter) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.context.start_span(
            SpanOptions {
                name: S::NAME.into(),
                attributes: attributes.into(),
                links: vec![],
            },
            |context| {
                let child = TypedSpanStarter {
                    context: context.clone(),
                };
                callback(
                    SchemaTelemetrySpan {
                        context,
                        schema: PhantomData,
                    },
                    child,
                )
            },
        )
    }
}

pub trait TelemetryMetricSchema {
    const NAME: &'static str;
    type Attributes: Into<SpanAttributes>;
}
pub trait CounterSchema: TelemetryMetricSchema {}
pub trait UpDownCounterSchema: TelemetryMetricSchema {}
pub trait HistogramSchema: TelemetryMetricSchema {}

pub struct SchemaTelemetryCounter<S: CounterSchema> {
    instrument: Counter,
    schema: PhantomData<S>,
}
pub struct SchemaTelemetryUpDownCounter<S: UpDownCounterSchema> {
    instrument: UpDownCounter,
    schema: PhantomData<S>,
}
pub struct SchemaTelemetryHistogram<S: HistogramSchema> {
    instrument: Histogram,
    schema: PhantomData<S>,
}
impl<S: CounterSchema> SchemaTelemetryCounter<S> {
    pub fn add(&self, value: f64, attributes: S::Attributes) {
        self.instrument.add(value, attributes.into());
    }
}
impl<S: UpDownCounterSchema> SchemaTelemetryUpDownCounter<S> {
    pub fn add(&self, value: f64, attributes: S::Attributes) {
        self.instrument.add(value, attributes.into());
    }
}
impl<S: HistogramSchema> SchemaTelemetryHistogram<S> {
    pub fn record(&self, value: f64, attributes: S::Attributes, context: Option<&Context>) {
        self.instrument.record(value, attributes.into(), context);
    }
}
impl TypedTelemetryMetrics {
    /// Metric kinds are distinct marker bounds, so histograms cannot be used as counters.
    ///
    /// ```compile_fail
    /// use pi_rs_telemetry::*;
    /// struct Latency;
    /// impl TelemetryMetricSchema for Latency { const NAME: &'static str = "latency"; type Attributes = SpanAttributes; }
    /// impl HistogramSchema for Latency {}
    /// let metrics = create_typed_metrics(Context::new().metrics(), &[]);
    /// let counter = metrics.counter_schema::<Latency>();
    /// ```
    pub fn counter_schema<S: CounterSchema>(&self) -> SchemaTelemetryCounter<S> {
        SchemaTelemetryCounter {
            instrument: self.counter(S::NAME),
            schema: PhantomData,
        }
    }
    pub fn up_down_counter_schema<S: UpDownCounterSchema>(
        &self,
    ) -> SchemaTelemetryUpDownCounter<S> {
        SchemaTelemetryUpDownCounter {
            instrument: self.up_down_counter(S::NAME),
            schema: PhantomData,
        }
    }
    pub fn histogram_schema<S: HistogramSchema>(&self) -> SchemaTelemetryHistogram<S> {
        SchemaTelemetryHistogram {
            instrument: self.histogram(S::NAME),
            schema: PhantomData,
        }
    }
}
