use super::{Extension, Invocation, PromptSection, RenderSection, ToolRegistration};
use crate::tasks::Task;
use serde_json::Value;
use std::sync::Arc;

pub fn define_extension(extension: Extension) -> Extension {
    extension
}
pub fn define_tool(tool: ToolRegistration) -> ToolRegistration {
    tool
}
pub fn section(key: impl Into<String>, render: RenderSection, tag: Option<bool>) -> PromptSection {
    PromptSection {
        key: key.into(),
        render,
        tag: tag.unwrap_or(true),
    }
}

pub struct HookTask {
    pub task: Arc<Task<Invocation>>,
    pub input: Value,
}
pub fn hook_task(task: Arc<Task<Invocation>>, input: Value) -> HookTask {
    HookTask { task, input }
}
#[derive(Clone)]
pub struct HookRegistration<H> {
    pub task: String,
    pub handlers: H,
}
pub fn hook<H>(task: &Task<Invocation>, handlers: H) -> HookRegistration<H> {
    HookRegistration {
        task: task.name().to_owned(),
        handlers,
    }
}

#[derive(Clone)]
pub enum Wrap {
    Tool {
        name: String,
        wrap: Arc<dyn Fn(ToolRegistration) -> ToolRegistration + Send + Sync>,
    },
    Section {
        key: String,
        wrap: Arc<dyn Fn(PromptSection) -> PromptSection + Send + Sync>,
    },
}
pub fn wrap_tool(
    tool: &ToolRegistration,
    wrapper: impl Fn(ToolRegistration) -> ToolRegistration + Send + Sync + 'static,
) -> Wrap {
    Wrap::Tool {
        name: tool.declaration.name.clone(),
        wrap: Arc::new(wrapper),
    }
}
pub fn wrap_section(
    key: impl Into<String>,
    wrapper: impl Fn(PromptSection) -> PromptSection + Send + Sync + 'static,
) -> Wrap {
    Wrap::Section {
        key: key.into(),
        wrap: Arc::new(wrapper),
    }
}

pub type HookHandler = Arc<
    dyn for<'a> Fn(
            Value,
            &'a Invocation,
            &'a pi_rs_chord::context::Context,
        )
            -> futures_util::future::BoxFuture<'a, crate::session::Result<Option<Value>>>
        + Send
        + Sync,
>;
pub type HookHandlers = indexmap::IndexMap<String, HookHandler>;

impl super::Agent {
    pub(super) fn hooks(&self, kind: &str, event: &str) -> Vec<HookHandler> {
        self.extensions
            .iter()
            .flat_map(|e| e.hooks.iter())
            .filter(|h| h.task == kind)
            .filter_map(|h| h.handlers.get(event).cloned())
            .collect()
    }
}

impl serde::Serialize for HookTask {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut fields = serializer.serialize_struct("HookTask", 2)?;
        fields.serialize_field("task", self.task.name())?;
        fields.serialize_field("input", &self.input)?;
        fields.end()
    }
}
