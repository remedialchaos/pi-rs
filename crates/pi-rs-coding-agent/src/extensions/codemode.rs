//! The `codemode` built-in extension: a sandboxed JavaScript tool for calling other tools.
//!
//! Upstream's implementation lives in `packages/coding-agent/src/extensions/codemode/`. The
//! execution engine itself is `pi_rs_codemode` (Wasmtime + QuickJS-WASM); this module owns the
//! extension-facing part: the tool definition, mode/budget settings, and sandbox construction
//! from the tools currently registered with the session.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use indexmap::IndexMap;
use pi_rs_agent::CancellationToken;
use pi_rs_codemode::{
    CodemodeResult, CodemodeSandbox, CodemodeTool, ExecuteOptions, HostError, OutputItem,
    SandboxOptions, ToolDeclaration,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{Extension, ExtensionRegistry, ToolExposure, ToolRegistration};
use crate::core::extensions::types::{
    ExecuteToolOptions, ExtensionHost, ExtensionToolContext, ToolDefinition,
    ToolExposure as RuntimeToolExposure, ToolOutcome,
};
use crate::core::session_manager::SessionEntry;
use crate::extensions::tool_search::{Bm25Ranker, ToolInfo, ToolSearchDocument};

pub const CODEMODE_TOOL_NAME: &str = "codemode";
pub const CODEMODE_STORE_ENTRY_TYPE: &str = "codemode-store";
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;
pub const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_MAX_CODEMODE_WORKERS: usize = 2;
pub const CODEMODE_WORKER_WAIT_MS: u64 = 60_000;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 40_000;

pub fn max_codemode_workers() -> usize {
    std::env::var("PI_CODEMODE_MAX_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CODEMODE_WORKERS)
}

#[derive(Default)]
struct WorkerPoolState {
    active: usize,
    waiters: VecDeque<oneshot::Sender<()>>,
}

struct WorkerPool {
    state: Mutex<WorkerPoolState>,
}

impl WorkerPool {
    const fn new() -> Self {
        Self {
            state: Mutex::new(WorkerPoolState {
                active: 0,
                waiters: VecDeque::new(),
            }),
        }
    }

    fn try_admit_or_queue(&self, limit: usize) -> Option<oneshot::Receiver<()>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active < limit && state.waiters.is_empty() {
            state.active += 1;
            return None;
        }
        let (tx, rx) = oneshot::channel();
        state.waiters.push_back(tx);
        Some(rx)
    }

    fn release(&self, limit: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = state.active.saturating_sub(1);
        while state.active < limit {
            let Some(tx) = state.waiters.pop_front() else {
                break;
            };
            if tx.send(()).is_ok() {
                state.active += 1;
            }
        }
    }
}

pub struct WorkerSlotGuard(&'static WorkerPool);

impl Drop for WorkerSlotGuard {
    fn drop(&mut self) {
        self.0.release(max_codemode_workers());
    }
}

static WORKER_POOL: OnceLock<WorkerPool> = OnceLock::new();

fn worker_pool() -> &'static WorkerPool {
    WORKER_POOL.get_or_init(WorkerPool::new)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodemodeWaitError {
    Aborted,
    Timeout,
}

pub async fn reserve_codemode_worker(
    signal: Option<&CancellationToken>,
    timeout_ms: u64,
) -> Result<WorkerSlotGuard, CodemodeWaitError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(CodemodeWaitError::Aborted);
    }
    let pool = worker_pool();
    let limit = max_codemode_workers();
    let Some(mut rx) = pool.try_admit_or_queue(limit) else {
        return Ok(WorkerSlotGuard(pool));
    };
    let timer = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timer);
    if let Some(signal) = signal {
        tokio::select! {
            res = &mut rx => match res {
                Ok(()) => Ok(WorkerSlotGuard(pool)),
                Err(_) => Err(CodemodeWaitError::Timeout),
            },
            () = signal.cancelled() => {
                rx.close();
                Err(CodemodeWaitError::Aborted)
            }
            () = &mut timer => {
                rx.close();
                Err(CodemodeWaitError::Timeout)
            }
        }
    } else {
        tokio::select! {
            res = &mut rx => match res {
                Ok(()) => Ok(WorkerSlotGuard(pool)),
                Err(_) => Err(CodemodeWaitError::Timeout),
            },
            () = &mut timer => {
                rx.close();
                Err(CodemodeWaitError::Timeout)
            }
        }
    }
}

