//! Live worker extensions and their Durable tool admission rules.
use std::{
    borrow::Cow,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use indexmap::IndexMap;
use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    state::{Batch, NativeSource},
};
use pi_rs_durable::{
    harness::{
        Extension, HarnessRegistry, RegistrySnapshot, ToolRegistration, create_registry,
        registry_reader,
    },
    session::Error as DurableError,
    tool_search::{
        TOOL_SEARCH_EXTENSION_NAME, ToolSearchHost, ToolSearchToolInput, create_tool_search_tool,
    },
    tools::{ToolError, ToolResult},
};

use crate::runtime::{
    main_conversation::is_top_level,
    session_lock::{LockCapability, session_lock},
};

#[derive(Clone, Default)]
pub struct ToolAccess {
    pub main_only: bool,
    pub locked: LockedTool,
}

#[derive(Clone, Default)]
pub enum LockedTool {
    #[default]
    Pass,
    Never,
    Capability(LockCapability),
}

pub fn guard_tool(mut tool: ToolRegistration, access: ToolAccess) -> ToolRegistration {
    if !access.main_only && matches!(access.locked, LockedTool::Pass) {
        return tool;
    }
    let name = tool.declaration.name.clone();
    let execute = tool.execute;
    tool.execute = Arc::new(move |args, invocation, context| {
        let name = name.clone();
        let access = access.clone();
        let execute = execute.clone();
        Box::pin(async move {
            let session = &invocation.harness().session;
            let refusal = if access.main_only
                && !is_top_level(session, invocation.record.conversation_id, context)
                    .await
                    .map_err(|e| DurableError::invalid(e.to_string()))?
            {
                Some("works only in the session's main conversation.")
            } else if !matches!(access.locked, LockedTool::Pass) {
                match (
                    session_lock(session, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?,
                    access.locked,
                ) {
                    (Some(_), LockedTool::Never) => Some("is not available in a locked session."),
                    (Some(lock), LockedTool::Capability(capability))
                        if !lock.capabilities.contains(&capability) =>
                    {
                        Some("is not among the resources the owner picked for this session.")
                    }
                    _ => None,
                }
            } else {
                None
            };
            if let Some(reason) = refusal {
                return Ok(ToolResult {
                    output: Some(vec![InputContent::Text(format!("{name} {reason}").into())]),
                    is_error: Some(true),
                    ..Default::default()
                }
                .into());
            }
            execute(args, invocation, context).await
        })
    });
    tool
}

#[derive(Clone, Default)]
struct Contributions {
    extensions: IndexMap<String, Extension>,
    switches: IndexMap<String, bool>,
}

/// Native plugins publish extensions here. The harness observes successful snapshots.
pub struct DurableRegistry {
    contributions: Mutex<Contributions>,
    source: NativeSource<Option<Arc<RegistrySnapshot>>>,
}

impl Default for DurableRegistry {
    fn default() -> Self {
        Self {
            contributions: Mutex::default(),
            source: NativeSource::new(
                Batch::new(|_| {}),
                Some(create_registry().snapshot()),
                |a, b| a == b,
            ),
        }
    }
}

impl DurableRegistry {
    pub fn install(&self, extension: Extension) -> Result<()> {
        self.update(|state| {
            state.extensions.insert(extension.name.clone(), extension);
        })
    }

    /// Apply p2's session and conversation access rules before making tools live.
    pub fn install_with_access(
        &self,
        mut extension: Extension,
        access: &IndexMap<String, ToolAccess>,
    ) -> Result<()> {
        extension.tools = extension
            .tools
            .into_iter()
            .map(|tool| {
                let rule = access
                    .get(&tool.declaration.name)
                    .cloned()
                    .unwrap_or_default();
                guard_tool(tool, rule)
            })
            .collect();
        self.install(extension)
    }

    pub fn uninstall(&self, name: &str) -> Result<()> {
        self.update(|state| {
            state.extensions.shift_remove(name);
        })
    }

    /// A missing switch enables the extension, as does an explicit true.
    pub fn switch(&self, name: &str, enabled: Option<bool>) -> Result<()> {
        self.update(|state| match enabled {
            Some(enabled) => {
                state.switches.insert(name.into(), enabled);
            }
            None => {
                state.switches.shift_remove(name);
            }
        })
    }

    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.source.value().unwrap_or_default()
    }

    pub(super) fn reader(&self, context: &Context) -> Result<HarnessRegistry> {
        Ok(HarnessRegistry::Chord(registry_reader(
            &self.source,
            context,
        )?))
    }

    fn update(&self, change: impl FnOnce(&mut Contributions)) -> Result<()> {
        let mut current = self
            .contributions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut next = current.clone();
        change(&mut next);
        let registry = create_registry();
        let enabled = |name: &str| next.switches.get(name) != Some(&false);
        let mut deferred = false;
        for extension in next
            .extensions
            .values()
            .filter(|extension| enabled(&extension.name))
        {
            deferred |= extension.tools.iter().any(|tool| {
                tool.deferred
                    && tool
                        .callers
                        .as_ref()
                        .is_none_or(|callers| callers.iter().any(|caller| caller == "model"))
            });
            registry.install(extension.clone()).map_err(durable_error)?;
        }
        if deferred
            && !(next.extensions.contains_key("codemode") && enabled("codemode"))
            && !next.extensions.contains_key(TOOL_SEARCH_EXTENSION_NAME)
        {
            registry
                .install(tool_search_extension())
                .map_err(durable_error)?;
        }
        self.source
            .set(Some(registry.snapshot()), &Context::background())?;
        *current = next;
        Ok(())
    }
}

pub(super) fn durable_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}

struct DeferredTools(Vec<Tool>);

impl ToolSearchHost for DeferredTools {
    fn deferred_tools<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _: &'life1 Context,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<Cow<'life0, [Tool]>, ToolError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(Cow::Borrowed(self.0.as_slice())) })
    }
}

fn tool_search_extension() -> Extension {
    let search = Arc::new(create_tool_search_tool());
    let spec = search.spec();
    Extension {
        name: TOOL_SEARCH_EXTENSION_NAME.into(),
        tools: vec![ToolRegistration {
            declaration: Tool {
                name: spec.name.to_owned(),
                description: spec.description.clone(),
                parameters: spec.parameters.clone(),
                constrained_sampling: None,
            },
            sequential: false,
            prepare_arguments: None,
            cancel: None,
            structured_output_schema: None,
            output_limits: None,
            replay_safe: true,
            callers: Some(vec!["model".into()]),
            deferred: false,
            execute: Arc::new(move |args, invocation, context| {
                let search = search.clone();
                Box::pin(async move {
                    let args: ToolSearchToolInput = serde_json::from_value(args)?;
                    let conversation = invocation
                        .harness()
                        .conversation(invocation.record.conversation_id, context)
                        .await?
                        .ok_or_else(|| DurableError::invalid("Conversation is gone"))?;
                    let tools = DeferredTools(
                        conversation
                            .agent(context)
                            .await?
                            .deferred
                            .into_iter()
                            .map(|tool| tool.declaration)
                            .collect(),
                    );
                    let result = search
                        .execute(args, &tools, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    Ok(ToolResult {
                        output: result.output,
                        details: result.details.map(serde_json::to_value).transpose()?,
                        control: result.control,
                        ..Default::default()
                    }
                    .into())
                })
            }),
        }],
        ..Default::default()
    }
}
