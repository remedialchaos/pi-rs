//! `AgentTool` wrappers for the eight built-in tools.
//!
//! Each wrapper owns a `pi_rs_ai::Tool` declaration whose parameters are the exact JSON Schema
//! emitted by upstream's TypeBox definitions. Execution is delegated to the pure executors in this
//! directory; this file only adapts arguments, cancellation, and result content blocks. Ported from
//! `packages/coding-agent/src/core/tools/{read,bash,powershell,edit,write,grep,find,ls}.ts`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::Engine;
use futures_util::future::BoxFuture;
use pi_rs_agent::{
    AgentError, AgentTool, AgentToolResult, AgentToolUpdateCallback, CancellationToken, Result,
};
use pi_rs_ai::{JsonObject, Tool};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::tool_definition_wrapper::{
    build_tool, details_object, error_result, image_result, text_result, text_result_with_details,
    truncation_value,
};

const READ_DESCRIPTION: &str = "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";
const EDIT_DESCRIPTION: &str = "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.";
const WRITE_DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";
const GREP_DESCRIPTION: &str = "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.";
const FIND_DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).";
const LS_DESCRIPTION: &str = "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first).";

fn shell_description(shell_name: &str) -> String {
    format!(
        "Execute a {shell_name} command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
    )
}

fn read_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
            "offset": {"type": "number", "description": "Line number to start reading from (1-indexed)"},
            "limit": {"type": "number", "description": "Maximum number of lines to read"}
        },
        "required": ["path"]
    })
}

fn shell_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Shell command to execute"},
            "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
        },
        "required": ["command"]
    })
}

fn edit_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
            "edits": {
                "type": "array",
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": {
                            "type": "string",
                            "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                        },
                        "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
                    },
                    "required": ["oldText", "newText"]
                }
            }
        },
        "required": ["path", "edits"]
    })
}

fn write_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
            "content": {"type": "string", "description": "Content to write to the file"}
        },
        "required": ["path", "content"]
    })
}

fn grep_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
            "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
            "glob": {
                "type": "string",
                "description": "Filter files by glob pattern relative to path, e.g. 'src/**/*.ts'. Without '/', e.g. '*.ts', it matches file names at any depth"
            },
            "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
            "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
            "context": {"type": "number", "description": "Number of lines to show before and after each match (default: 0)"},
            "limit": {"type": "number", "description": "Maximum number of matches to return (default: 100)"}
        },
        "required": ["pattern"]
    })
}

fn find_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern relative to the search directory, e.g. 'src/**/*.ts'. Without '/', e.g. '*.ts', it matches file names at any depth"
            },
            "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
            "limit": {"type": "number", "description": "Maximum number of results (default: 1000)"}
        },
        "required": ["pattern"]
    })
}

fn ls_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory to list (default: current directory)"},
            "limit": {"type": "number", "description": "Maximum number of entries to return (default: 500)"}
        }
    })
}

fn aborted() -> AgentError {
    AgentError::Callback("Operation aborted".to_string())
}

fn check_cancel(signal: &CancellationToken) -> Result<()> {
    if signal.is_cancelled() {
        Err(aborted())
    } else {
        Ok(())
    }
}

fn parse<T: DeserializeOwned>(params: &Value) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|error| AgentError::Callback(error.to_string()))
}

fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
}

/// Upstream `prepareEditArguments`: accept a JSON string or a single edit object, fold legacy
/// top-level `oldText`/`newText` into `edits`, and remove the now-redundant legacy fields.
fn prepare_edit_args(args: &mut JsonObject) {
    match args.get("edits").cloned() {
        Some(Value::String(text)) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                if parsed.is_array() {
                    args.insert("edits".to_string(), parsed);
                } else if is_single_edit(&parsed) {
                    args.insert("edits".to_string(), json!([parsed]));
                }
            }
        }
        Some(value) if value.is_object() && is_single_edit(&value) => {
            args.insert("edits".to_string(), json!([value]));
        }
        _ => {}
    }

    let legacy = (
        args.get("oldText")
            .and_then(Value::as_str)
            .map(str::to_string),
        args.get("newText")
            .and_then(Value::as_str)
            .map(str::to_string),
    );
    if let (Some(old_text), Some(new_text)) = legacy {
        let mut edits = match args.remove("edits") {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        edits.push(json!({"oldText": old_text, "newText": new_text}));
        args.remove("oldText");
        args.remove("newText");
        args.insert("edits".to_string(), Value::Array(edits));
    }
}

