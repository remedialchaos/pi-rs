//! Offer the native codemode tool only the MCP namespaces and tools the conversation leases.

use std::sync::{Arc, LazyLock, Weak};

use futures_util::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
use pi_rs_chord::context::Context;
use pi_rs_codemode::{CodemodeSandbox, SandboxOptions};
use pi_rs_durable::{
    codemode::{
        CODEMODE_TOOL_NAME, CodemodeCatalog, CodemodeError, CodemodeExtensionSpec,
        CodemodeInvocation, CodemodeNestedInput, CodemodeNestedRequest, CodemodeNestedResult,
        CodemodeTool, CodemodeToolOptions, CodemodeToolSpec, CodemodeTools,
        DEFAULT_CODEMODE_INLINE_BUDGET, SessionCodemodeDocuments, ToolNamespace, codemode_section,
        create_codemode_tool,
    },
    documents::resolve_address,
    harness::{Agent, Invocation, NESTED_CALL_DOC, PromptSection, Wrap, section},
    session::Error,
    tools::ToolResult,
};
use pi_rs_mcp::{mcp_namespace, mcp_server_summary};
use serde_json::Value;

use crate::{api::v1::worker::WorkerProcess, runtime::location::Location};

use super::tools::{Key, Offer, Variant, key, locate};

static CODEMODE: LazyLock<std::result::Result<CodemodeTool, String>> = LazyLock::new(|| {
    let sandbox = CodemodeSandbox::new(SandboxOptions {
        memory_limit_bytes: Some(crate::plugins::codemode::CODEMODE_MEMORY_LIMIT_BYTES),
        ..Default::default()
    })
    .map_err(|error| error.to_string())?;
    let mut options = CodemodeToolOptions::new(sandbox);
    // The host's codemode facet has no registered classify or image-generation nested tasks.
    options.models = false;
    Ok(create_codemode_tool(options))
});

pub(super) fn adapt(
    offer: Arc<Offer>,
    worker: Weak<WorkerProcess>,
    variants: Arc<IndexMap<Key, Variant>>,
) -> (Wrap, PromptSection) {
    let prompt_offer = offer.clone();
    let prompt_worker = worker.clone();
    let prompt_variants = variants.clone();
    let prompt = section(
        CODEMODE_TOOL_NAME,
        Arc::new(move |input, ctx| {
            let (offer, worker, variants) = (
                prompt_offer.clone(),
                prompt_worker.clone(),
                prompt_variants.clone(),
            );
            Box::pin(async move {
                let conversation = input["conversationId"]
                    .as_u64()
                    .ok_or_else(|| Error::invalid("Missing conversation ID"))?;
                let worker = worker
                    .upgrade()
                    .ok_or_else(|| Error::invalid("Worker stopped"))?;
                let parts = worker.harness().map_err(invalid)?;
                let harness = &parts.durable;
                let conversation = harness
                    .conversation(conversation, ctx)
                    .await?
                    .ok_or_else(|| Error::invalid("Conversation is gone"))?;
                let location =
                    locate(&worker, conversation.id, Some(&harness.session), ctx).await?;
                let agent = conversation.agent(ctx).await?;
                codemode_section(
                    &catalog(&offer, &variants, &location, &agent),
                    DEFAULT_CODEMODE_INLINE_BUDGET,
                )
                .map_err(invalid)
            })
        }),
        None,
    );
    let wrap = Wrap::Tool {
        name: CODEMODE_TOOL_NAME.into(),
        wrap: Arc::new(move |mut base| {
            let (offer, worker, variants) = (offer.clone(), worker.clone(), variants.clone());
            base.execute = Arc::new(move |args, invocation, ctx| {
                let (offer, worker, variants) = (offer.clone(), worker.clone(), variants.clone());
                Box::pin(async move {
                    let tool = CODEMODE.as_ref().map_err(invalid)?;
                    let worker = worker
                        .upgrade()
                        .ok_or_else(|| Error::invalid("Worker stopped"))?;
                    let harness = invocation.harness();
                    let id = invocation.record.conversation_id;
                    let conversation = harness
                        .conversation(id, ctx)
                        .await?
                        .ok_or_else(|| Error::invalid("Conversation is gone"))?;
                    let location = locate(&worker, id, Some(&harness.session), ctx).await?;
                    let agent = conversation.agent(ctx).await?;
                    let host = ToolHost {
                        invocation,
                        visible: catalog(&offer, &variants, &location, &agent),
                    };
                    let documents = SessionCodemodeDocuments::new(
                        harness.session.clone(),
                        id,
                        invocation.record.id,
                    )?;
                    let env = invocation.env(ctx).await?;
                    let models = invocation.models();
                    let result = tool
                        .execute(
                            serde_json::from_value(args)?,
                            CodemodeInvocation {
                                tools: &host,
                                documents: &documents,
                                models: Some(&models),
                                env: env.as_deref(),
                            },
                            ctx,
                        )
                        .await
                        .map_err(invalid)?;
                    Ok(ToolResult {
                        output: Some(result.output),
                        is_error: Some(result.is_error),
                        details: Some(serde_json::to_value(result.details)?),
                        ..Default::default()
                    }
                    .into())
                })
            });
            base
        }),
    };
    (wrap, prompt)
}

