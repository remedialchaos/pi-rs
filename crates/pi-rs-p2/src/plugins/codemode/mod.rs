//! `codemode` built-in plugin: installs Durable's `codemode` extension backed by
//! `pi-rs-codemode::CodemodeSandbox` (Wasmtime + QuickJS), binds live conversation tools,
//! models, durable `store`/`load`, and `RunningCalls` tracking, and reacts live to `settings.codemode`.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex, Weak},
};

use futures_util::future::BoxFuture;
use pi_rs_ai::registry::Models;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Members, ServeOptions, define_facet},
};
use pi_rs_codemode::{CodemodeSandbox, SandboxOptions};
use pi_rs_durable::{
    ROOT_CONVERSATION_ID, TaskId,
    codemode::{
        CODEMODE_TOOL_NAME, CodemodeCatalog, CodemodeError, CodemodeInvocation,
        CodemodeNestedInput, CodemodeNestedRequest, CodemodeNestedResult, CodemodeToolInput,
        CodemodeToolOptions, CodemodeToolSpec, CodemodeTools, SessionCodemodeDocuments,
        create_codemode_extension,
    },
    documents::resolve_address,
    harness::{
        Agent, Extension, Harness, Invocation, NESTED_CALL_DOC, PromptSection, ToolRegistration,
        define_extension, section,
    },
    session::Error as DurableError,
    tool_search::TOOL_SEARCH_TOOL_NAME,
    tools::ToolResult,
};
use serde_json::Value;
use tokio::sync::Notify;

use crate::api::v1::{
    FacetKind, PluginRegistry,
    p2_contracts::lookup,
    worker::{WorkerProcess, worker_of},
};

/// Default memory limit for p2 codemode scripts (matches upstream `CODEMODE_MEMORY_LIMIT_BYTES`).
pub const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Tracks active `codemode` tool executions so reload drains and removal cancels them.
#[derive(Default)]
pub struct RunningCalls {
    active: Mutex<HashSet<TaskId>>,
    notify: Notify,
}

struct RunningCallGuard<'a> {
    calls: &'a RunningCalls,
    task_id: TaskId,
}

impl Drop for RunningCallGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self.calls.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(&self.task_id);
        if guard.is_empty() {
            self.calls.notify.notify_waiters();
        }
    }
}

impl RunningCalls {
    pub fn count(&self) -> usize {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub async fn track<F, T>(&self, task_id: TaskId, work: F) -> T
    where
        F: Future<Output = T>,
    {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task_id);
        let _guard = RunningCallGuard {
            calls: self,
            task_id,
        };
        work.await
    }

    pub async fn idle(&self) {
        loop {
            let notified = self.notify.notified();
            if self.count() == 0 {
                return;
            }
            notified.await;
        }
    }

    pub async fn cancel(&self, harness: &Harness, ctx: &Context) {
        let ids: Vec<TaskId> = self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        for id in ids {
            let _ = harness.abort_task(id, ctx).await;
        }
    }
}

fn catalog_from_agent(agent: &Agent) -> CodemodeCatalog {
    let callable = agent
        .callable
        .iter()
        .filter(|tool| {
            let name = tool.declaration.name.as_str();
            name != CODEMODE_TOOL_NAME && name != TOOL_SEARCH_TOOL_NAME
        })
        .map(|tool| CodemodeToolSpec {
            name: tool.declaration.name.clone(),
            description: tool.declaration.description.clone(),
            parameters: tool.declaration.parameters.clone(),
            structured_output_schema: tool.structured_output_schema.clone(),
            deferred: tool.deferred,
        })
        .collect();
    CodemodeCatalog {
        callable,
        extensions: Vec::new(),
    }
}

fn dispatch_err(error: impl std::fmt::Display) -> CodemodeError {
    CodemodeError::Dispatch(error.to_string())
}

fn durable_err(error: impl std::fmt::Display) -> DurableError {
    DurableError::invalid(error.to_string())
}

struct HarnessCodemodeTools<'a> {
    invocation: &'a Invocation,
    catalog: CodemodeCatalog,
}