fn read_details(details: &super::read::ReadDetails) -> Value {
    details_object([
        (
            "truncation",
            details.truncation.as_ref().map(truncation_value),
        ),
        (
            "imageUnsupportedByModel",
            details
                .image_unsupported_by_model
                .as_ref()
                .map(|id| json!(id)),
        ),
    ])
}

fn bash_details(details: &super::bash::BashDetails) -> Value {
    details_object([
        (
            "truncation",
            details.truncation.as_ref().map(truncation_value),
        ),
        (
            "fullOutputPath",
            details.full_output_path.as_ref().map(|path| json!(path)),
        ),
        (
            "fullOutputError",
            details.full_output_error.as_ref().map(|err| json!(err)),
        ),
    ])
}

fn edit_details(details: &super::edit::EditDetails) -> Value {
    details_object([
        ("diff", Some(json!(details.diff))),
        ("patch", Some(json!(details.patch))),
        (
            "firstChangedLine",
            details.first_changed_line.map(|line| json!(line)),
        ),
    ])
}

fn grep_details(details: &super::grep::GrepDetails) -> Value {
    details_object([
        (
            "truncation",
            details.truncation.as_ref().map(truncation_value),
        ),
        (
            "matchLimitReached",
            details.match_limit_reached.map(|limit| json!(limit)),
        ),
        (
            "linesTruncated",
            details.lines_truncated.then_some(json!(true)),
        ),
    ])
}

fn find_details(details: &super::find::FindDetails) -> Value {
    details_object([
        (
            "truncation",
            details.truncation.as_ref().map(truncation_value),
        ),
        (
            "resultLimitReached",
            details.result_limit_reached.map(|limit| json!(limit)),
        ),
    ])
}

fn ls_details(details: &super::ls::LsDetails) -> Value {
    details_object([
        (
            "truncation",
            details.truncation.as_ref().map(truncation_value),
        ),
        (
            "entryLimitReached",
            details.entry_limit_reached.map(|limit| json!(limit)),
        ),
    ])
}

/// Read tool: text files go through the pure executor; image files are detected by magic bytes,
/// converted to PNG when needed, and resized with `utils::image_resize`.
pub struct ReadTool {
    cwd: PathBuf,
    auto_resize_images: bool,
    declaration: Tool,
}

impl ReadTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            auto_resize_images: true,
            declaration: build_tool("read", READ_DESCRIPTION, read_parameters(), true),
        }
    }

    pub fn with_auto_resize_images(mut self, auto_resize_images: bool) -> Self {
        self.auto_resize_images = auto_resize_images;
        self
    }
}

