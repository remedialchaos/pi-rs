//! Definitions are separate from the scheduler that reserves and runs a task.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use pi_rs_chord::context::Context;
use serde_json::Value;

use crate::errors::BoxError;
use crate::ids::{ConversationId, TaskId};
use crate::storage_types::TaskRecord;

pub type TaskInitial = Arc<dyn Fn(&Value) -> Result<Value, BoxError> + Send + Sync>;
pub type TaskMigration =
    Arc<dyn Fn(Value, Value, u64) -> Result<MigratedTask, BoxError> + Send + Sync>;
pub type PhaseHandler<R> = Arc<
    dyn for<'a> Fn(&'a TaskRecord, &'a R, &'a Context) -> BoxFuture<'a, Result<(), BoxError>>
        + Send
        + Sync,
>;

#[derive(Debug, PartialEq)]
pub struct MigratedTask {
    pub input: Value,
    pub checkpoint: Value,
}

/// The scheduler supplies the invocation implementation; it is not part of a definition.
pub trait TaskRuntime: Send + Sync {
    fn task_id(&self) -> TaskId;
    fn conversation_id(&self) -> ConversationId;
}

pub struct TaskDefinition<R: ?Sized = dyn TaskRuntime, H = ()> {
    pub name: String,
    pub version: u64,
    pub initial: TaskInitial,
    pub phases: IndexMap<String, PhaseHandler<R>>,
    pub abort: PhaseHandler<R>,
    pub migrate: Option<TaskMigration>,
    pub hooks: H,
}

pub struct Task<R: ?Sized = dyn TaskRuntime, H = ()> {
    pub definition: TaskDefinition<R, H>,
}

/// Register this token with the caller's scheduler to execute its phase map.
pub const fn define_task<R: ?Sized, H>(definition: TaskDefinition<R, H>) -> Task<R, H> {
    Task { definition }
}

impl<R: ?Sized, H> Task<R, H> {
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    pub const fn version(&self) -> u64 {
        self.definition.version
    }

    pub fn initial(&self, input: &Value) -> Result<Value, BoxError> {
        (self.definition.initial)(input)
    }
}
