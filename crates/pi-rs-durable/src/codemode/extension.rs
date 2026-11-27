use super::*;
use crate::{
    documents::resolve_address,
    harness::{Agent, Extension, Invocation, NESTED_CALL_DOC, ToolRegistration, section},
    session::Error,
    tools::ToolResult,
};
use pi_rs_chord::output::{OutputLimits, Retain};

pub(crate) fn catalog(agent: &Agent) -> CodemodeCatalog {
    CodemodeCatalog {
        callable: agent
            .callable
            .iter()
            .map(|t| CodemodeToolSpec {
                name: t.declaration.name.clone(),
                description: t.declaration.description.clone(),
                parameters: t.declaration.parameters.clone(),
                structured_output_schema: t.structured_output_schema.clone(),
                deferred: t.deferred,
            })
            .collect(),
        extensions: agent
            .extensions
            .iter()
            .map(|e| CodemodeExtensionSpec {
                namespace: None,
                tools: e.tools.iter().map(|t| t.declaration.name.clone()).collect(),
            })
            .collect(),
    }
}

fn dispatch(error: impl std::fmt::Display) -> CodemodeError {
    CodemodeError::Dispatch(error.to_string())
}

#[async_trait]
impl CodemodeTools for Invocation {
    fn now(&self) -> u64 {
        Invocation::now(self)
    }
    async fn available_tools(&self, ctx: &Context) -> Result<CodemodeCatalog, CodemodeError> {
        Ok(catalog(&self.agent(ctx).await.map_err(dispatch)?))
    }
    async fn previous_nested_call(
        &self,
        key: &str,
        ctx: &Context,
    ) -> Result<Option<CodemodeNestedInput>, CodemodeError> {
        let session = &self.harness().session;
        let address = resolve_address(&NESTED_CALL_DOC, Some(self.record.id), Some(key.into()))
            .map_err(dispatch)?;
        let Some(member) = session
            .snapshot(&NESTED_CALL_DOC, &address, ctx)
            .await
            .map_err(dispatch)?
        else {
            return Ok(None);
        };
        let id = member
            .get("taskId")
            .and_then(Value::as_u64)
            .ok_or_else(|| dispatch("Missing nested task ID"))?;
        let task = session
            .task(id, ctx)
            .await
            .map_err(dispatch)?
            .ok_or_else(|| dispatch("Missing nested task"))?;
        let call = &task.input["call"];
        Ok(Some(CodemodeNestedInput {
            name: call["name"]
                .as_str()
                .ok_or_else(|| dispatch("Missing nested tool name"))?
                .into(),
            arguments: call["arguments"].clone(),
        }))
    }
    async fn execute_nested(
        &self,
        request: CodemodeNestedRequest<'_>,
        ctx: &Context,
    ) -> Result<CodemodeNestedResult, CodemodeError> {
        let receipt = self
            .execute_tool(
                request.key,
                request.name,
                serde_json::from_value(request.arguments.clone())?,
                true,
                false,
                ctx,
            )
            .await
            .map_err(dispatch)?;
        let errors = receipt["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["severity"] == "error")
            .filter_map(|d| d["message"].as_str().map(str::to_owned))
            .collect();
        let mut result: CodemodeNestedResult = serde_json::from_value(receipt)?;
        result.errors = errors;
        Ok(result)
    }
}

impl CodemodeExtension {
    pub fn into_extension(self) -> Extension {
        let tool = self.tool;
        let budget = tool.options.inline_budget;
        let models = tool.options.models;
        let mut tools = vec![ToolRegistration {
            declaration: tool.declaration(),
            sequential: false,
            prepare_arguments: None,
            cancel: None,
            structured_output_schema: None,
            output_limits: Some(OutputLimits {
                max_bytes: OUTPUT_LIMITS.max_bytes,
                max_lines: OUTPUT_LIMITS.max_lines,
                retain: Retain::Head,
            }),
            replay_safe: true,
            callers: Some(vec!["model".into()]),
            deferred: false,
            execute: Arc::new(move |args, inv, ctx| {
                let tool = tool.clone();
                Box::pin(async move {
                    let documents = SessionCodemodeDocuments::new(
                        inv.harness().session.clone(),
                        inv.record.conversation_id,
                        inv.record.id,
                    )?;
                    let env = inv.env(ctx).await?;
                    let models = inv.models();
                    let result = tool
                        .execute(
                            serde_json::from_value(args)?,
                            CodemodeInvocation {
                                tools: inv,
                                documents: &documents,
                                models: Some(&models),
                                env: env.as_deref(),
                            },
                            ctx,
                        )
                        .await
                        .map_err(|e| Error::invalid(e.to_string()))?;
                    ToolResult {
                        output: Some(result.output),
                        is_error: Some(result.is_error),
                        details: Some(result.details),
                        ..Default::default()
                    }
                    .into_execution()
                })
            }),
        }];
        if models {
            for spec in create_model_tools() {
                let name = spec.name.clone();
                tools.push(ToolRegistration {
                    declaration: Tool {
                        name: spec.name,
                        description: spec.description,
                        parameters: spec.parameters,
                        constrained_sampling: None,
                    },
                    structured_output_schema: spec.structured_output_schema,
                    sequential: false,
                    prepare_arguments: None,
                    cancel: None,
                    output_limits: None,
                    replay_safe: false,
                    callers: Some(vec!["tools".into()]),
                    deferred: false,
                    execute: Arc::new(move |args, inv, ctx| {
                        let name = name.clone();
                        Box::pin(async move {
                            let result =
                                models::execute_model_tool(&inv.models(), &name, &args, ctx)
                                    .await
                                    .map_err(|e| Error::invalid(e.to_string()))?;
                            Ok(ToolResult {
                                output: Some(result.output),
                                structured_output: result.nested.structured_output,
                                is_error: Some(result.nested.is_error),
                                usage: result.nested.usage,
                                ..Default::default()
                            }
                            .into())
                        })
                    }),
                });
            }
        }
        Extension {
            name: CODEMODE_TOOL_NAME.into(),
            tools,
            sections: vec![section(
                CODEMODE_TOOL_NAME,
                Arc::new(move |input, _| {
                    Box::pin(async move {
                        let catalog = serde_json::from_value(input["toolCatalog"].clone())?;
                        codemode_section(&catalog, budget)
                            .map_err(|e| Error::invalid(e.to_string()))
                    })
                }),
                None,
            )],
            ..Default::default()
        }
    }
}

pub fn create_codemode_harness_extension(options: CodemodeToolOptions) -> Extension {
    create_codemode_extension(options).into_extension()
}
