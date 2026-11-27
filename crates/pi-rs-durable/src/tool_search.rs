//! Discovery of deferred tools. Hosts supply the current branch's candidates and commit requested
//! additions together with the result, rather than mutating the branch during searching.

use std::{borrow::Cow, collections::HashSet, fmt::Write};

use async_trait::async_trait;
use pi_rs_ai::Tool;
use pi_rs_chord::context::Context;
use pi_rs_schema::{Annotations, Properties, obj, opt, resolver::unescape_pointer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    search::{SearchDocument, rank_documents},
    tools::{ToolControl, ToolError, ToolResult, ToolSpec, described_num, described_str},
};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
pub const TOOL_SEARCH_EXTENSION_NAME: &str = "tool-search";
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

const DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSearchToolInput {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSearchToolDetails {
    pub loaded: Vec<String>,
}

#[async_trait]
pub trait ToolSearchHost: Send + Sync {
    /// Deferred tools eligible for discovery now; exclude already offered tools.
    async fn deferred_tools(&self, context: &Context) -> Result<Cow<'_, [Tool]>, ToolError>;
}

pub struct ToolSearchTool(ToolSpec);

pub fn create_tool_search_tool() -> ToolSearchTool {
    ToolSearchTool(ToolSpec {
        name: TOOL_SEARCH_TOOL_NAME,
        description: DESCRIPTION.into(),
        parameters: obj(&Properties::new()
            .with("query", &described_str("Search query for deferred tools."))
            .with_optional("limit", &opt(&described_num(format!("Maximum number of tools to return. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}.")))), &Annotations::new(), true),
        structured_output_schema: None,
        output_limits: None,
    })
}

pub fn create_tool_search_extension() -> crate::harness::Extension {
    let mut registration = create_tool_search_tool().spec().registration(
        true,
        std::sync::Arc::new(|args, invocation, ctx| {
            Box::pin(async move {
                let args: ToolSearchToolInput = serde_json::from_value(args)?;
                let limit =
                    validate(&args).map_err(|e| crate::session::Error::invalid(e.to_string()))?;
                let agent = invocation.agent(ctx).await?;
                let candidates: Vec<_> =
                    agent.deferred.into_iter().map(|t| t.declaration).collect();
                search(&args.query, limit, &candidates).into_execution()
            })
        }),
    );
    registration.callers = Some(vec!["model".into()]);
    crate::harness::Extension {
        name: TOOL_SEARCH_EXTENSION_NAME.into(),
        tools: vec![registration],
        ..Default::default()
    }
}

impl ToolSearchTool {
    pub fn spec(&self) -> &ToolSpec {
        &self.0
    }
    pub const fn model_only(&self) -> bool {
        true
    }
    pub const fn replay_safe(&self) -> bool {
        true
    }

    pub async fn execute(
        &self,
        args: ToolSearchToolInput,
        host: &dyn ToolSearchHost,
        context: &Context,
    ) -> Result<ToolResult<ToolSearchToolDetails>, ToolError> {
        let max = validate(&args)?;
        let candidates = host.deferred_tools(context).await?;
        Ok(search(&args.query, max, &candidates))
    }
}

fn validate(args: &ToolSearchToolInput) -> Result<usize, ToolError> {
    if args
        .query
        .trim_matches(crate::tools::is_js_space)
        .is_empty()
    {
        return Err(ToolError::Message("query must not be empty".into()));
    }
    let Some(limit) = args.limit else {
        return Ok(DEFAULT_TOOL_SEARCH_LIMIT);
    };
    if !limit.is_finite() || limit <= 0.0 || limit.fract() != 0.0 {
        return Err(ToolError::Message(
            "limit must be a positive integer".into(),
        ));
    }
    Ok(limit as usize)
}

fn pointer_segment(value: &str) -> Option<Cow<'_, str>> {
    let decoded: Cow<'_, str> = if value.contains('%') {
        let mut bytes = Vec::with_capacity(value.len());
        let mut iter = value.bytes();
        while let Some(byte) = iter.next() {
            if byte == b'%' {
                let high = (iter.next()? as char).to_digit(16)?;
                let low = (iter.next()? as char).to_digit(16)?;
                bytes.push((high * 16 + low) as u8);
            } else {
                bytes.push(byte);
            }
        }
        Cow::Owned(String::from_utf8(bytes).ok()?)
    } else {
        Cow::Borrowed(value)
    };
    Some(if decoded.contains('~') {
        Cow::Owned(unescape_pointer(&decoded))
    } else {
        decoded
    })
}