impl AgentTool for ReadTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::read::ReadInput = parse(params)?;
            let absolute = super::path_utils::resolve_read_path(&input.path, &self.cwd);

            if let Some(mime_type) = super::read::detect_image_mime_type(&absolute) {
                let bytes = std::fs::read(&absolute)
                    .map_err(|error| AgentError::Callback(error.to_string()))?;
                let (bytes, mime_type, conversion_hint) = if mime_type == "image/bmp" {
                    let png = crate::utils::image_convert::convert_bytes_to_png(&bytes)
                        .map_err(AgentError::Callback)?;
                    (
                        png,
                        "image/png".to_string(),
                        Some("[Image converted from image/bmp to image/png.]".to_string()),
                    )
                } else {
                    (bytes, mime_type, None)
                };
                if signal.is_cancelled() {
                    return Err(aborted());
                }

                let mut hints = Vec::new();
                let (data, final_mime) = if self.auto_resize_images {
                    match crate::utils::image_resize::resize_image(
                        &bytes,
                        &mime_type,
                        crate::utils::image_resize::ImageResizeOptions::default(),
                    ) {
                        Ok(Some(resized)) => {
                            if let Some(note) =
                                crate::utils::image_resize::format_dimension_note(&resized)
                            {
                                hints.push(note);
                            }
                            (resized.data, resized.mime_type)
                        }
                        Ok(None) => {
                            let note = format!(
                                "Read image file [{mime_type}]\n[Image omitted: could not be resized below the inline image size limit.]"
                            );
                            return Ok(text_result(note));
                        }
                        Err(error) => return Err(AgentError::Callback(error)),
                    }
                } else {
                    (
                        base64::engine::general_purpose::STANDARD.encode(&bytes),
                        mime_type.clone(),
                    )
                };
                if let Some(hint) = conversion_hint {
                    hints.insert(0, hint);
                }

                let mut note = format!("Read image file [{final_mime}]");
                if !hints.is_empty() {
                    note.push('\n');
                    note.push_str(&hints.join("\n"));
                }
                check_cancel(signal)?;
                return Ok(image_result(note, data, final_mime));
            }

            let execution =
                super::read::execute_read(&input, &self.cwd).map_err(AgentError::Callback)?;
            check_cancel(signal)?;
            let mut result = match execution.output {
                super::read::ReadOutput::Text(text) => AgentToolResult {
                    structured_content: Some(Value::String(text.clone())),
                    ..text_result(text)
                },
                super::read::ReadOutput::Image {
                    data,
                    mime_type,
                    note,
                } => image_result(note, data, mime_type),
            };
            if let Some(details) = execution.details {
                result.details = Some(read_details(&details));
            }
            Ok(result)
        })
    }
}

fn shell_structured_output(output: &super::bash::BashOutput) -> Value {
    let mut value = json!({
        "output": output.output,
        "truncated": output.truncated,
        "exit_code": output.exit_code,
        "wall_time_seconds": output.wall_time_seconds,
    });
    if let Some(path) = &output.full_output_path {
        value["full_output_path"] = json!(path);
    }
    if let Some(error) = &output.full_output_error {
        value["full_output_error"] = json!(error);
    }
    value
}

fn shell_result(result: super::bash::BashResult) -> AgentToolResult {
    let details = result.details.as_ref().map(bash_details);
    let structured = Some(shell_structured_output(&result.structured));
    if result.is_error {
        error_result(result.text, details, structured)
    } else {
        AgentToolResult {
            structured_content: structured,
            ..text_result_with_details(result.text, details)
        }
    }
}

fn bash_declaration() -> Tool {
    build_tool("bash", &shell_description("bash"), shell_parameters(), true)
}

fn powershell_declaration() -> Tool {
    build_tool(
        "powershell",
        &shell_description("PowerShell"),
        shell_parameters(),
        true,
    )
}

fn shell_chunk_callback(on_update: AgentToolUpdateCallback) -> super::bash::BashChunkCallback {
    let state = Arc::new(Mutex::new((String::new(), Option::<Instant>::None)));
    Arc::new(move |chunk: &str| {
        if chunk.is_empty() {
            return;
        }
        let snapshot_text = {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.0.push_str(chunk);
            let now = Instant::now();
            let should_emit = guard
                .1
                .is_none_or(|last| now.duration_since(last).as_millis() >= 100);
            if !should_emit {
                return;
            }
            guard.1 = Some(now);
            let truncated = super::truncate::truncate_tail(
                &guard.0,
                &super::truncate::TruncationOptions::default(),
            );
            truncated.content
        };
        on_update(text_result(snapshot_text));
    })
}

/// Bash tool: `/bin/bash -c <command>` on Unix (or `sh` when bash is absent).
pub struct BashTool {
    cwd: PathBuf,
    declaration: Tool,
}

impl BashTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: bash_declaration(),
        }
    }
}

impl AgentTool for BashTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::bash::BashInput = parse(params)?;
            on_update(AgentToolResult::default());
            let chunk_cb = shell_chunk_callback(on_update);
            let result =
                super::bash::execute_bash_with(&input, &self.cwd, Some(signal), Some(chunk_cb))
                    .await
                    .map_err(AgentError::Callback)?;
            check_cancel(signal)?;
            Ok(shell_result(result))
        })
    }
}

/// PowerShell tool: available on Windows only, upstream's behavior.
pub struct PowerShellTool {
    cwd: PathBuf,
    declaration: Tool,
}