impl CodemodeTools for HarnessCodemodeTools<'_> {
    fn now(&self) -> u64 {
        self.invocation.now()
    }

    fn available_tools<'a, 'b, 'async_trait>(
        &'a self,
        _context: &'b Context,
    ) -> BoxFuture<'async_trait, std::result::Result<CodemodeCatalog, CodemodeError>>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(self.catalog.clone()) })
    }

    fn previous_nested_call<'a, 'b, 'c, 'async_trait>(
        &'a self,
        key: &'b str,
        ctx: &'c Context,
    ) -> BoxFuture<'async_trait, std::result::Result<Option<CodemodeNestedInput>, CodemodeError>>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        'c: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let session = &self.invocation.harness().session;
            let address = resolve_address(
                &NESTED_CALL_DOC,
                Some(self.invocation.record.id),
                Some(key.into()),
            )
            .map_err(dispatch_err)?;
            let Some(document) = session
                .snapshot(&NESTED_CALL_DOC, &address, ctx)
                .await
                .map_err(dispatch_err)?
            else {
                return Ok(None);
            };
            let Some(id) = document.get("taskId").and_then(Value::as_u64) else {
                return Ok(None);
            };
            let task = session
                .task(id, ctx)
                .await
                .map_err(dispatch_err)?
                .ok_or_else(|| dispatch_err("Missing nested tool call"))?;
            let call = &task.input["call"];
            let name = call["name"]
                .as_str()
                .ok_or_else(|| dispatch_err("Missing nested tool name"))?;
            Ok(Some(CodemodeNestedInput {
                name: name.to_owned(),
                arguments: call["arguments"].clone(),
            }))
        })
    }

    fn execute_nested<'a, 'b, 'c, 'async_trait>(
        &'a self,
        request: CodemodeNestedRequest<'b>,
        ctx: &'c Context,
    ) -> BoxFuture<'async_trait, std::result::Result<CodemodeNestedResult, CodemodeError>>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        'c: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if let Some(model_result) = request.execute_model(ctx).await {
                return Ok(model_result.nested);
            }
            let args = serde_json::from_value(request.arguments.clone())?;
            let value = self
                .invocation
                .execute_tool(request.key, request.name, args, true, false, ctx)
                .await
                .map_err(dispatch_err)?;
            let mut result: CodemodeNestedResult = serde_json::from_value(value.clone())?;
            result.errors = value["diagnostics"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| row["severity"] == "error")
                .filter_map(|row| row["message"].as_str().map(str::to_owned))
                .collect();
            Ok(result)
        })
    }
}

/// Builds the Durable `codemode` extension backed by `sandbox`.
pub fn build_codemode_extension(sandbox: CodemodeSandbox) -> Extension {
    build_codemode_extension_with(sandbox, None, None, Arc::new(RunningCalls::default()))
}

