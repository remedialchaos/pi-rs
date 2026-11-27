use super::*;
use crate::{
    env::{ExecutionErrorCode, ShellOutputSkip},
    harness::{ExecuteTool, Extension, Invocation, ToolExecution, ToolRegistration},
    session::{Error, Result},
};
use std::sync::Arc;

impl ToolSpec {
    pub(crate) fn registration(&self, replay_safe: bool, execute: ExecuteTool) -> ToolRegistration {
        ToolRegistration {
            declaration: pi_rs_ai::Tool {
                name: self.name.into(),
                description: self.description.clone(),
                parameters: self.parameters.clone(),
                constrained_sampling: None,
            },
            structured_output_schema: self.structured_output_schema.clone(),
            output_limits: self.output_limits,
            replay_safe,
            execute,
            sequential: false,
            prepare_arguments: None,
            cancel: None,
            callers: None,
            deferred: false,
        }
    }
}

impl<D: Serialize> ToolResult<D> {
    pub(crate) fn into_execution(self) -> Result<ToolExecution> {
        Ok(ToolResult {
            output: self.output,
            details: self.details.map(serde_json::to_value).transpose()?,
            diagnostics: self.diagnostics,
            control: self.control,
            usage: self.usage,
            structured_output: self.structured_output,
            is_error: self.is_error,
        }
        .into())
    }
}

struct InvocationOutput<'a>(&'a Invocation);
impl ShellOutput for InvocationOutput<'_> {
    fn output(
        &mut self,
        text: &str,
        skipped: Option<ShellOutputSkip>,
    ) -> std::result::Result<(), ExecutionError> {
        self.0
            .output(
                text,
                skipped.map(|s| pi_rs_chord::output::OutputSkip {
                    bytes: s.bytes,
                    newlines: s.newlines,
                    ends_with_newline: s.ends_with_newline,
                }),
            )
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::CallbackError, e.to_string()))
    }
    fn retained_output(&mut self) -> RetainedShellOutput {
        let retained = self.0.retained_output();
        RetainedShellOutput {
            text: String::from_utf16_lossy(&retained.text.0),
            truncated: retained.dropped_bytes > 0,
        }
    }
    fn diagnostic(&mut self, diagnostic: ToolDiagnostic) {
        self.0.diagnostic(diagnostic);
    }
}

pub fn create_coding_tools_extension(images: Option<Arc<dyn ImageProcessor>>) -> Extension {
    let tools = create_coding_tools(ReadToolOptions {
        images: images.as_deref(),
    });
    let read_images = images.clone();
    let read = tools.read.spec().registration(
        true,
        Arc::new(move |args, inv, ctx| {
            let images = read_images.clone();
            Box::pin(async move {
                let env = inv.env(ctx).await?;
                create_read_tool(ReadToolOptions {
                    images: images.as_deref(),
                })
                .execute_with_model(
                    serde_json::from_value(args)?,
                    env.as_deref(),
                    ctx,
                    || async {
                        let agent = inv
                            .agent(ctx)
                            .await
                            .map_err(|e| ToolError::Message(e.to_string()))?;
                        let model = &agent.state["model"];
                        Ok(model["provider"]
                            .as_str()
                            .zip(model["modelId"].as_str())
                            .and_then(|(p, id)| inv.models().get_model(p, id)))
                    },
                )
                .await
                .map_err(|e| Error::invalid(e.to_string()))?
                .into_execution()
            })
        }),
    );
    let write = tools.write.spec().registration(
        false,
        Arc::new(|args, inv, ctx| {
            Box::pin(async move {
                let env = inv.env(ctx).await?;
                create_write_tool()
                    .execute(serde_json::from_value(args)?, env.as_deref(), ctx)
                    .await
                    .map_err(|e| Error::invalid(e.to_string()))?
                    .into_execution()
            })
        }),
    );
    let mut edit = tools.edit.spec().registration(
        false,
        Arc::new(|args, inv, ctx| {
            Box::pin(async move {
                let env = inv.env(ctx).await?;
                create_edit_tool()
                    .execute(serde_json::from_value(args)?, env.as_deref(), ctx)
                    .await
                    .map_err(|e| Error::invalid(e.to_string()))?
                    .into_execution()
            })
        }),
    );
    edit.prepare_arguments = Some(Arc::new(|args| Ok(prepare_edit_arguments(&args))));
    let bash = tools.bash.spec().registration(
        false,
        Arc::new(|args, inv, ctx| {
            Box::pin(async move {
                let env = inv.env(ctx).await?;
                create_bash_tool(BashToolOptions::default())
                    .execute(
                        serde_json::from_value(args)?,
                        env.as_deref(),
                        &mut InvocationOutput(inv),
                        ctx,
                    )
                    .await
                    .map_err(|e| Error::invalid(e.to_string()))?
                    .into_execution()
            })
        }),
    );
    Extension {
        name: CODING_TOOLS_EXTENSION_NAME.into(),
        tools: vec![read, write, edit, bash],
        ..Default::default()
    }
}
