//! The `codemode` built-in extension: a sandboxed JavaScript tool for calling other tools.
//!
//! Upstream's implementation lives in `packages/coding-agent/src/extensions/codemode/`. The
//! execution engine itself is `pi_rs_codemode` (Wasmtime + QuickJS-WASM); this module owns the
//! extension-facing part: the tool definition, mode/budget settings, and sandbox construction
//! from the tools currently registered with the session.

use pi_rs_codemode::{CodemodeSandbox, CodemodeTool, HostError, SandboxOptions, ToolDeclaration};
use serde_json::Value;

use super::{Extension, ExtensionRegistry, ToolExposure, ToolRegistration};
use crate::extensions::tool_search::ToolInfo;

pub const CODEMODE_TOOL_NAME: &str = "codemode";
pub const CODEMODE_STORE_ENTRY_TYPE: &str = "codemode-store";
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;

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

/// Build a sandbox exposing exactly the supplied tools. The session replaces the stub executor
/// with the real tool dispatcher when it binds tools to a run.
pub fn build_sandbox(specs: &[CodemodeToolSpec]) -> Result<CodemodeSandbox, HostError> {
    CodemodeSandbox::new(SandboxOptions {
        tools: specs.iter().map(CodemodeToolSpec::codemode_tool).collect(),
        ..SandboxOptions::default()
    })
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
}