impl PowerShellTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: powershell_declaration(),
        }
    }
}

impl AgentTool for PowerShellTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::powershell::PowerShellInput = parse(params)?;
            on_update(AgentToolResult::default());
            let chunk_cb = shell_chunk_callback(on_update);
            let result = super::powershell::execute_powershell_with(
                &input,
                &self.cwd,
                Some(signal),
                Some(chunk_cb),
            )
            .await
            .map_err(AgentError::Callback)?;
            check_cancel(signal)?;
            Ok(shell_result(result))
        })
    }
}

/// Edit tool: exact text replacement, with legacy argument coercion.
pub struct EditTool {
    cwd: PathBuf,
    declaration: Tool,
    queue: Arc<super::file_mutation_queue::FileMutationQueue>,
}

impl EditTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_queue(
            cwd,
            super::file_mutation_queue::shared_file_mutation_queue(),
        )
    }

    pub fn with_queue(
        cwd: impl Into<PathBuf>,
        queue: Arc<super::file_mutation_queue::FileMutationQueue>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: build_tool("edit", EDIT_DESCRIPTION, edit_parameters(), true),
            queue,
        }
    }
}

impl AgentTool for EditTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input =
                super::edit::prepare_edit_arguments(params).map_err(AgentError::Callback)?;
            let path = input.path.clone();
            let cwd = self.cwd.clone();
            let execution = self
                .queue
                .run_with(&path, &self.cwd, AgentError::Callback, || async move {
                    check_cancel(signal)?;
                    tokio::task::spawn_blocking(move || super::edit::execute_edit(&input, &cwd))
                        .await
                        .map_err(|error| AgentError::Callback(error.to_string()))?
                        .map_err(AgentError::Callback)
                })
                .await?;
            check_cancel(signal)?;
            Ok(text_result_with_details(
                execution.output,
                Some(edit_details(&execution.details)),
            ))
        })
    }

    fn prepare_arguments(&self, args: &mut JsonObject) -> Result<()> {
        prepare_edit_args(args);
        Ok(())
    }
}

/// Write tool: create or overwrite a file and its parent directories.
pub struct WriteTool {
    cwd: PathBuf,
    declaration: Tool,
    queue: Arc<super::file_mutation_queue::FileMutationQueue>,
}

impl WriteTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_queue(
            cwd,
            super::file_mutation_queue::shared_file_mutation_queue(),
        )
    }

    pub fn with_queue(
        cwd: impl Into<PathBuf>,
        queue: Arc<super::file_mutation_queue::FileMutationQueue>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: build_tool("write", WRITE_DESCRIPTION, write_parameters(), true),
            queue,
        }
    }
}

impl AgentTool for WriteTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::write::WriteInput = parse(params)?;
            let path = input.path.clone();
            let cwd = self.cwd.clone();
            let text = self
                .queue
                .run_with(&path, &self.cwd, AgentError::Callback, || async move {
                    check_cancel(signal)?;
                    super::write::write_file(&input, &cwd)
                        .await
                        .map_err(AgentError::Callback)
                })
                .await?;
            check_cancel(signal)?;
            Ok(text_result(text))
        })
    }
}

/// Grep tool: content search through `rg --json`.
pub struct GrepTool {
    cwd: PathBuf,
    declaration: Tool,
}

impl GrepTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: build_tool("grep", GREP_DESCRIPTION, grep_parameters(), false),
        }
    }
}

impl AgentTool for GrepTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::grep::GrepInput = parse(params)?;
            // `execute_grep` holds `&dyn GrepOperations` across an await, which makes its future
            // !Send. Drive it on a current-thread Tokio runtime in a helper thread so the tool's
            // own future stays Send as the `AgentTool` trait requires.
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let cwd = self.cwd.clone();
            std::thread::spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime
                        .block_on(super::grep::execute_grep(
                            &input,
                            &cwd,
                            &super::grep::LocalGrepOperations,
                        ))
                        .map_err(AgentError::Callback),
                    Err(error) => Err(AgentError::Callback(error.to_string())),
                };
                let _ = sender.send(result);
            });
            let execution = receiver
                .await
                .map_err(|error| AgentError::Callback(error.to_string()))??;
            check_cancel(signal)?;
            Ok(text_result_with_details(
                execution.output,
                execution.details.as_ref().map(grep_details),
            ))
        })
    }
}

