use super::{Invocation, ToolRegistration};
use crate::{JoinPolicy, SignalKey, TaskId, TaskState, tools::ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ToolPark {
    Sleep {
        checkpoint: Option<Value>,
        until: Option<u64>,
        signals: Option<Vec<SignalKey>>,
    },
    Wait {
        checkpoint: Option<Value>,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
}

pub enum ToolExecution {
    Result(Box<ToolResult>),
    Park(Arc<ToolPark>),
}
impl From<ToolResult> for ToolExecution {
    fn from(value: ToolResult) -> Self {
        Self::Result(Box::new(value))
    }
}
impl ToolPark {
    pub(super) fn state(&self, args: Value, replay: &Value) -> TaskState {
        let parked = |payload: &Option<Value>| {
            let mut data = serde_json::json!({"phase":"parked","arguments":args,"replay":replay});
            if let Some(value) = payload {
                data["checkpoint"] = value.clone();
            }
            data
        };
        match self {
            Self::Sleep {
                checkpoint,
                until,
                signals,
            } => TaskState::Sleeping {
                checkpoint: parked(checkpoint),
                until: *until,
                signals: signals.clone(),
            },
            Self::Wait {
                checkpoint,
                on,
                policy,
            } => TaskState::Waiting {
                checkpoint: parked(checkpoint),
                on: on.clone(),
                policy: *policy,
            },
        }
    }
}

pub(super) fn wrap_composite(
    tool: ToolRegistration,
    wrap: &dyn Fn(ToolRegistration) -> ToolRegistration,
) -> ToolRegistration {
    let mut base = tool;
    let execute = base.execute.clone();
    base.execute = Arc::new(move |args, inv, ctx| {
        let execute = execute.clone();
        Box::pin(async move {
            let value = execute(args, inv, ctx).await?;
            if let ToolExecution::Park(park) = &value {
                inv.base_parks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(park.clone());
            }
            Ok(value)
        })
    });
    let mut composite = wrap(base);
    let execute = composite.execute.clone();
    composite.execute = Arc::new(move |args, inv: &Invocation, ctx| {
        let execute = execute.clone();
        Box::pin(async move {
            let returned = execute(args, inv, ctx).await?;
            if let ToolExecution::Park(park) = &returned
                && !inv
                    .base_parks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|p| Arc::ptr_eq(park, p))
            {
                return Err(crate::session::Error::invalid(
                    "a wrapper returned a park its base did not return",
                ));
            }
            Ok(returned)
        })
    });
    composite
}
