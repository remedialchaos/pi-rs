//! Built-in coding tools, independent of a harness or tool registry.

mod bash;
mod edit;
mod extensions;
pub use extensions::create_coding_tools_extension;
pub mod edit_diff;
mod file_mutation_queue;
pub mod image;
mod path_utils;
mod read;
mod write;

use pi_rs_ai::types::{InputContent, Usage};
use pi_rs_chord::{context::Context, output::OutputLimits};
use pi_rs_schema::{Annotations, JsonSchema, NumOptions, StrOptions, builders::str_with, num};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::env::{ExecutionEnv, ExecutionError, FileError};

pub use crate::images::{
    DEFAULT_IMAGE_LIMITS, ImageLimits, ImageProcessor, PreparedImage, image_limits,
};
pub use bash::{
    BashExecution, BashPrepare, BashTool, BashToolInput, BashToolOptions, PowerShellTool,
    PowerShellToolInput, PowerShellToolOptions, RetainedShellOutput, ShellOutput,
    ShellStructuredOutput, TailShellOutput, create_bash_tool, create_powershell_tool,
};
pub use edit::{
    EditTool, EditToolDetails, EditToolInput, create_edit_tool, prepare_edit_arguments,
};
pub use file_mutation_queue::with_file_mutation_queue;
pub use path_utils::{resolve_read_tool_path, resolve_tool_path};
pub use read::{
    ReadTool, ReadToolDetails, ReadToolInput, ReadToolOptions, ReadTruncation, create_read_tool,
};
pub use write::{WriteTool, WriteToolInput, create_write_tool};

pub const CODING_TOOLS_EXTENSION_NAME: &str = "coding-tools";

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    pub parameters: JsonSchema,
    pub structured_output_schema: Option<JsonSchema>,
    pub output_limits: Option<OutputLimits>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ToolDiagnostic {
    pub fn new(
        severity: DiagnosticSeverity,
        code: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            message: message.into(),
            code: code.map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolControl {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

/// `None` for `output` means the streamed text is the result; `Some([])` means explicitly empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult<D = Value> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<InputContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<D>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<ToolControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl<D> Default for ToolResult<D> {
    fn default() -> Self {
        Self {
            output: None,
            structured_output: None,
            is_error: None,
            details: None,
            diagnostics: None,
            control: None,
            usage: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("No execution environment is configured")]
    MissingEnvironment,
    #[error("Operation aborted")]
    Aborted,
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error(transparent)]
    Image(#[from] crate::images::ImageProcessingError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Could not edit file: {path}. Error code: {code}.")]
    EditAccess {
        path: String,
        code: &'static str,
        #[source]
        source: FileError,
    },
    #[error("{0}")]
    Message(String),
}

pub fn require_env(env: Option<&dyn ExecutionEnv>) -> Result<&dyn ExecutionEnv, ToolError> {
    env.ok_or(ToolError::MissingEnvironment)
}

pub(super) fn check_aborted(context: &Context) -> Result<(), ToolError> {
    if context.is_aborted() {
        Err(ToolError::Aborted)
    } else {
        Ok(())
    }
}

pub(crate) fn is_js_space(ch: char) -> bool {
    matches!(ch, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

pub(crate) fn described_str(description: &str) -> JsonSchema {
    str_with(&StrOptions::new().annotations(Annotations::new().description(description)))
}

pub(crate) fn described_num(description: impl Into<String>) -> JsonSchema {
    num(&NumOptions::new().annotations(Annotations::new().description(description)))
}

pub(super) fn text_output(text: String) -> InputContent {
    InputContent::Text(text.into())
}

pub struct CodingTools<'a> {
    pub read: ReadTool<'a>,
    pub write: WriteTool,
    pub edit: EditTool,
    pub bash: BashTool<'a>,
}

impl CodingTools<'_> {
    pub fn specs(&self) -> [&ToolSpec; 4] {
        [
            self.read.spec(),
            self.write.spec(),
            self.edit.spec(),
            self.bash.spec(),
        ]
    }
}

pub fn create_coding_tools(options: ReadToolOptions<'_>) -> CodingTools<'_> {
    CodingTools {
        read: create_read_tool(options),
        write: create_write_tool(),
        edit: create_edit_tool(),
        bash: create_bash_tool(BashToolOptions::default()),
    }
}
