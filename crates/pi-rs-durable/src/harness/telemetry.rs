use crate::{TaskId, TaskOutcome, TaskRecord, TaskState};
use futures_util::FutureExt;
use pi_rs_telemetry::{
    Context, Counter, ErrorDetails, Histogram, SpanAttributes, SpanFuture, SpanLink, SpanOptions,
    SpanStatus, TelemetryContext, TelemetryCounter, TelemetryHistogram, TelemetrySpan,
    TelemetryUpDownCounter, UpDownCounter, create_typed_metrics,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    convert::Infallible,
    future::Ready,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{LazyLock, Mutex},
    time::Instant,
};

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    // INFALLIBLE: the bundled schema is the exported upstream JSON.
    serde_json::from_str(super::DURABLE_TELEMETRY_SCHEMA).expect("valid durable telemetry schema")
});

pub(crate) struct HarnessTelemetry {
    pub(super) root: Context,
    enabled: bool,
    reports: Counter,
    duration: Histogram,
    active: UpDownCounter,
    live: Mutex<HashMap<TaskId, (String, bool)>>,
}

impl HarnessTelemetry {
    pub(super) fn new(context: Context) -> Self {
        let root = context.extract(None);
        let metrics = create_typed_metrics(root.metrics(), std::slice::from_ref(&SCHEMA));
        Self {
            enabled: !root
                .metrics()
                .same_instance(&pi_rs_telemetry::Metrics::noop()),
            root,
            reports: metrics.counter("pi.durable.reports"),
            duration: metrics.histogram("pi.durable.phase.duration"),
            active: metrics.up_down_counter("pi.durable.tasks.active"),
            live: Mutex::default(),
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn track(&self, record: &TaskRecord, loaded: bool) {
        if !self.enabled {
            return;
        }
        let change = {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(record.state, TaskState::Terminal { .. }) {
                live.remove(&record.id).map(|(kind, _)| (kind, -1.0))
            } else if let std::collections::hash_map::Entry::Vacant(entry) = live.entry(record.id) {
                entry.insert((record.kind.clone(), loaded));
                Some((record.kind.clone(), 1.0))
            } else {
                None
            }
        };
        if let Some((kind, delta)) = change {
            self.count_active(kind, delta);
        }
    }

    pub(crate) fn close(&self) {
        let live = std::mem::take(&mut *self.live.lock().unwrap_or_else(|e| e.into_inner()));
        for (_, (kind, _)) in live {
            self.count_active(kind, -1.0);
        }
    }

    fn count_active(&self, kind: String, delta: f64) {
        if catch_unwind(AssertUnwindSafe(|| {
            self.active
                .add(delta, [("task".into(), kind.into())].into())
        }))
        .is_err()
        {
            self.report("host_error", None, None);
        }
    }

    pub(crate) fn report(&self, kind: &str, task: Option<TaskId>, context: Option<&Context>) {
        let mut attributes = SpanAttributes::from([("type".into(), kind.into())]);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            self.reports.add(1.0, attributes.clone());
            if let Some(context) = context {
                if let Some(id) = task {
                    attributes.insert("task.id".into(), (id as f64).into());
                    attributes.insert("reason".into(), "threw".into());
                }
                context.add_event("pi.durable.report", attributes);
            }
        }));
    }

    pub(super) fn phase(
        &self,
        record: &TaskRecord,
        checkpoint: &Value,
        phase: &str,
        first: bool,
    ) -> PhaseSpan {
        let resumed = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&record.id)
            .is_some_and(|(_, resumed)| std::mem::take(resumed));
        let mut attributes = SpanAttributes::from([
            ("task.id".into(), (record.id as f64).into()),
            ("task.name".into(), record.kind.clone().into()),
            ("phase".into(), phase.into()),
            (
                "kind".into(),
                if record.abort_requested {
                    "abort"
                } else {
                    "run"
                }
                .into(),
            ),
            ("resumed".into(), resumed.into()),
        ]);
        if let Some(attempt) = checkpoint["attempt"].as_f64() {
            attributes.insert("attempt".into(), attempt.into());
        }
        let links = if first || resumed {
            record.links.as_deref().unwrap_or_default()
        } else {
            &[]
        };
        let span = OpenSpan::new(
            &self.root.extract(record.trace.as_ref()),
            SpanOptions {
                name: "pi.durable.phase".into(),
                attributes,
                links: links
                    .iter()
                    .map(|carrier| link(self.root.extract(Some(carrier))))
                    .collect(),
            },
        );
        PhaseSpan {
            span,
            metric: self.duration.clone(),
            task: record.kind.clone(),
            phase: phase.to_owned(),
            started: Instant::now(),
            settled: false,
        }
    }
}

