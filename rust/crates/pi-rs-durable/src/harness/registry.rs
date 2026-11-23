use super::Invocation;
use crate::{
    session::{Error, Result},
    tasks::Task,
};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use pi_rs_ai::types::Tool;
use pi_rs_chord::context::Context;
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

pub type ExecuteTool = Arc<
    dyn for<'a> Fn(
            Value,
            &'a Invocation,
            &'a Context,
        ) -> BoxFuture<'a, Result<super::ToolExecution>>
        + Send
        + Sync,
>;
pub type RenderSection = Arc<
    dyn for<'a> Fn(&'a Value, &'a Context) -> BoxFuture<'a, Result<Option<String>>> + Send + Sync,
>;
pub type PrepareArguments = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;
pub type CancelTool = Arc<
    dyn for<'a> Fn(Value, &'a Invocation, &'a Context) -> BoxFuture<'a, Result<()>> + Send + Sync,
>;
#[derive(Clone)]
pub struct ToolRegistration {
    pub declaration: Tool,
    pub sequential: bool,
    pub prepare_arguments: Option<PrepareArguments>,
    pub cancel: Option<CancelTool>,
    pub structured_output_schema: Option<Value>,
    pub output_limits: Option<pi_rs_chord::output::OutputLimits>,
    pub execute: ExecuteTool,
    pub replay_safe: bool,
    pub callers: Option<Vec<String>>,
    pub deferred: bool,
}
#[derive(Clone)]
pub struct PromptSection {
    pub key: String,
    pub render: RenderSection,
    pub tag: bool,
}
#[derive(Clone, Default)]
pub struct Extension {
    pub hooks: Vec<super::HookRegistration<super::HookHandlers>>,
    pub wraps: Vec<super::Wrap>,
    pub name: String,
    pub tasks: Vec<Arc<Task<Invocation>>>,
    pub tools: Vec<ToolRegistration>,
    pub sections: Vec<PromptSection>,
}

#[derive(Clone, Default)]
pub struct RegistrySnapshot {
    extensions: IndexMap<String, Arc<Extension>>,
    tasks: IndexMap<String, Arc<Task<Invocation>>>,
}
impl PartialEq for RegistrySnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.extensions.len() == other.extensions.len()
            && self.tasks.len() == other.tasks.len()
            && self
                .extensions
                .iter()
                .zip(&other.extensions)
                .all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
            && self
                .tasks
                .iter()
                .zip(&other.tasks)
                .all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
    }
}
impl RegistrySnapshot {
    pub fn installed(&self) -> impl Iterator<Item = &Arc<Extension>> {
        self.extensions.values()
    }
    pub fn extension(&self, name: &str) -> Option<&Arc<Extension>> {
        self.extensions.get(name)
    }
    pub fn task(&self, name: &str) -> Option<&Arc<Task<Invocation>>> {
        self.tasks.get(name)
    }
    pub fn tasks(&self) -> impl Iterator<Item = &Arc<Task<Invocation>>> {
        self.tasks.values()
    }
    pub fn tools(&self) -> impl Iterator<Item = (&Extension, &ToolRegistration)> {
        self.extensions
            .values()
            .flat_map(|e| e.tools.iter().map(|tool| (e.as_ref(), tool)))
    }
    pub fn sections(&self) -> impl Iterator<Item = (&Extension, &PromptSection)> {
        self.extensions
            .values()
            .flat_map(|e| e.sections.iter().map(|s| (e.as_ref(), s)))
    }
}
pub struct Registry {
    current: watch::Sender<Arc<RegistrySnapshot>>,
    builtins: IndexMap<String, Arc<Task<Invocation>>>,
    line: Mutex<()>,
}
pub fn create_registry() -> Registry {
    let builtins = [
        super::generation_task(),
        super::tool_task(),
        super::compaction_task(),
    ];
    let builtins: IndexMap<_, _> = builtins
        .into_iter()
        .map(|t| (t.name().to_owned(), t))
        .collect();
    Registry {
        current: watch::channel(Arc::new(RegistrySnapshot {
            tasks: builtins.clone(),
            ..Default::default()
        }))
        .0,
        builtins,
        line: Mutex::new(()),
    }
}
impl Registry {
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.current.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<RegistrySnapshot>> {
        self.current.subscribe()
    }
    pub fn install(&self, extension: Extension) -> Result<()> {
        let mut seen = HashSet::new();
        for tool in &extension.tools {
            let name = &tool.declaration.name;
            if !seen.insert(name) {
                return Err(Error::invalid(format!(
                    "Extension {} has two tools named {name}",
                    extension.name
                )));
            }
        }
        seen.clear();
        for section in &extension.sections {
            let key = &section.key;
            if !key.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(Error::invalid(format!(
                    "Section key {key:?} must match /^[a-z][a-z0-9_-]*$/"
                )));
            }
            if key == "instructions" {
                return Err(Error::invalid(
                    "Section key instructions is reserved for the agent's instructions",
                ));
            }
            if !seen.insert(key) {
                return Err(Error::invalid(format!(
                    "Extension {} has two sections with key {key}",
                    extension.name
                )));
            }
        }
        let _guard = self.line.lock().unwrap_or_else(|e| e.into_inner());
        let mut extensions = self.snapshot().extensions.clone();
        extensions.insert(extension.name.clone(), Arc::new(extension));
        self.publish(extensions)
    }
    pub fn uninstall(&self, name: &str) -> Result<()> {
        let _guard = self.line.lock().unwrap_or_else(|e| e.into_inner());
        let mut extensions = self.snapshot().extensions.clone();
        if extensions.shift_remove(name).is_some() {
            self.publish(extensions)?;
        }
        Ok(())
    }
    fn publish(&self, extensions: IndexMap<String, Arc<Extension>>) -> Result<()> {
        let mut tasks = self.builtins.clone();
        for extension in extensions.values() {
            for task in &extension.tasks {
                if tasks.insert(task.name().into(), task.clone()).is_some() {
                    return Err(Error::invalid(format!(
                        "Task {} of extension {} is already installed",
                        task.name(),
                        extension.name
                    )));
                }
            }
        }
        self.current
            .send_replace(Arc::new(RegistrySnapshot { extensions, tasks }));
        Ok(())
    }
}
