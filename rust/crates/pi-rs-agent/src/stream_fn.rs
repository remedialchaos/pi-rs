use std::sync::{Arc, RwLock};

use pi_rs_ai::{Context, Models};

use crate::{AgentError, Result, StreamFn};

static DEFAULT: RwLock<Option<StreamFn>> = RwLock::new(None);

/// Installs (or removes) the fallback used by agents without an explicit stream function.
pub fn set_default_stream_fn(stream_fn: Option<StreamFn>) {
    *DEFAULT.write().unwrap_or_else(|e| e.into_inner()) = stream_fn;
}

pub fn default_stream_fn() -> Result<StreamFn> {
    DEFAULT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or(AgentError::MissingStreamFunction)
}

/// Adapts a caller-owned model registry without installing a process-wide default.
pub fn models_stream_fn(models: Models) -> StreamFn {
    Arc::new(move |model, context, options| {
        let stream = models.stream_simple(
            model,
            Context {
                messages: context.into_messages(),
                ..Context::default()
            },
            options,
        );
        Box::pin(async move { Ok(stream) })
    })
}