/// Upstream `CodemodeMode`: scripts may be available alongside tools (`on`) or exclusively
/// (`only`). `off` is represented by deactivating the tool, not by a mode value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CodemodeMode {
    #[default]
    On,
    Only,
}

impl CodemodeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Only => "only",
        }
    }
}

pub fn parse_mode(value: &str) -> Option<CodemodeMode> {
    match value {
        "on" => Some(CodemodeMode::On),
        "only" => Some(CodemodeMode::Only),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CodemodeExtensionOptions {
    /// Overrides the `codemode.mode` setting.
    pub mode: Option<CodemodeMode>,
    /// Overrides the `codemode.inlineBudget` setting.
    pub inline_budget: Option<usize>,
    /// Expose the model catalog to scripts as `models`.
    pub models: bool,
}

/// Tool metadata needed to declare a tool inside a codemode script.
#[derive(Clone, Debug)]
pub struct CodemodeToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl CodemodeToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    pub fn codemode_tool(&self) -> CodemodeTool {
        let declaration = ToolDeclaration {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: Some(self.parameters.clone()),
            ..ToolDeclaration::default()
        };
        CodemodeTool {
            declaration,
            spread: false,
            execute: std::sync::Arc::new(|_, _| {
                Box::pin(async { Ok(Some(serde_json::Value::Null)) })
            }),
        }
    }
}

/// Build a sandbox exposing the supplied tools with a 256 MiB QuickJS heap cap.
pub fn build_sandbox(specs: &[CodemodeToolSpec]) -> Result<CodemodeSandbox, HostError> {
    CodemodeSandbox::new(SandboxOptions {
        tools: specs.iter().map(CodemodeToolSpec::codemode_tool).collect(),
        memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
        ..SandboxOptions::default()
    })
}

pub fn codemode_tool_definition() -> ToolDefinition {
    let mut def = ToolDefinition::new(
        CODEMODE_TOOL_NAME,
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Raw JavaScript source.",
                },
            },
            "required": ["code"],
            "additionalProperties": false,
        }),
    );
    def.label = "codemode".to_owned();
    def.description = codemode_description(&[], DEFAULT_CODEMODE_INLINE_BUDGET);
    def.exposure = RuntimeToolExposure::ModelOnly;
    def
}

pub fn format_codemode_output(output: &[OutputItem]) -> Value {
    let total_text = output
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { console: false, .. }))
        .count();
    let mut blocks = Vec::new();
    let mut console_lines = Vec::new();
    let mut text_index = 0_usize;
    for item in output {
        match item {
            OutputItem::Image { data, mime_type } => {
                blocks.push(json!({ "type": "image", "data": data, "mimeType": mime_type }));
            }
            OutputItem::Text {
                text,
                console: true,
            } => console_lines.push(text.clone()),
            OutputItem::Text {
                text,
                console: false,
            } => {
                text_index += 1;
                let formatted = if total_text > 1 {
                    format!("==> text {text_index}/{total_text} <==\n{text}")
                } else {
                    text.clone()
                };
                blocks.push(json!({ "type": "text", "text": formatted }));
            }
        }
    }
    if !console_lines.is_empty() {
        blocks.push(json!({
            "type": "text",
            "text": format!("<console_output>\n{}\n</console_output>", console_lines.join("\n")),
        }));
    }
    Value::Array(blocks)
}

pub fn read_codemode_store(branch: &[SessionEntry]) -> IndexMap<String, Value> {
    let mut store = IndexMap::new();
    for entry in branch {
        let SessionEntry::Custom {
            custom_type, data, ..
        } = entry
        else {
            continue;
        };
        if custom_type != CODEMODE_STORE_ENTRY_TYPE {
            continue;
        }
        let Some(data) = data.as_ref() else {
            continue;
        };
        if let Some(deleted) = data.get("delete").and_then(Value::as_array) {
            for key in deleted.iter().filter_map(Value::as_str) {
                store.shift_remove(key);
            }
        }
        if let Some(set) = data.get("set").and_then(Value::as_object) {
            for (key, value) in set {
                store.insert(key.clone(), value.clone());
            }
        }
    }
    store
}

