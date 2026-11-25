use pi_rs_chord::context::Context;
use pi_rs_schema::{Annotations, Properties, obj};
use serde::{Deserialize, Serialize};

use crate::env::ExecutionEnv;

use super::{
    ToolError, ToolResult, ToolSpec, check_aborted, described_str, require_env, resolve_tool_path,
    text_output, with_file_mutation_queue,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteToolInput {
    pub path: String,
    pub content: String,
}

pub struct WriteTool(ToolSpec);

pub fn create_write_tool() -> WriteTool {
    WriteTool(ToolSpec {
        name: "write",
        description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
        parameters: obj(&Properties::new()
            .with("path", &described_str("Path to the file to write (relative or absolute)"))
            .with("content", &described_str("Content to write to the file")), &Annotations::new(), true),
        structured_output_schema: None,
        output_limits: None,
    })
}

impl WriteTool {
    pub fn spec(&self) -> &ToolSpec {
        &self.0
    }

    pub async fn execute(
        &self,
        args: WriteToolInput,
        env: Option<&dyn ExecutionEnv>,
        context: &Context,
    ) -> Result<ToolResult, ToolError> {
        let env = require_env(env)?;
        let absolute = resolve_tool_path(env, &args.path, context).await?;
        with_file_mutation_queue(
            env,
            &absolute,
            async || {
                check_aborted(context)?;
                env.write_file(&absolute, args.content.as_bytes(), context)
                    .await?;
                check_aborted(context)?;
                Ok(ToolResult {
                    output: Some(vec![text_output(format!(
                        "Successfully wrote to {}",
                        args.path
                    ))]),
                    ..ToolResult::default()
                })
            },
            context,
        )
        .await
    }
}