/// Find tool: glob-based file search.
pub struct FindTool {
    cwd: PathBuf,
    declaration: Tool,
}

impl FindTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: build_tool("find", FIND_DESCRIPTION, find_parameters(), false),
        }
    }
}

impl AgentTool for FindTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::find::FindInput = parse(params)?;
            let execution =
                super::find::execute_find(&input, &self.cwd, &super::find::LocalFindOperations)
                    .map_err(AgentError::Callback)?;
            check_cancel(signal)?;
            Ok(text_result_with_details(
                execution.output,
                execution.details.as_ref().map(find_details),
            ))
        })
    }
}

/// LS tool: directory listing.
pub struct LsTool {
    cwd: PathBuf,
    declaration: Tool,
}

impl LsTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            declaration: build_tool("ls", LS_DESCRIPTION, ls_parameters(), false),
        }
    }
}

impl AgentTool for LsTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        &self.declaration.name
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        params: &'a Value,
        signal: &'a CancellationToken,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFuture<'a, Result<AgentToolResult>> {
        Box::pin(async move {
            check_cancel(signal)?;
            let input: super::ls::LsInput = parse(params)?;
            let execution =
                super::ls::execute_ls(&input, &self.cwd).map_err(AgentError::Callback)?;
            check_cancel(signal)?;
            Ok(text_result_with_details(
                execution.output,
                execution.details.as_ref().map(ls_details),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn noop_update(_: AgentToolResult) {}

    fn callback() -> AgentToolUpdateCallback {
        Arc::new(noop_update)
    }

    fn text_of(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                pi_rs_ai::InputContent::Text(text) => Some(text.text.as_str()),
                pi_rs_ai::InputContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn declarations_match_upstream_names_labels_and_required_fields() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let tools: Vec<Box<dyn AgentTool>> = vec![
            Box::new(ReadTool::new(dir.path())),
            Box::new(BashTool::new(dir.path())),
            Box::new(PowerShellTool::new(dir.path())),
            Box::new(EditTool::new(dir.path())),
            Box::new(WriteTool::new(dir.path())),
            Box::new(GrepTool::new(dir.path())),
            Box::new(FindTool::new(dir.path())),
            Box::new(LsTool::new(dir.path())),
        ];
        let expected: [(&str, Value); 8] = [
            ("read", json!(["path"])),
            ("bash", json!(["command"])),
            ("powershell", json!(["command"])),
            ("edit", json!(["path", "edits"])),
            ("write", json!(["path", "content"])),
            ("grep", json!(["pattern"])),
            ("find", json!(["pattern"])),
            ("ls", Value::Null),
        ];
        for (tool, (name, required)) in tools.iter().zip(expected) {
            assert_eq!(tool.declaration().name, name);
            assert_eq!(tool.label(), name);
            assert_eq!(tool.declaration().parameters["required"], required);
            assert_eq!(tool.declaration().parameters["type"], json!("object"));
        }
    }

    #[test]
    fn strict_sampling_only_on_five_builtins() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let strict: [(Box<dyn AgentTool>, bool); 8] = [
            (Box::new(ReadTool::new(dir.path())), true),
            (Box::new(BashTool::new(dir.path())), true),
            (Box::new(PowerShellTool::new(dir.path())), true),
            (Box::new(EditTool::new(dir.path())), true),
            (Box::new(WriteTool::new(dir.path())), true),
            (Box::new(GrepTool::new(dir.path())), false),
            (Box::new(FindTool::new(dir.path())), false),
            (Box::new(LsTool::new(dir.path())), false),
        ];
        for (tool, is_strict) in strict {
            assert_eq!(tool.declaration().constrained_sampling.is_some(), is_strict);
        }
    }

    #[test]
    fn edit_prepare_arguments_folds_legacy_fields_and_string_edits() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let tool = EditTool::new(dir.path());
        let mut args = serde_json::from_value(json!({
            "path": "f.txt",
            "oldText": "before",
            "newText": "after"
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        tool.prepare_arguments(&mut args)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            args,
            serde_json::from_value(json!({
                "path": "f.txt",
                "edits": [{"oldText": "before", "newText": "after"}]
            }))
            .unwrap_or_else(|e| panic!("{e}"))
        );

        let mut string_edits = serde_json::from_value(json!({
            "path": "f.txt",
            "edits": "[{\"oldText\": \"a\", \"newText\": \"b\"}]"
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        tool.prepare_arguments(&mut string_edits)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(string_edits["edits"][0]["oldText"], json!("a"));
    }

    #[test]
    fn edit_prepare_arguments_wraps_single_edit_and_appends_to_existing() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let tool = EditTool::new(dir.path());
        let mut single = serde_json::from_value(json!({
            "path": "f.txt",
            "edits": {"oldText": "a", "newText": "b"}
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        tool.prepare_arguments(&mut single)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(single["edits"].as_array().map(Vec::len), Some(1));

        let mut both = serde_json::from_value(json!({
            "path": "f.txt",
            "edits": [{"oldText": "a", "newText": "b"}],
            "oldText": "c",
            "newText": "d"
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        tool.prepare_arguments(&mut both)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(both["edits"].as_array().map(Vec::len), Some(2));
        assert!(both.get("oldText").is_none());
    }

    async fn execute_tool(
        tool: &dyn AgentTool,
        params: Value,
        signal: &CancellationToken,
    ) -> Result<AgentToolResult> {
        tool.execute("call", &params, signal, callback()).await
    }

    #[tokio::test]
    async fn read_tool_reads_text() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "hello\nworld")?;
        let tool = ReadTool::new(dir.path());
        let signal = CancellationToken::new();
        let result = execute_tool(&tool, json!({"path": "f.txt"}), &signal)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(text_of(&result), "hello\nworld");
        assert_eq!(
            result.structured_content,
            Some(Value::String("hello\nworld".into()))
        );
        Ok(())
    }

    #[tokio::test]
    async fn read_tool_reports_missing_file_as_callback_error() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = ReadTool::new(dir.path());
        let signal = CancellationToken::new();
        let error = match execute_tool(&tool, json!({"path": "missing.txt"}), &signal).await {
            Ok(_) => return Err(std::io::Error::other("expected error")),
            Err(error) => error,
        };
        assert!(error.to_string().contains("ENOENT"));
        Ok(())
    }

    #[tokio::test]
    async fn read_tool_returns_png_image_block() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(dir.path().join("image.dat"), &bytes)?;
        let tool = ReadTool::new(dir.path());
        let signal = CancellationToken::new();
        let result = execute_tool(&tool, json!({"path": "image.dat"}), &signal)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert!(text_of(&result).starts_with("Read image file [image/png]"));
        assert!(
            result
                .content
                .iter()
                .any(|block| matches!(block, pi_rs_ai::InputContent::Image(_)))
        );
        assert!(result.structured_content.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn read_tool_converts_bmp_to_png() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Bmp,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(dir.path().join("picture.bmp"), &bytes)?;
        let tool = ReadTool::new(dir.path());
        let signal = CancellationToken::new();
        let result = execute_tool(&tool, json!({"path": "picture.bmp"}), &signal)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert!(text_of(&result).contains("[Image converted from image/bmp to image/png.]"));
        Ok(())
    }

    #[tokio::test]
    async fn write_tool_creates_file() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = WriteTool::new(dir.path());
        let signal = CancellationToken::new();
        let result = execute_tool(
            &tool,
            json!({"path": "nested/f.txt", "content": "data"}),
            &signal,
        )
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(text_of(&result), "Successfully wrote to nested/f.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/f.txt"))?,
            "data"
        );
        Ok(())
    }

    #[tokio::test]
    async fn edit_tool_applies_legacy_arguments() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "before\n")?;
        let tool = EditTool::new(dir.path());
        let signal = CancellationToken::new();
        let result = execute_tool(
            &tool,
            json!({"path": "f.txt", "oldText": "before", "newText": "after"}),
            &signal,
        )
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(
            text_of(&result),
            "Successfully replaced 1 block(s) in f.txt."
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt"))?,
            "after\n"
        );
        assert!(
            result
                .details
                .and_then(|d| d.get("patch").cloned())
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn bash_tool_captures_stdout_and_marks_nonzero_exit() -> std::io::Result<()> {
        if !PathBuf::from("/bin/sh").exists() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let tool = BashTool::new(dir.path());
        let signal = CancellationToken::new();
        let ok = tool
            .execute("call", &json!({"command": "echo hi"}), &signal, callback())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(text_of(&ok).trim(), "hi");
        assert_eq!(
            ok.structured_content
                .and_then(|v| v.get("exit_code").cloned()),
            Some(json!(0))
        );

        let failed = tool
            .execute("call", &json!({"command": "exit 3"}), &signal, callback())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            failed
                .structured_content
                .and_then(|v| v.get("exit_code").cloned()),
            Some(json!(3))
        );
        Ok(())
    }

    #[tokio::test]
    async fn powershell_tool_is_unavailable_off_windows() -> std::io::Result<()> {
        if cfg!(windows) {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let tool = PowerShellTool::new(dir.path());
        let signal = CancellationToken::new();
        let error = match tool
            .execute("call", &json!({"command": "echo hi"}), &signal, callback())
            .await
        {
            Ok(_) => return Err(std::io::Error::other("expected powershell error")),
            Err(error) => error,
        };
        assert!(error.to_string().contains("only available on Windows"));
        Ok(())
    }

    #[tokio::test]
    async fn find_and_ls_tools_list_files() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("a.txt"), "")?;
        std::fs::create_dir(dir.path().join("sub"))?;
        let signal = CancellationToken::new();

        let find = FindTool::new(dir.path());
        let found = find
            .execute("call", &json!({"pattern": "*.txt"}), &signal, callback())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(text_of(&found), "a.txt");

        let ls = LsTool::new(dir.path());
        let listed = ls
            .execute("call", &json!({}), &signal, callback())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert_eq!(text_of(&listed), "a.txt\nsub/");
        Ok(())
    }

    #[tokio::test]
    async fn grep_tool_finds_match_when_rg_present() -> std::io::Result<()> {
        if !super::super::grep::rg_available() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "needle here\n")?;
        let tool = GrepTool::new(dir.path());
        let signal = CancellationToken::new();
        let result = tool
            .execute("call", &json!({"pattern": "needle"}), &signal, callback())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert!(text_of(&result).contains("f.txt:1: needle here"));
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_signal_is_rejected_before_execution() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = WriteTool::new(dir.path());
        let token = CancellationToken::new();
        token.cancel();
        let error = match execute_tool(&tool, json!({"path": "x", "content": "y"}), &token).await {
            Ok(_) => return Err(std::io::Error::other("expected cancellation error")),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "Operation aborted");
        Ok(())
    }

    #[tokio::test]
    async fn bash_tool_streams_incremental_updates_and_cancels_sleeping_process()
    -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let tool = BashTool::new(dir.path());
        let signal = CancellationToken::new();
        let chunks = Arc::new(Mutex::new(Vec::<String>::new()));
        let chunks_ref = Arc::clone(&chunks);
        let on_update: AgentToolUpdateCallback = Arc::new(move |partial| {
            let text = partial
                .content
                .iter()
                .filter_map(|c| match c {
                    pi_rs_ai::InputContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            chunks_ref
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(text);
        });
        let result = tool
            .execute(
                "call-stream",
                &json!({"command": "printf 'step1\\n'; sleep 0.05; printf 'step2\\n'"}),
                &signal,
                on_update,
            )
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        assert!(text_of(&result).contains("step2"));
        assert!(!chunks.lock().unwrap_or_else(|e| e.into_inner()).is_empty());

        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            cancel_trigger.cancel();
        });
        let err = match tool
            .execute(
                "call-abort",
                &json!({"command": "sleep 10"}),
                &cancel,
                callback(),
            )
            .await
        {
            Ok(_) => return Err(std::io::Error::other("expected aborted error")),
            Err(error) => error,
        };
        assert!(err.to_string().contains("Command aborted"), "{err}");
        Ok(())
    }
}