pub fn build_live_sandbox(
    definitions: &[ToolDefinition],
    ctx: &ExtensionToolContext,
) -> Result<CodemodeSandbox, HostError> {
    let tool_infos: Vec<ToolInfo> = definitions
        .iter()
        .filter(|def| {
            def.name != CODEMODE_TOOL_NAME && !matches!(def.exposure, RuntimeToolExposure::Hidden)
        })
        .map(|def| {
            let mut info = ToolInfo::new(&def.name, &def.description, def.parameters.clone());
            if let Some(rest) = def.name.strip_prefix("mcp__")
                && let Some((ns, _)) = rest.split_once("__")
            {
                info = info.with_namespace(ns);
            }
            info
        })
        .collect();
    let tools: Vec<CodemodeTool> = tool_infos
        .iter()
        .map(|info| {
            let tool_name = info.name.clone();
            let exec_ctx = ctx.clone();
            CodemodeTool {
                declaration: ToolDeclaration {
                    name: info.name.clone(),
                    description: info.description.clone(),
                    input_schema: Some(info.parameters.clone()),
                    ..ToolDeclaration::default()
                },
                spread: false,
                execute: Arc::new(move |args, _tool_ctx| {
                    let tool_name = tool_name.clone();
                    let exec_ctx = exec_ctx.clone();
                    Box::pin(async move {
                        let outcome = exec_ctx
                            .execute_tool(
                                &tool_name,
                                args.unwrap_or_else(|| json!({})),
                                ExecuteToolOptions::default(),
                            )
                            .await;
                        if outcome.is_error {
                            let message = outcome.result["content"]
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|item| item["text"].as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .filter(|text| !text.is_empty())
                                .unwrap_or_else(|| format!("Tool {tool_name} failed"));
                            return Err(HostError(message));
                        }
                        if let Some(structured) = outcome
                            .result
                            .get("structuredContent")
                            .filter(|v| !v.is_null())
                        {
                            return Ok(Some(structured.clone()));
                        }
                        let text = outcome.result["content"]
                            .as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|item| item["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        Ok(Some(Value::String(text)))
                    })
                }),
            }
        })
        .collect();

    let catalog = Arc::new(tool_infos);
    let catalog_search = Arc::clone(&catalog);
    let search_tools = CodemodeTool::new("searchTools", move |args, _| {
        let catalog = Arc::clone(&catalog_search);
        Box::pin(async move {
            let query = args
                .as_ref()
                .and_then(|v| {
                    v.get("query")
                        .and_then(Value::as_str)
                        .or_else(|| v.as_str())
                })
                .unwrap_or("");
            let limit = args
                .as_ref()
                .and_then(|v| v["limit"].as_u64())
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(8);
            let docs: Vec<ToolSearchDocument> = catalog
                .iter()
                .map(|t| {
                    let doc = ToolSearchDocument::new(&t.name, &t.description, &t.parameters);
                    match &t.namespace {
                        Some(ns) => doc.with_namespace(ns),
                        None => doc,
                    }
                })
                .collect();
            let matches = Bm25Ranker::default().rank(query, &docs, limit);
            let names: Vec<Value> = matches.into_iter().map(|m| json!(m.name)).collect();
            Ok(Some(Value::Array(names)))
        })
    });
    let catalog_describe = Arc::clone(&catalog);
    let describe_tool = CodemodeTool::new("describeTool", move |args, _| {
        let catalog = Arc::clone(&catalog_describe);
        Box::pin(async move {
            let name = args
                .as_ref()
                .and_then(|v| v.get("name").and_then(Value::as_str).or_else(|| v.as_str()))
                .unwrap_or("");
            Ok(catalog.iter().find(|t| t.name == name).map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            }))
        })
    });
    let catalog_list = catalog;
    let list_tools = CodemodeTool::new("listTools", move |_args, _| {
        let catalog = Arc::clone(&catalog_list);
        Box::pin(async move {
            Ok(Some(Value::Array(
                catalog.iter().map(|t| json!(t.name)).collect(),
            )))
        })
    });

    CodemodeSandbox::new(SandboxOptions {
        tools,
        globals: vec![search_tools, describe_tool, list_tools],
        memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
        ..SandboxOptions::default()
    })
}