fn catalog(
    offer: &Offer,
    variants: &IndexMap<Key, Variant>,
    location: &Location,
    agent: &Agent,
) -> CodemodeCatalog {
    let all: IndexSet<_> = variants
        .values()
        .map(|variant| variant.name.as_str())
        .collect();
    let permitted: IndexSet<_> = agent
        .callable
        .iter()
        .map(|tool| tool.declaration.name.as_str())
        .collect();
    let mut extensions = Vec::new();
    let mut visible = IndexSet::new();
    for server in offer.servers_at(location) {
        let tools: Vec<_> = server
            .tools
            .iter()
            .filter_map(|tool| variants.get(&key(&server.server, tool)))
            .filter(|variant| permitted.contains(variant.name.as_str()))
            .map(|variant| variant.name.clone())
            .collect();
        visible.extend(tools.iter().cloned());
        let summary = mcp_server_summary(
            server.description.as_deref(),
            server.instructions.as_deref(),
        );
        let summary = if summary.encode_utf16().count() > 250 {
            let mut size = 0usize;
            let prefix: String = summary
                .chars()
                .take_while(|character| {
                    size += character.len_utf16();
                    size <= 249
                })
                .collect();
            format!("{}…", prefix.trim_end())
        } else {
            summary
        };
        let mut description = if summary.is_empty() {
            String::new()
        } else {
            format!("{summary}\n")
        };
        description.push_str(&format!(
            "Tools of the MCP server \"{}\" (find them with searchTools()).",
            server.server
        ));
        if server.auth.as_deref() == Some("oauth") {
            description.push_str(&format!(" It needs the user's sign-in: if it has no tools, ask the user to sign in from the MCP panel in p2's sidebar or at /mcp/sign-in/{} on p2.",server.server));
        }
        extensions.push(CodemodeExtensionSpec {
            namespace: Some(ToolNamespace {
                name: mcp_namespace(&server.server),
                description,
                instructions: server.instructions.clone(),
            }),
            tools,
        });
    }
    let callable = agent
        .callable
        .iter()
        .filter(|tool| {
            let name = &tool.declaration.name;
            visible.contains(name) || !all.contains(name.as_str())
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
        extensions,
    }
}

struct ToolHost<'a> {
    invocation: &'a Invocation,
    visible: CodemodeCatalog,
}

impl CodemodeTools for ToolHost<'_> {
    fn now(&self) -> u64 {
        self.invocation.now()
    }

    fn available_tools<'a, 'b, 'async_trait>(
        &'a self,
        _ctx: &'b Context,
    ) -> BoxFuture<'async_trait, std::result::Result<CodemodeCatalog, CodemodeError>>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(self.visible.clone()) })
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
            .map_err(dispatch)?;
            let Some(document) = session
                .snapshot(&NESTED_CALL_DOC, &address, ctx)
                .await
                .map_err(dispatch)?
            else {
                return Ok(None);
            };
            let Some(id) = document.get("taskId").and_then(Value::as_u64) else {
                return Ok(None);
            };
            let task = session
                .task(id, ctx)
                .await
                .map_err(dispatch)?
                .ok_or_else(|| dispatch("Missing nested tool call"))?;
            let call = &task.input["call"];
            let name = call["name"]
                .as_str()
                .ok_or_else(|| dispatch("Missing nested tool name"))?;
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
            let args = serde_json::from_value(request.arguments.clone())?;
            let value = self
                .invocation
                .execute_tool(request.key, request.name, args, true, false, ctx)
                .await
                .map_err(dispatch)?;
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

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::invalid(error.to_string())
}
fn dispatch(error: impl std::fmt::Display) -> CodemodeError {
    CodemodeError::Dispatch(error.to_string())
}