impl Drop for HarnessTelemetry {
    fn drop(&mut self) {
        self.close();
    }
}

pub(super) fn link(context: Context) -> SpanLink {
    SpanLink {
        context,
        attributes: SpanAttributes::new(),
    }
}

pub(super) struct OpenSpan {
    pub(super) context: Context,
    finish: Option<SpanFuture<Ready<Result<(), Infallible>>>>,
}

impl OpenSpan {
    pub(super) fn new(parent: &Context, options: SpanOptions) -> Self {
        // start_span calls its callback immediately. Poll the ready completion on drop,
        // including when the driver cancels or drops an in-flight invocation.
        let mut context = Context::noop();
        let completion = parent.start_span(options, |span| {
            context = span;
            std::future::ready(Ok(()))
        });
        Self {
            context,
            finish: Some(completion),
        }
    }
}

impl Drop for OpenSpan {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            let _ = catch_unwind(AssertUnwindSafe(|| finish.now_or_never()));
        }
    }
}

pub(super) struct PhaseSpan {
    pub(super) span: OpenSpan,
    metric: Histogram,
    task: String,
    phase: String,
    started: Instant,
    settled: bool,
}

impl PhaseSpan {
    pub(super) fn finish(&mut self, current: &TaskRecord, next: &TaskRecord) {
        let mut error = None;
        let outcome = match &next.state {
            TaskState::Terminal { outcome } | TaskState::Completing { outcome } => match outcome {
                TaskOutcome::Completed { .. } => "done".to_owned(),
                TaskOutcome::Aborted { .. } => "aborted".to_owned(),
                TaskOutcome::Faulted { .. } => {
                    error = Some("task_faulted");
                    "faulted".to_owned()
                }
                TaskOutcome::Failed { .. } => {
                    error = Some("task_failed");
                    "faulted".to_owned()
                }
                TaskOutcome::Orphaned { .. } => {
                    error = Some("task_orphaned");
                    "faulted".to_owned()
                }
            },
            _ if current.abort_requested != next.abort_requested => "aborted".to_owned(),
            TaskState::Waiting { .. } => "waiting".to_owned(),
            TaskState::Sleeping { .. } => "sleeping".to_owned(),
            TaskState::Running { checkpoint, .. } | TaskState::Pending { checkpoint, .. } => {
                format!("next={}", checkpoint["phase"].as_str().unwrap_or_default())
            }
        };
        self.settle(outcome, error);
    }

    fn settle(&mut self, outcome: String, error: Option<&str>) {
        self.settled = true;
        let span = &self.span.context;
        span.set_attributes([("outcome".into(), outcome.clone().into())].into());
        if let Some(name) = error {
            span.set_status(SpanStatus::Error {
                error: Some(ErrorDetails {
                    name: name.into(),
                    message: "Task phase failed".into(),
                }),
            });
        }
        let _ = catch_unwind(AssertUnwindSafe(|| {
            self.metric.record(
                self.started.elapsed().as_secs_f64() * 1000.0,
                [
                    ("task".into(), self.task.clone().into()),
                    ("phase".into(), self.phase.clone().into()),
                    ("outcome".into(), outcome.into()),
                ]
                .into(),
                Some(span),
            );
        }));
    }
}

impl Drop for PhaseSpan {
    fn drop(&mut self) {
        if !self.settled {
            self.settle("aborted".into(), None);
        }
    }
}

pub(super) fn active_context(context: &pi_rs_chord::context::Context) -> Option<Context> {
    let telemetry = context.telemetry();
    telemetry.inject(telemetry).map(|_| telemetry.clone())
}