pub async fn execute_codemode(
    args: Value,
    ctx: &ExtensionToolContext,
    store: IndexMap<String, Value>,
    host: Option<&Arc<ExtensionHost>>,
) -> Result<ToolOutcome, String> {
    let Some(code) = args.get("code").and_then(Value::as_str) else {
        return Ok(ToolOutcome::error(
            &ctx.tool_call_id,
            CODEMODE_TOOL_NAME,
            "Missing required parameter \"code\".",
        ));
    };
    let _slot = match reserve_codemode_worker(None, CODEMODE_WORKER_WAIT_MS).await {
        Ok(guard) => guard,
        Err(CodemodeWaitError::Aborted) => {
            return Ok(ToolOutcome::error(
                &ctx.tool_call_id,
                CODEMODE_TOOL_NAME,
                "Script aborted while waiting for a codemode worker.",
            ));
        }
        Err(CodemodeWaitError::Timeout) => {
            return Ok(ToolOutcome::error(
                &ctx.tool_call_id,
                CODEMODE_TOOL_NAME,
                "Timed out waiting for a codemode worker after 60s.",
            ));
        }
    };
    let defs = ctx.base.host.get_all_tools();
    let sandbox = build_live_sandbox(&defs, ctx).map_err(|error| error.to_string())?;
    let result = sandbox
        .execute(
            code,
            ExecuteOptions {
                store,
                ..ExecuteOptions::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    match result {
        CodemodeResult::Success {
            output,
            calls,
            store_writes,
            ..
        } => {
            if let Some(host) = host
                && (!store_writes.set.is_empty() || !store_writes.delete.is_empty())
            {
                let _ = host.append_entry(
                    CODEMODE_STORE_ENTRY_TYPE.to_owned(),
                    Some(json!({
                        "set": store_writes.set,
                        "delete": store_writes.delete,
                    })),
                );
            }
            let mut content = format_codemode_output(&output);
            if let Some(blocks) = content.as_array_mut() {
                let total_len: usize = blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str().map(str::len))
                    .sum();
                if total_len > DEFAULT_MAX_OUTPUT_CHARS {
                    let combined = blocks
                        .iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Ok(path) = crate::utils::output_files::write_output_file(
                        "pi-codemode",
                        ".txt",
                        combined.as_bytes(),
                    ) {
                        let preview = crate::core::tools::truncate::slice_head(
                            &combined,
                            DEFAULT_MAX_OUTPUT_CHARS,
                        );
                        *blocks = vec![json!({
                            "type": "text",
                            "text": format!("{preview}\n\n[Output truncated; full output written to {}]", path.display()),
                        })];
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "Script completed." }));
                }
            }
            Ok(ToolOutcome::success(
                json!({ "type": "toolCall", "id": ctx.tool_call_id, "name": CODEMODE_TOOL_NAME, "arguments": args }),
                json!({
                    "content": content,
                    "details": { "calls": calls },
                }),
            ))
        }
        CodemodeResult::Failure { error, output, .. } => {
            let mut blocks = format_codemode_output(&output)
                .as_array()
                .cloned()
                .unwrap_or_default();
            blocks.push(json!({
                "type": "text",
                "text": format!("Script error: {}", error.message),
            }));
            Ok(ToolOutcome {
                tool_call: json!({ "type": "toolCall", "id": ctx.tool_call_id, "name": CODEMODE_TOOL_NAME, "arguments": args }),
                result: json!({ "content": blocks, "details": {} }),
                is_error: true,
                nested_calls: Vec::new(),
            })
        }
    }
}

/// Description of the codemode tool. Tool declarations are capped by `inline_budget` characters;
/// the remaining tools stay discoverable through `searchTools()` inside a script.
pub fn codemode_description(tools: &[ToolInfo], inline_budget: usize) -> String {
    let mut lines = vec![
        "# Code mode".to_string(),
        String::new(),
        "Write an async JavaScript script and call tools with `tools.<name>(args)`.".to_string(),
        "Use `searchTools(query)` to discover tools that are not listed below.".to_string(),
    ];
    let mut used = lines.iter().map(String::len).sum::<usize>();
    for tool in tools {
        let line = format!(
            "- `{}`: {}",
            tool.name,
            tool.description.lines().next().unwrap_or("")
        );
        if used + line.len() + 1 > inline_budget {
            break;
        }
        used += line.len() + 1;
        lines.push(line);
    }
    lines.join("\n")
}

/// The built-in `codemode` extension. Registered inactive: MCP and `defaultTools` activate it.
#[derive(Clone, Debug, Default)]
pub struct CodemodeExtension {
    options: CodemodeExtensionOptions,
}

impl CodemodeExtension {
    pub fn with_options(options: CodemodeExtensionOptions) -> Self {
        Self { options }
    }

    pub fn mode(&self) -> CodemodeMode {
        self.options.mode.unwrap_or_default()
    }