/// Builds the Durable `codemode` extension with live worker models, optional worker process
/// lookup for prompt section declarations, and `RunningCalls` tracking.
pub fn build_codemode_extension_with(
    sandbox: CodemodeSandbox,
    models: Option<Models>,
    worker: Option<Weak<WorkerProcess>>,
    calls: Arc<RunningCalls>,
) -> Extension {
    let cm = Arc::new(create_codemode_extension(CodemodeToolOptions::new(sandbox)));
    let declaration = cm.tool.declaration();
    let cm_exec = cm.clone();
    let registration = ToolRegistration {
        declaration,
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: Some(vec!["model".into()]),
        deferred: false,
        execute: Arc::new(move |args, invocation, ctx| {
            let cm = cm_exec.clone();
            let models = models.clone();
            let calls = calls.clone();
            Box::pin(async move {
                let input: CodemodeToolInput = serde_json::from_value(args)?;
                let harness = invocation.harness();
                let conv_id = invocation.record.conversation_id;
                let task_id = invocation.record.id;
                let conversation = harness
                    .conversation(conv_id, ctx)
                    .await?
                    .ok_or_else(|| DurableError::invalid("Conversation is gone"))?;
                let agent = conversation.agent(ctx).await?;
                let tools = HarnessCodemodeTools {
                    invocation,
                    catalog: catalog_from_agent(&agent),
                };
                let documents =
                    SessionCodemodeDocuments::new(harness.session.clone(), conv_id, task_id)?;
                let env = invocation.env(ctx).await?;
                let resolved_models = models.unwrap_or_else(|| invocation.models());
                let result = calls
                    .track(
                        task_id,
                        cm.tool.execute(
                            input,
                            CodemodeInvocation {
                                tools: &tools,
                                documents: &documents,
                                models: Some(&resolved_models),
                                env: env.as_deref(),
                            },
                            ctx,
                        ),
                    )
                    .await
                    .map_err(durable_err)?;
                Ok(ToolResult {
                    output: Some(result.output),
                    is_error: Some(result.is_error),
                    details: Some(serde_json::to_value(result.details)?),
                    ..Default::default()
                }
                .into())
            })
        }),
    };

    let mut sections: Vec<PromptSection> = Vec::new();
    if let Some(worker) = worker {
        let cm_section = cm.clone();
        sections.push(section(
            CODEMODE_TOOL_NAME,
            Arc::new(move |input, ctx| {
                let worker = worker.clone();
                let cm = cm_section.clone();
                Box::pin(async move {
                    let Some(worker) = worker.upgrade() else {
                        return Ok(None);
                    };
                    let Ok(parts) = worker.harness() else {
                        return Ok(None);
                    };
                    let conv_id = input["conversationId"]
                        .as_u64()
                        .unwrap_or(ROOT_CONVERSATION_ID);
                    let Some(conversation) = parts.durable.conversation(conv_id, ctx).await? else {
                        return Ok(None);
                    };
                    let agent = conversation.agent(ctx).await?;
                    cm.section(&catalog_from_agent(&agent)).map_err(durable_err)
                })
            }),
            None,
        ));
    }

    define_extension(Extension {
        name: CODEMODE_TOOL_NAME.into(),
        tools: vec![registration],
        sections,
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let runtime_contract = Arc::new(lookup("p2.codemode-runtime")?);
    registry.register(
        "codemode",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                runtime_contract.clone(),
                move |_, _, object| {
                    if let Ok(worker) = worker_of(&object.host()) {
                        let sandbox = CodemodeSandbox::new(SandboxOptions {
                            memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
                            ..Default::default()
                        })
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                        let calls = Arc::new(RunningCalls::default());
                        let extension = build_codemode_extension_with(
                            sandbox,
                            Some(worker.registries.models.clone()),
                            Some(Arc::downgrade(&worker)),
                            calls.clone(),
                        );
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let initial_enabled = registries
                            .settings
                            .borrow()
                            .get("codemode")
                            .and_then(Value::as_bool)
                            != Some(false);
                        let _ = registries
                            .extensions
                            .switch(&ext_name, Some(initial_enabled));

                        let settings_rx = registries.settings.subscribe();
                        let switch_regs = registries.clone();
                        let switch_name = ext_name.clone();
                        object.on_live(move |ctx| {
                            let switch_regs = switch_regs.clone();
                            let switch_name = switch_name.clone();
                            let mut rx = settings_rx.clone();
                            Box::pin(async move {
                                while matches!(
                                    ctx.context.await_with(rx.changed()).await,
                                    Ok(Ok(()))
                                ) {
                                    let enabled = rx
                                        .borrow_and_update()
                                        .get("codemode")
                                        .and_then(Value::as_bool)
                                        != Some(false);
                                    let _ =
                                        switch_regs.extensions.switch(&switch_name, Some(enabled));
                                }
                                Ok(())
                            })
                        });

                        let close_worker = Arc::downgrade(&worker);
                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                let calls = calls.clone();
                                let close_worker = close_worker.clone();
                                Box::pin(async move {
                                    if let Some(worker) = close_worker.upgrade()
                                        && !worker.stopping()
                                        && let Ok(parts) = worker.harness()
                                    {
                                        calls.cancel(&parts.durable, &Context::background()).await;
                                    }
                                    let _ = registries.extensions.switch(&ext_name, None);
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}
