use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

use pi_rs_telemetry::{
    Context, ErrorDetails, SpanAttributes, SpanStatus, TelemetryContext, TelemetryCounter,
    TelemetryHistogram, TelemetrySchemaDefinition, TelemetrySpan, create_typed_metrics,
    create_typed_span_starter, define_telemetry_schema,
};

use crate::model::{Model, ThinkingLevel};
use crate::types::{AssistantMessage, AssistantMessageEvent, StopReason};

// INFALLIBLE: the embedded telemetry schema is generated from the upstream schema export.
pub static AI_TELEMETRY_SCHEMA: LazyLock<TelemetrySchemaDefinition> = LazyLock::new(|| {
    define_telemetry_schema(
        serde_json::from_str(include_str!("../assets/telemetry.json"))
            .expect("valid embedded AI telemetry schema"),
    )
});

pub(crate) struct RequestTelemetry {
    span: Context,
    base: SpanAttributes,
    started: Instant,
    first: bool,
    tool_chars: HashMap<usize, usize>,
}
impl RequestTelemetry {
    pub(crate) async fn observe<T, F, Fut>(
        parent: Context,
        model: Model,
        reasoning: Option<ThinkingLevel>,
        poll: bool,
        work: F,
    ) -> T
    where
        F: FnOnce(RequestTelemetry, Model) -> Fut,
        Fut: Future<Output = T>,
    {
        let base = SpanAttributes::from([
            ("provider".into(), model.provider.clone().into()),
            ("model".into(), model.id.clone().into()),
        ]);
        let mut start = base.clone();
        start.insert("api".into(), model.api.as_str().into());
        if let Some(r) = reasoning {
            start.insert("reasoning".into(), r.as_str().into());
        }
        let started = Instant::now();
        let span = if poll { "pi.ai.poll" } else { "pi.ai.request" };
        let controller =
            create_typed_span_starter(parent, std::slice::from_ref(&AI_TELEMETRY_SCHEMA));
        match controller
            .start(span, start, |span, _| async move {
                Ok::<_, std::convert::Infallible>(
                    work(
                        Self {
                            span,
                            base,
                            started,
                            first: true,
                            tool_chars: HashMap::new(),
                        },
                        model,
                    )
                    .await,
                )
            })
            .await
        {
            Ok(result) => result,
            Err(impossible) => match impossible {},
        }
    }
    pub(crate) fn event(&mut self, event: &AssistantMessageEvent) {
        if std::mem::take(&mut self.first) {
            self.span
                .add_event("pi.ai.first_chunk", SpanAttributes::new());
            let metrics = create_typed_metrics(
                self.span.metrics(),
                std::slice::from_ref(&AI_TELEMETRY_SCHEMA),
            );
            metrics.histogram("pi.ai.first_chunk.duration").record(
                self.started.elapsed().as_secs_f64() * 1000.0,
                self.base.clone(),
                Some(&self.span),
            );
        }
        let (kind, index, chars, tool) = match event {
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                *self.tool_chars.entry(*content_index).or_default() += delta.encode_utf16().count();
                return;
            }
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => ("text", *content_index, content.encode_utf16().count(), None),
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => (
                "thinking",
                *content_index,
                content.encode_utf16().count(),
                None,
            ),
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => (
                "tool_call",
                *content_index,
                self.tool_chars.remove(content_index).unwrap_or_default(),
                Some(tool_call.name.as_str()),
            ),
            _ => return,
        };
        let mut attr = SpanAttributes::from([
            ("type".into(), kind.into()),
            ("index".into(), (index as f64).into()),
            ("chars".into(), (chars as f64).into()),
        ]);
        if let Some(tool) = tool {
            attr.insert("tool".into(), tool.into());
        }
        self.span.add_event("pi.ai.block", attr);
    }
    pub(crate) fn result(self, message: &AssistantMessage, poll: bool) {
        let usage = &message.usage;
        let mut attrs = SpanAttributes::from([
            ("stop".into(), message.stop_reason.as_str().into()),
            ("usage.input".into(), usage.input.into()),
            ("usage.output".into(), usage.output.into()),
            ("usage.cache_read".into(), usage.cache_read.into()),
            ("usage.cache_write".into(), usage.cache_write.into()),
            ("cost.total".into(), usage.cost.total.into()),
        ]);
        if let Some(id) = &message.response_id {
            attrs.insert("response.id".into(), id.clone().into());
        }
        self.span.set_attributes(attrs);
        if message.stop_reason == StopReason::Error {
            let message = crate::utils::sanitize_surrogates(
                message
                    .error_message
                    .as_deref()
                    .unwrap_or_default()
                    .encode_utf16()
                    .take(500),
            );
            self.span.set_status(SpanStatus::Error {
                error: Some(ErrorDetails {
                    name: "provider_error".into(),
                    message,
                }),
            });
        }
        let metrics = create_typed_metrics(
            self.span.metrics(),
            std::slice::from_ref(&AI_TELEMETRY_SCHEMA),
        );
        let mut end = self.base.clone();
        end.insert("stop".into(), message.stop_reason.as_str().into());
        metrics.histogram("pi.ai.request.duration").record(
            self.started.elapsed().as_secs_f64() * 1000.0,
            end,
            Some(&self.span),
        );
        if !poll
            || !matches!(
                message.stop_reason,
                StopReason::Pending | StopReason::Deferred
            )
        {
            let tokens = metrics.counter("pi.ai.tokens");
            for (kind, n) in [
                ("input", usage.input),
                ("output", usage.output),
                ("cache_read", usage.cache_read),
                ("cache_write", usage.cache_write),
            ] {
                let mut attr = self.base.clone();
                attr.insert("kind".into(), kind.into());
                tokens.add(n, attr);
            }
            metrics
                .counter("pi.ai.cost")
                .add(usage.cost.total, self.base);
        }
    }
}