    pub fn inline_budget(&self) -> usize {
        self.options
            .inline_budget
            .unwrap_or(DEFAULT_CODEMODE_INLINE_BUDGET)
    }
}

impl Extension for CodemodeExtension {
    fn name(&self) -> &'static str {
        "codemode"
    }

    fn register(&self, registry: &mut ExtensionRegistry) {
        registry.register_tool_with(ToolRegistration {
            name: CODEMODE_TOOL_NAME.to_string(),
            default_active: false,
            exposure: ToolExposure::Codemode,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mode_parsing_matches_upstream_values() {
        assert_eq!(parse_mode("on"), Some(CodemodeMode::On));
        assert_eq!(parse_mode("only"), Some(CodemodeMode::Only));
        assert_eq!(parse_mode("off"), None);
    }

    #[test]
    fn extension_defaults_match_settings() {
        let extension = CodemodeExtension::default();
        assert_eq!(extension.mode(), CodemodeMode::On);
        assert_eq!(extension.inline_budget(), 3000);
        assert_eq!(CodemodeMode::Only.as_str(), "only");
    }

    #[test]
    fn extension_registers_inactive_codemode_tool() {
        let mut registry = ExtensionRegistry::default();
        CodemodeExtension::default().register(&mut registry);
        assert_eq!(registry.tools.len(), 1);
        assert_eq!(registry.tools[0].name, CODEMODE_TOOL_NAME);
        assert!(!registry.tools[0].default_active);
        assert_eq!(registry.tools[0].exposure, ToolExposure::Codemode);
    }

    #[test]
    fn tool_specs_are_declared_to_the_sandbox() {
        let sandbox = build_sandbox(&[CodemodeToolSpec::new(
            "echo",
            "Echo a value",
            json!({"type": "object", "properties": {"value": {"type": "string"}}}),
        )])
        .expect("sandbox with one host tool");
        let tools = sandbox.tools().expect("tool list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "echo");
        assert_eq!(
            tools[0].declaration.input_schema,
            Some(json!({"type": "object", "properties": {"value": {"type": "string"}}}))
        );
    }

    #[tokio::test]
    async fn sandbox_executes_offline_script() -> Result<(), HostError> {
        let sandbox = build_sandbox(&[])?;
        let result = sandbox
            .execute("return 1 + 1", pi_rs_codemode::ExecuteOptions::default())
            .await?;
        assert_eq!(result.value(), Some(&json!(2)));
        Ok(())
    }

    #[test]
    fn description_respects_inline_budget() {
        let tools = vec![
            ToolInfo::new("alpha", "First tool", json!({})),
            ToolInfo::new("beta", "Second tool", json!({})),
        ];
        let full = codemode_description(&tools, 10_000);
        assert!(full.contains("alpha"));
        assert!(full.contains("beta"));
        let short = codemode_description(&tools, 0);
        assert!(!short.contains("alpha"));
    }

    #[test]
    fn format_codemode_output_labels_blocks_and_console_lines() {
        let items = vec![
            OutputItem::Text {
                text: "first".to_owned(),
                console: false,
            },
            OutputItem::Text {
                text: "second".to_owned(),
                console: false,
            },
            OutputItem::Text {
                text: "log-1".to_owned(),
                console: true,
            },
        ];
        let blocks = format_codemode_output(&items);
        let texts: Vec<&str> = blocks
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["text"].as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "==> text 1/2 <==\nfirst",
                "==> text 2/2 <==\nsecond",
                "<console_output>\nlog-1\n</console_output>",
            ]
        );
    }

    #[test]
    fn read_codemode_store_replays_set_and_delete_entries() {
        let entries = vec![
            SessionEntry::Custom {
                id: "1".into(),
                parent_id: None,
                timestamp: "2026-01-01T00:00:00Z".into(),
                custom_type: CODEMODE_STORE_ENTRY_TYPE.into(),
                data: Some(json!({ "set": { "a": 1, "b": 2 } })),
            },
            SessionEntry::Custom {
                id: "2".into(),
                parent_id: Some("1".into()),
                timestamp: "2026-01-01T00:00:01Z".into(),
                custom_type: CODEMODE_STORE_ENTRY_TYPE.into(),
                data: Some(json!({ "delete": ["a"], "set": { "c": "three" } })),
            },
        ];
        let store = read_codemode_store(&entries);
        assert_eq!(store.get("a"), None);
        assert_eq!(store.get("b"), Some(&json!(2)));
        assert_eq!(store.get("c"), Some(&json!("three")));
    }
}