fn resolve<'a>(reference: &str, schema: &'a Value) -> Option<&'a Value> {
    if reference == "#" {
        return Some(schema);
    }
    let mut value = schema;
    for token in reference
        .strip_prefix("#/")?
        .split('/')
        .filter(|token| !token.is_empty())
    {
        value = value.as_object()?.get(pointer_segment(token)?.as_ref())?;
    }
    value.is_object().then_some(value)
}

fn append_part(text: &mut String, part: &str) {
    if part.trim_matches(crate::tools::is_js_space).is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(part);
}

fn search_text(tool: &Tool) -> String {
    enum Visit<'a> {
        Schema(&'a Value),
        Fields(&'a Value),
        Part(&'a str),
    }
    let root = &tool.parameters;
    let mut seen = HashSet::from([std::ptr::from_ref(root)]);
    let mut stack = vec![Visit::Schema(root)];
    let mut text = String::new();
    append_part(&mut text, &tool.description);
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Schema(node) if node.is_object() => {
                stack.push(Visit::Fields(node));
                if let Some(target) = node
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| resolve(r, root))
                    && seen.insert(std::ptr::from_ref(target))
                {
                    stack.push(Visit::Schema(target));
                }
            }
            Visit::Schema(_) => {}
            Visit::Fields(node) => {
                if let Some(description) = node.get("description").and_then(Value::as_str) {
                    append_part(&mut text, description);
                }
                for keyword in ["allOf", "oneOf", "anyOf"] {
                    if let Some(variants) = node.get(keyword).and_then(Value::as_array) {
                        stack.extend(variants.iter().rev().map(Visit::Schema));
                    }
                }
                if let Some(items) = node.get("items") {
                    stack.push(Visit::Schema(items));
                }
                if let Some(properties) = node.get("properties").and_then(Value::as_object) {
                    for (name, value) in properties.iter().rev() {
                        stack.push(Visit::Schema(value));
                        stack.push(Visit::Part(name));
                    }
                }
            }
            Visit::Part(part) => append_part(&mut text, part),
        }
    }
    text
}

fn search(query: &str, max: usize, candidates: &[Tool]) -> ToolResult<ToolSearchToolDetails> {
    let corpus: Vec<_> = candidates.iter().map(search_text).collect();
    let documents: Vec<_> = candidates
        .iter()
        .zip(&corpus)
        .map(|(tool, text)| SearchDocument {
            name: &tool.name,
            text,
            namespace_text: None,
        })
        .collect();
    let loaded = rank_documents(query, &documents, max);
    let (text, control) = if loaded.is_empty() {
        ("No matching tools found.".into(), None)
    } else {
        let count = loaded.len();
        let suffix = if count == 1 { "" } else { "s" };
        let mut text =
            format!("Loaded {count} tool{suffix}. They are available from your next call:");
        for name in &loaded {
            let description = candidates
                .iter()
                .find(|tool| tool.name == *name)
                .map_or("", |tool| tool.description.as_str());
            let description = description.trim_matches(crate::tools::is_js_space);
            let first_line = description.lines().next().unwrap_or("");
            let _ = write!(text, "\n- {name}: {first_line}");
        }
        (
            text,
            Some(ToolControl {
                add_tools: loaded.iter().map(|&name| name.to_owned()).collect(),
                ..ToolControl::default()
            }),
        )
    };
    ToolResult {
        output: Some(vec![pi_rs_ai::InputContent::Text(text.into())]),
        details: Some(ToolSearchToolDetails {
            loaded: loaded.into_iter().map(str::to_owned).collect(),
        }),
        control,
        ..ToolResult::default()
    }
}
