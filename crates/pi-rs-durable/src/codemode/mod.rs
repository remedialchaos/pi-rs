//! Durable JavaScript tools over the `pi-rs-codemode` sandbox and `pi-rs-ai` providers.

mod describe;
mod models;
mod output;
mod run;
mod session;
mod state;

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use pi_rs_ai::{ConstrainedSampling, ConstrainedSamplingConfig, InputContent, Tool, Usage};
use pi_rs_chord::context::{AbortError, Context};
use pi_rs_codemode::{CodemodeSandbox, HostError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::env::ExecutionEnv;

pub use describe::{
    CodemodeCatalog, CodemodeExtensionSpec, CodemodeToolSpec, DEFAULT_CODEMODE_INLINE_BUDGET,
    ToolNamespace, codemode_description, codemode_section, declaration_of, declared_namespaces,
    namespaces_of, sample_of,
};
pub use models::{
    CLASSIFY_TOOL, CODEMODE_INTERNAL_PREFIX, CodemodeModelProvider, CodemodeModelToolResult,
    GENERATE_IMAGES_TOOL, MAX_CONCURRENT_MODEL_CALLS, create_model_tools,
};
pub use output::{CodemodeOutputLimits, DEFAULT_MAX_OUTPUT_TOKENS, OUTPUT_LIMITS};
pub use session::SessionCodemodeDocuments;
pub use state::{
    CODEMODE_DOCUMENT_VERSION, CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC,
    CODEMODE_STORE_CHECKPOINT_DELTAS, CODEMODE_STORE_DOC, CodemodeDeclaredNamespace,
    CodemodeDeclaredTool, CodemodeDocuments, CodemodeGlobalSpec, CodemodeJournalEntry,
    CodemodeJournalGlobal, CodemodeJournalState, CodemodeJournalUpdate, CodemodeOutcome,
    CodemodeRunState, CodemodeSnapshot, CodemodeStart, CodemodeStoreState, codemode_journal_doc,
    codemode_run_doc, codemode_store_doc,
};

pub const CODEMODE_TOOL_NAME: &str = "codemode";
/// Bump when changes would alter replayed VM input, globals or outcomes.
pub const CODEMODE_MODULE_VERSION: u32 = 2;

// The sandbox crate owns source parsing but does not export the upstream Lark grammar.
pub const CODEMODE_SOURCE_GRAMMAR: &str = r"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";

#[derive(Debug, thiserror::Error)]
pub enum CodemodeError {
    #[error("{0}")]
    Source(String),
    #[error("{0}")]
    Declaration(String),
    #[error("{0}")]
    Documents(String),
    #[error("{0}")]
    Dispatch(String),
    #[error("{0}")]
    Model(String),
    #[error("The code mode sandbox was closed while the script ran")]
    Interrupted,
    #[error(transparent)]
    Aborted(#[from] AbortError),
    #[error(transparent)]
    Sandbox(#[from] HostError),
    #[error(transparent)]
    Ai(#[from] pi_rs_ai::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeCallStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

impl CodemodeCallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedCall {
    pub id: u32,
    pub name: String,
    pub args: String,
    pub status: CodemodeCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDetails {
    pub calls: Vec<CodemodeNestedCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolResult {
    pub output: Vec<InputContent>,
    pub is_error: bool,
    pub details: CodemodeToolDetails,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedResult {
    #[serde(
        default,
        deserialize_with = "state::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_output: Option<Value>,
    pub is_error: bool,
    /// The nested tool's diagnostics of severity `error`, in presentation order.
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl CodemodeNestedResult {
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            is_error: true,
            errors: vec![error.into()],
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodemodeNestedInput {
    pub name: String,
    pub arguments: Value,
}

pub struct CodemodeNestedRequest<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub arguments: &'a Value,
    model_tool: bool,
    models: Option<&'a dyn CodemodeModelProvider>,
}

impl CodemodeNestedRequest<'_> {
    /// Run an internal model tool when admitting a new nested task, not when reusing its saved result.
    /// `None` means this request is an ordinary nested tool.
    pub async fn execute_model(&self, context: &Context) -> Option<CodemodeModelToolResult> {
        if !self.model_tool {
            return None;
        }
        let result = match self.models {
            Some(models) => {
                models::execute_model_tool(models, self.name, self.arguments, context).await
            }
            None => Err(CodemodeError::Model(
                "No model provider is configured".into(),
            )),
        };
        Some(result.unwrap_or_else(|error| CodemodeModelToolResult {
            output: Vec::new(),
            nested: CodemodeNestedResult::failed(error.to_string()),
        }))
    }
}

#[async_trait]
pub trait CodemodeTools: Send + Sync {
    fn now(&self) -> u64 {
        chrono::Utc::now()
            .timestamp_millis()
            .try_into()
            .unwrap_or(0)
    }
    async fn available_tools(&self, context: &Context) -> Result<CodemodeCatalog, CodemodeError>;
    /// Inspect the previously admitted operation at a key without running it.
    async fn previous_nested_call(
        &self,
        key: &str,
        context: &Context,
    ) -> Result<Option<CodemodeNestedInput>, CodemodeError>;
    /// Attach to any nested task already at `request.key`, or make it durably visible before starting the effect.
    /// Return task and tool failures as a `CodemodeNestedResult`; `Err` indicates a failure to dispatch or reattach.
    /// For internal model tools, execute `request.execute_model(context)` and record its result in the nested task.
    async fn execute_nested(
        &self,
        request: CodemodeNestedRequest<'_>,
        context: &Context,
    ) -> Result<CodemodeNestedResult, CodemodeError>;
}

pub type CodemodeGlobalExecutor = Arc<
    dyn for<'a> Fn(
            Option<&'a Value>,
            &'a Context,
        ) -> BoxFuture<'a, Result<Option<Value>, CodemodeError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct CodemodeGlobal {
    pub name: String,
    pub spread: bool,
    pub execute: CodemodeGlobalExecutor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeToolInput {
    pub code: String,
}

#[derive(Clone)]
pub struct CodemodeToolOptions {
    pub sandbox: CodemodeSandbox,
    pub inline_budget: usize,
    pub models: bool,
    pub globals: Vec<CodemodeGlobal>,
}

impl CodemodeToolOptions {
    pub fn new(sandbox: CodemodeSandbox) -> Self {
        Self {
            sandbox,
            inline_budget: DEFAULT_CODEMODE_INLINE_BUDGET,
            models: true,
            globals: Vec::new(),
        }
    }
}

impl Default for CodemodeToolOptions {
    fn default() -> Self {
        Self::new(CodemodeSandbox::default())
    }
}

/// The host binds one run's tools, durable documents, provider and optional spill environment.
pub struct CodemodeInvocation<'a> {
    pub tools: &'a dyn CodemodeTools,
    pub documents: &'a dyn CodemodeDocuments,
    pub models: Option<&'a dyn CodemodeModelProvider>,
    pub env: Option<&'a dyn ExecutionEnv>,
}

/// Model-only and replay-safe. The host controls activation and exposure.
#[derive(Clone)]
pub struct CodemodeTool {
    options: CodemodeToolOptions,
}

impl CodemodeTool {
    pub fn output_limits(&self) -> CodemodeOutputLimits {
        OUTPUT_LIMITS
    }

    pub fn declaration(&self) -> Tool {
        Tool {
            name: CODEMODE_TOOL_NAME.into(),
            description: codemode_description(self.options.models),
            parameters: json!({"type": "object", "properties": {"code": {"type": "string", "description": "Raw JavaScript source."}}, "required": ["code"]}),
            constrained_sampling: Some(ConstrainedSampling::Enabled(
                ConstrainedSamplingConfig::Grammar {
                    variants: [("openai_lark".into(), CODEMODE_SOURCE_GRAMMAR.into())].into(),
                },
            )),
        }
    }

    pub async fn execute(
        &self,
        input: CodemodeToolInput,
        invocation: CodemodeInvocation<'_>,
        context: &Context,
    ) -> Result<CodemodeToolResult, CodemodeError> {
        run::execute(&self.options, input.code, invocation, context).await
    }
}

#[derive(Clone)]
pub struct CodemodeExtension {
    pub tool: CodemodeTool,
}

impl CodemodeExtension {
    /// Nested-only registrations; these tools are deliberately hidden from scripts' tool catalog.
    pub fn model_tools(&self) -> Option<[CodemodeToolSpec; 2]> {
        self.tool.options.models.then(create_model_tools)
    }

    pub fn section(&self, catalog: &CodemodeCatalog) -> Result<Option<String>, CodemodeError> {
        codemode_section(catalog, self.tool.options.inline_budget)
    }
}

pub fn create_codemode_tool(options: CodemodeToolOptions) -> CodemodeTool {
    CodemodeTool { options }
}

pub fn create_codemode_extension(options: CodemodeToolOptions) -> CodemodeExtension {
    CodemodeExtension {
        tool: create_codemode_tool(options),
    }
}
