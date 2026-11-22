//! Buffered telemetry ingestion and page trust-boundary filtering, using public telemetry value types.
use crate::{Result, data::sorted_copy, json::stringify};
use indexmap::IndexMap;
use pi_rs_telemetry::{AttributeValue, SpanAttributes, StoredMetric, StoredSpan};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
pub const INTERVAL_MS: f64 = 10_000.0;
pub const PAGE_SPANS_PER_MINUTE: usize = 1200;
pub const PAGE_BATCH_SPANS: usize = 100;
pub const WORKER_BATCH_ROWS: usize = 1000;
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetryBatch {
    pub spans: Vec<StoredSpan>,
    pub metrics: Vec<StoredMetric>,
}
pub fn sorted_attributes(attributes: &SpanAttributes) -> SpanAttributes {
    let mut pairs: Vec<_> = attributes
        .iter()
        .filter(|(_, v)| !matches!(v, AttributeValue::Undefined))
        .collect();
    pairs.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
    pairs
        .into_iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
pub fn series_key(metric: &StoredMetric) -> Result<String> {
    let attrs = serde_json::to_value(sorted_attributes(&metric.attributes))?;
    stringify(&json!([
        metric.name,
        metric.kind,
        metric.unit,
        stringify(&sorted_copy(&attrs))?
    ]))
}
/// Capacity includes the last taken batch until release. The owner serializes take/release.
pub struct TelemetryRecorder {
    capacity: usize,
    spans: VecDeque<StoredSpan>,
    metrics: IndexMap<String, StoredMetric>,
    in_flight: usize,
    in_flight_metrics: usize,
    stopped: bool,
    pub dropped_spans: u64,
    pub dropped_measurements: u64,
}
impl TelemetryRecorder {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            spans: VecDeque::new(),
            metrics: IndexMap::new(),
            in_flight: 0,
            in_flight_metrics: 0,
            stopped: false,
            dropped_spans: 0,
            dropped_measurements: 0,
        }
    }
    pub fn queued_spans(&self) -> usize {
        self.spans.len()
    }
    pub fn stopped(&self) -> bool {
        self.stopped
    }
    pub fn ingest(&mut self, batch: TelemetryBatch) -> Result<usize> {
        if self.stopped {
            return Ok(0);
        }
        let mut queued = 0;
        for span in batch.spans {
            if self.spans.len() + self.in_flight >= self.capacity {
                self.dropped_spans += 1;
            } else {
                self.spans.push_back(span);
                queued += 1;
            }
        }
        for metric in batch.metrics {
            self.merge(metric)?;
        }
        Ok(queued)
    }
    fn merge(&mut self, mut metric: StoredMetric) -> Result<()> {
        let key = format!(
            "{}:{}",
            crate::json::number_to_string(metric.interval_ms),
            series_key(&metric)?
        );
        if let Some(previous) = self.metrics.get(&key) {
            metric.count += previous.count;
            metric.sum += previous.sum;
            metric.min = metric.min.min(previous.min);
            metric.max = metric.max.max(previous.max);
        } else if self.metrics.len() + self.in_flight_metrics >= self.capacity {
            self.dropped_measurements += 1;
            return Ok(());
        }
        self.metrics.insert(key, metric);
        Ok(())
    }
    pub fn pending(&self, partial: bool, now_ms: f64) -> bool {
        !self.spans.is_empty()
            || self
                .metrics
                .values()
                .any(|m| partial || m.interval_ms < (now_ms / INTERVAL_MS).floor() * INTERVAL_MS)
    }
    pub fn take(&mut self, partial: bool, limit: usize, now_ms: f64) -> TelemetryBatch {
        let spans: Vec<_> = self.spans.drain(..limit.min(self.spans.len())).collect();
        self.in_flight = spans.len();
        let current = (now_ms / INTERVAL_MS).floor() * INTERVAL_MS;
        let mut metrics = Vec::new();
        self.metrics.retain(|_, metric| {
            if partial || metric.interval_ms < current {
                metrics.push(metric.clone());
                false
            } else {
                true
            }
        });
        self.in_flight_metrics = metrics.len();
        TelemetryBatch { spans, metrics }
    }
    pub fn release(&mut self) {
        self.in_flight = 0;
        self.in_flight_metrics = 0;
    }
    pub fn stop(&mut self, discard: bool) {
        self.stopped = true;
        if discard {
            self.spans.clear();
            self.metrics.clear();
        }
    }
}
#[derive(Default)]
pub struct PageBudgets {
    pages: IndexMap<String, (f64, usize)>,
}
impl PageBudgets {
    /// The application must admit the principal and validate PageBatch before invoking this filter.
    pub fn filter(
        &mut self,
        incarnation: &str,
        now_ms: f64,
        spans: Vec<StoredSpan>,
        mut principal_attributes: impl FnMut() -> SpanAttributes,
    ) -> Vec<StoredSpan> {
        if self
            .pages
            .get(incarnation)
            .is_none_or(|(start, _)| now_ms - start >= 60_000.0)
        {
            self.pages.shift_remove(incarnation);
            if self.pages.len() >= 1000 {
                self.pages.shift_remove_index(0);
            }
            self.pages.insert(incarnation.into(), (now_ms, 0));
        }
        let Some((_, count)) = self.pages.get_mut(incarnation) else {
            return Vec::new();
        };
        let prefix = format!("browser:{incarnation}:");
        let mut accepted = Vec::new();
        for mut span in spans {
            if *count >= PAGE_SPANS_PER_MINUTE {
                break;
            }
            if !span.id.starts_with(&prefix)
                || !span.trace_id.starts_with(&prefix)
                || span
                    .parent_id
                    .as_ref()
                    .is_some_and(|p| !p.starts_with(&prefix))
                || span.ended_ms < span.started_ms
            {
                continue;
            }
            *count += 1;
            span.attributes.retain(|k, _| !k.starts_with("chord."));
            let mut attrs = SpanAttributes::new();
            attrs.insert(
                "chord.host".into(),
                AttributeValue::String("browser".into()),
            );
            attrs.extend(span.attributes);
            attrs.extend(principal_attributes());
            span.attributes = attrs;
            accepted.push(span);
        }
        accepted
    }
}
