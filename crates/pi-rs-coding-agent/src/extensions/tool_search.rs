//! Tool discovery: a BM25 ranker over deferred tool metadata and the optional `tool_search`
//! tool. Ported from `packages/coding-agent/src/extensions/tool-search/{index,tool}.ts`.
//!
//! The ranker deliberately uses the same term weighting as upstream: a query term in the tool name
//! counts three times its IDF, a namespace term half, and description terms use length-normalized
//! Okapi BM25. An exact name query wins before term coverage, and coverage wins before raw score.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use super::{Extension, ExtensionRegistry, ToolExposure, ToolRegistration};
use crate::core::extensions::types::{
    ExtensionHost, ExtensionToolContext, ToolDefinition, ToolExposure as RuntimeToolExposure,
    ToolOutcome,
};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

pub const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Naive singular form, so `issues` matches `issue` and `searches` matches `search`.
fn stem(term: &str) -> String {
    if term.len() > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if term.len() > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|suffix| term.ends_with(suffix))
    {
        return term[..term.len() - 2].to_string();
    }
    if term.len() > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].to_string();
    }
    term.to_string()
}

/// Insert identifier boundaries (`camelCase` -> `camel Case`, `HTTPServer` -> `HTTP Server`) and
/// split on every character that is not a Unicode letter, mark, or number.
pub fn tokenize(text: &str) -> Vec<String> {
    let normalized = text.nfc().to_string();
    let mut spaced = String::with_capacity(normalized.len() * 2);
    let chars: Vec<char> = normalized.chars().collect();
    for (index, &ch) in chars.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|position| chars.get(position))
            .copied();
        let next = chars.get(index + 1).copied();
        let boundary = match (previous, next) {
            (Some(lower), _) if lower.is_lowercase() || lower.is_ascii_digit() => ch.is_uppercase(),
            (Some(upper), Some(lower)) if upper.is_uppercase() && ch.is_uppercase() => {
                lower.is_lowercase()
            }
            _ => false,
        };
        if boundary {
            spaced.push(' ');
        }
        spaced.push(ch);
    }
    spaced
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .filter(|word| !STOP_WORDS.contains(&word.as_str()))
        .map(|word| {
            if word.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                stem(&word)
            } else {
                word
            }
        })
        .collect()
}

fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// Recursively collect descriptions and property names from a JSON schema, following local
/// `$ref`s once each so recursive definitions terminate.
fn schema_text(schema: &Value, root: &Value, parts: &mut Vec<String>, seen: &mut HashSet<String>) {
    if !is_object(schema) {
        return;
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && seen.insert(reference.to_string())
        && let Some(target) = reference.strip_prefix("#/").and_then(|path| {
            path.split('/').try_fold(root, |current, segment| {
                let segment = segment.replace("~1", "/").replace("~0", "~");
                current.get(segment)
            })
        })
    {
        schema_text(target, root, parts, seen);
    }
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        parts.push(description.to_string());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, root, parts, seen);
        }
    }
    if let Some(items) = schema.get("items") {
        schema_text(items, root, parts, seen);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            for variant in variants {
                schema_text(variant, root, parts, seen);
            }
        }
    }
}

/// A tool as the ranker sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSearchDocument {
    pub name: String,
    /// Description, schema descriptions, and property names.
    pub text: String,
    /// Name, description, and instructions of the tool's namespace.
    pub namespace_text: Option<String>,
}

impl ToolSearchDocument {
    pub fn new(name: impl Into<String>, description: &str, parameters: &Value) -> Self {
        let mut parts = vec![description.to_string()];
        let mut seen = HashSet::new();
        seen.insert(String::new());
        schema_text(parameters, parameters, &mut parts, &mut seen);
        Self {
            name: name.into(),
            text: parts
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            namespace_text: None,
        }
    }

    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace_text = Some(namespace.into());
        self
    }
}

/// A ranked tool match.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSearchMatch {
    pub name: String,
    pub score: f64,
}

#[derive(Clone, Debug)]
struct IndexedDocument {
    name: String,
    name_terms: HashSet<String>,
    local_name_terms: Vec<String>,
    text_counts: HashMap<String, usize>,
    text_length: usize,
    namespace_terms: HashSet<String>,
}

fn unique_terms(terms: Vec<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for term in terms {
        if !unique.contains(&term) {
            unique.push(term);
        }
    }
    unique
}

fn index_document(document: &ToolSearchDocument) -> IndexedDocument {
    let mut text_counts = HashMap::new();
    for term in tokenize(&document.text) {
        *text_counts.entry(term).or_insert(0) += 1;
    }
    let text_length = text_counts.values().sum();
    let local_segment = document.name.rsplit("__").next().unwrap_or(&document.name);
    IndexedDocument {
        name: document.name.clone(),
        name_terms: tokenize(&document.name).into_iter().collect(),
        local_name_terms: unique_terms(tokenize(local_segment)),
        text_counts,
        text_length,
        namespace_terms: document
            .namespace_text
            .as_deref()
            .map(tokenize)
            .unwrap_or_default()
            .into_iter()
            .collect(),
    }
}

fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn is_exact_name_query(query: &str, name: &str) -> bool {
    let normalized = normalize_name(name);
    normalized == query || normalized.ends_with(&format!("__{query}"))
}

const NAME_WEIGHT: f64 = 3.0;
const NAMESPACE_WEIGHT: f64 = 0.5;

/// Okapi BM25 with separate name and namespace fields.
#[derive(Clone, Copy, Debug)]
pub struct Bm25Ranker {
    pub k1: f64,
    pub b: f64,
}

impl Default for Bm25Ranker {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25Ranker {
    pub fn new(k1: f64, b: f64) -> Self {
        Self { k1, b }
    }

    pub fn rank(
        &self,
        query: &str,
        documents: &[ToolSearchDocument],
        limit: usize,
    ) -> Vec<ToolSearchMatch> {
        let query_terms = unique_terms(tokenize(query));
        if query_terms.is_empty() || documents.is_empty() || limit == 0 {
            return Vec::new();
        }
        let indexed: Vec<IndexedDocument> = documents.iter().map(index_document).collect();
        let total_length: usize = indexed.iter().map(|document| document.text_length).sum();
        let average_length = if total_length == 0 {
            1.0
        } else {
            total_length as f64 / indexed.len() as f64
        };
        let mut idf = HashMap::new();
        for term in &query_terms {
            let frequency = indexed
                .iter()
                .filter(|document| {
                    document.name_terms.contains(term)
                        || document.text_counts.contains_key(term)
                        || document.namespace_terms.contains(term)
                })
                .count();
            let score = (1.0
                + (indexed.len() as f64 - frequency as f64 + 0.5) / (frequency as f64 + 0.5))
                .ln();
            idf.insert(term.clone(), score);
        }
        let full_name_bonus: f64 = idf.values().sum();
        let exact_query = normalize_name(query);
        let mut matches: Vec<(ToolSearchMatch, usize, bool)> = Vec::new();
        for document in indexed {
            let norm =
                self.k1 * (1.0 - self.b + self.b * document.text_length as f64 / average_length);
            let mut score = 0.0;
            let mut covered = 0;
            for term in &query_terms {
                let count = document.text_counts.get(term).copied().unwrap_or(0) as f64;
                let weight = if document.name_terms.contains(term) {
                    NAME_WEIGHT
                } else {
                    0.0
                } + if document.namespace_terms.contains(term) {
                    NAMESPACE_WEIGHT
                } else {
                    0.0
                } + count * (self.k1 + 1.0) / (count + norm);
                if weight > 0.0 {
                    covered += 1;
                }
                score += idf.get(term).copied().unwrap_or(0.0) * weight;
            }
            if score == 0.0 {
                continue;
            }
            if !document.local_name_terms.is_empty()
                && document
                    .local_name_terms
                    .iter()
                    .all(|term| query_terms.contains(term))
            {
                score += full_name_bonus;
            }
            matches.push((
                ToolSearchMatch {
                    name: document.name.clone(),
                    score,
                },
                covered,
                is_exact_name_query(&exact_query, &document.name),
            ));
        }
        matches.sort_by(
            |(left, left_covered, left_exact), (right, right_covered, right_exact)| {
                right_exact
                    .cmp(left_exact)
                    .then_with(|| right_covered.cmp(left_covered))
                    .then_with(|| right.score.total_cmp(&left.score))
            },
        );
        matches
            .into_iter()
            .take(limit)
            .map(|(matching, _, _)| matching)
            .collect()
    }
}

/// Metadata the ranker needs from a tool.
#[derive(Clone, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub namespace: Option<String>,
    pub exposure: ToolExposure,
}

impl ToolInfo {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            namespace: None,
            exposure: ToolExposure::Model,
        }
    }

    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }

    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// The subset of the extension API `tool_search` needs to activate tools.
pub trait ToolAccess: Send + Sync {
    fn all_tools(&self) -> Vec<ToolInfo>;
    fn active_tools(&self) -> Vec<String>;
    fn set_active_tools(&self, names: Vec<String>);
}

fn is_searchable(exposure: ToolExposure) -> bool {
    matches!(exposure, ToolExposure::Codemode | ToolExposure::Deferred)
}

/// Error returned by the `tool_search` tool when its input is invalid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolSearchError {
    EmptyQuery,
    InvalidLimit,
}

impl std::fmt::Display for ToolSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyQuery => formatter.write_str("query must not be empty"),
            Self::InvalidLimit => formatter.write_str("limit must be a positive integer"),
        }
    }
}

impl std::error::Error for ToolSearchError {}

/// Result of a `tool_search` execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSearchExecution {
    pub text: String,
    pub loaded: Vec<String>,
}

/// Rank the searchable tools that are not active yet and activate the matches.
pub fn search_and_load(
    tools: &dyn ToolAccess,
    query: &str,
    limit: usize,
) -> Result<ToolSearchExecution, ToolSearchError> {
    if query.trim().is_empty() {
        return Err(ToolSearchError::EmptyQuery);
    }
    if limit == 0 {
        return Err(ToolSearchError::InvalidLimit);
    }
    let active = tools.active_tools();
    let candidates: Vec<ToolInfo> = tools
        .all_tools()
        .into_iter()
        .filter(|tool| is_searchable(tool.exposure) && !active.contains(&tool.name))
        .collect();
    let documents: Vec<ToolSearchDocument> = candidates
        .iter()
        .map(|tool| {
            let document = ToolSearchDocument::new(&tool.name, &tool.description, &tool.parameters);
            match &tool.namespace {
                Some(namespace) => document.with_namespace(namespace),
                None => document,
            }
        })
        .collect();
    let matches = Bm25Ranker::default().rank(query, &documents, limit);
    let loaded: Vec<String> = matches
        .iter()
        .map(|matching| matching.name.clone())
        .collect();
    if !loaded.is_empty() {
        let mut next = active;
        next.extend(loaded.iter().cloned());
        tools.set_active_tools(next);
    }
    let text = if matches.is_empty() {
        "No matching tools found.".to_string()
    } else {
        let lines: Vec<String> = matches
            .iter()
            .map(|matching| {
                let description = candidates
                    .iter()
                    .find(|tool| tool.name == matching.name)
                    .map_or("", |tool| tool.description.as_str());
                format!(
                    "- {}: {}",
                    matching.name,
                    description.trim().lines().next().unwrap_or("")
                )
            })
            .collect();
        format!(
            "Loaded {} tool{}. They are available from your next call:\n{}",
            matches.len(),
            if matches.len() == 1 { "" } else { "s" },
            lines.join("\n")
        )
    };
    Ok(ToolSearchExecution { text, loaded })
}

pub fn tool_search_definition() -> ToolDefinition {
    let mut def = ToolDefinition::new(
        TOOL_SEARCH_TOOL_NAME,
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query describing the tool or capability to find.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of tools to load (default: 8).",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    );
    def.label = "tool_search".to_owned();
    def.description = TOOL_SEARCH_DESCRIPTION.to_owned();
    def.exposure = RuntimeToolExposure::ModelOnly;
    def
}

struct HostToolAccess(Arc<ExtensionHost>);

impl ToolAccess for HostToolAccess {
    fn all_tools(&self) -> Vec<ToolInfo> {
        self.0
            .get_all_tools()
            .into_iter()
            .filter_map(|def| {
                let exposure = match def.exposure {
                    RuntimeToolExposure::Direct => ToolExposure::Model,
                    RuntimeToolExposure::ModelOnly => ToolExposure::ModelOnly,
                    RuntimeToolExposure::Codemode => ToolExposure::Codemode,
                    RuntimeToolExposure::Deferred => ToolExposure::Deferred,
                    RuntimeToolExposure::Hidden => return None,
                };
                let mut info = ToolInfo::new(def.name, def.description, def.parameters)
                    .with_exposure(exposure);
                let ns = info
                    .name
                    .strip_prefix("mcp__")
                    .and_then(|rest| rest.split_once("__"))
                    .map(|(ns, _)| ns.to_owned());
                if let Some(ns) = ns {
                    info = info.with_namespace(&ns);
                }
                Some(info)
            })
            .collect()
    }

    fn active_tools(&self) -> Vec<String> {
        self.0.get_active_tools()
    }

    fn set_active_tools(&self, names: Vec<String>) {
        let _ = self.0.set_active_tools(names);
    }
}

pub async fn execute_tool_search(
    args: Value,
    ctx: &ExtensionToolContext,
) -> Result<ToolOutcome, String> {
    let query = args["query"].as_str().unwrap_or("");
    let limit = args["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(DEFAULT_TOOL_SEARCH_LIMIT);
    let access = HostToolAccess(Arc::clone(&ctx.base.host));
    match search_and_load(&access, query, limit) {
        Ok(execution) => Ok(ToolOutcome::success(
            json!({ "type": "toolCall", "id": ctx.tool_call_id, "name": TOOL_SEARCH_TOOL_NAME, "arguments": args }),
            json!({
                "content": [{ "type": "text", "text": execution.text }],
                "details": { "loaded": execution.loaded },
            }),
        )),
        Err(error) => Ok(ToolOutcome::error(
            &ctx.tool_call_id,
            TOOL_SEARCH_TOOL_NAME,
            &error.to_string(),
        )),
    }
}

/// The built-in `tool_search` extension. Registered inactive by default, exactly like upstream.
#[derive(Clone, Copy, Debug, Default)]
pub struct ToolSearchExtension;

impl Extension for ToolSearchExtension {
    fn name(&self) -> &'static str {
        "tool-search"
    }

    fn register(&self, registry: &mut ExtensionRegistry) {
        registry.register_tool_with(ToolRegistration {
            name: TOOL_SEARCH_TOOL_NAME.to_string(),
            default_active: false,
            exposure: ToolExposure::ModelOnly,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokenize_splits_identifiers_and_drops_stop_words() {
        assert_eq!(
            tokenize("The SearchIssues tool"),
            ["search", "issue", "tool"]
        );
    }

    #[test]
    fn tokenize_handles_acronyms_and_punctuation() {
        assert_eq!(
            tokenize("HTTPServer_API-v2"),
            ["http", "server", "api", "v2"]
        );
    }

    #[test]
    fn stemming_matches_plurals() {
        assert_eq!(tokenize("searches"), ["search"]);
        assert_eq!(tokenize("issues"), ["issue"]);
        assert_eq!(tokenize("classes"), ["class"]);
    }

    #[test]
    fn tool_document_includes_schema_descriptions_and_property_names() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query" },
                "limit": { "type": "number" }
            }
        });
        let document = ToolSearchDocument::new("search", "Find things", &schema);
        assert!(document.text.contains("Find things"));
        assert!(document.text.contains("query"));
        assert!(document.text.contains("The search query"));
        assert!(document.text.contains("limit"));
    }

    #[test]
    fn tool_document_follows_local_refs_without_looping() {
        let schema = json!({
            "type": "object",
            "properties": { "child": { "$ref": "#/$defs/Node" } },
            "$defs": { "Node": { "description": "Recursive node", "properties": { "next": { "$ref": "#/$defs/Node" } } } }
        });
        let document = ToolSearchDocument::new("tree", "Tree tools", &schema);
        assert_eq!(document.text.matches("Recursive node").count(), 1);
    }

    #[test]
    fn name_match_beats_description_mentions() {
        let documents = [
            ToolSearchDocument::new("window", "A window tool", &json!({})),
            ToolSearchDocument::new("screenshot", "Take a screenshot of a window", &json!({})),
        ];
        let matches = Bm25Ranker::default().rank("screenshot window", &documents, 8);
        assert_eq!(matches[0].name, "screenshot");
    }

    #[test]
    fn exact_name_query_ranks_first() {
        let documents = [
            ToolSearchDocument::new("mcp__peekaboo__app", "Mention app and peekaboo", &json!({})),
            ToolSearchDocument::new("other__app", "app", &json!({})),
        ];
        let matches = Bm25Ranker::default().rank("thread_read", &documents, 8);
        assert!(matches.is_empty() || matches[0].name != "other__app");
        let exact = Bm25Ranker::default().rank("app", &documents, 8);
        assert!(
            exact
                .iter()
                .any(|matching| matching.name == "mcp__peekaboo__app")
        );
    }

    #[test]
    fn rank_respects_limit_and_empty_query() {
        let documents = [
            ToolSearchDocument::new("alpha", "alpha alpha", &json!({})),
            ToolSearchDocument::new("beta", "alpha beta", &json!({})),
        ];
        assert_eq!(Bm25Ranker::default().rank("alpha", &documents, 1).len(), 1);
        assert!(Bm25Ranker::default().rank("the", &documents, 8).is_empty());
    }

    #[derive(Default)]
    struct FakeTools {
        tools: Vec<ToolInfo>,
        active: std::sync::Mutex<Vec<String>>,
    }

    impl ToolAccess for FakeTools {
        fn all_tools(&self) -> Vec<ToolInfo> {
            self.tools.clone()
        }
        fn active_tools(&self) -> Vec<String> {
            self.active
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
        fn set_active_tools(&self, names: Vec<String>) {
            *self
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = names;
        }
    }

    #[test]
    fn search_activates_only_deferred_and_codemode_tools() {
        let tools = FakeTools {
            tools: vec![
                ToolInfo::new("codemode_tool", "Deferred docs search", json!({}))
                    .with_exposure(ToolExposure::Deferred),
                ToolInfo::new("direct_tool", "Direct docs search", json!({})),
            ],
            active: Default::default(),
        };
        let result = search_and_load(&tools, "docs search", 8).expect("valid query");
        assert_eq!(result.loaded, ["codemode_tool"]);
        assert_eq!(tools.active_tools(), ["codemode_tool"]);
        assert!(result.text.starts_with("Loaded 1 tool."));
    }

    #[test]
    fn search_rejects_invalid_input() {
        let tools = FakeTools::default();
        assert_eq!(
            search_and_load(&tools, "  ", 8),
            Err(ToolSearchError::EmptyQuery)
        );
        assert_eq!(
            search_and_load(&tools, "query", 0),
            Err(ToolSearchError::InvalidLimit)
        );
    }

    #[test]
    fn extension_registers_inactive_tool() {
        let mut registry = ExtensionRegistry::default();
        ToolSearchExtension.register(&mut registry);
        assert_eq!(registry.tools.len(), 1);
        assert_eq!(registry.tools[0].name, TOOL_SEARCH_TOOL_NAME);
        assert!(!registry.tools[0].default_active);
        assert_eq!(registry.tools[0].exposure, ToolExposure::ModelOnly);
    }

    #[tokio::test]
    async fn execute_tool_search_activates_matching_deferred_tool_on_host() -> Result<(), String> {
        let bus = Arc::new(crate::core::event_bus::EventBus::new());
        let host = Arc::new(crate::core::extensions::types::ExtensionHost::new(bus));
        host.commit();
        let active = Arc::new(std::sync::Mutex::new(vec!["read".to_owned()]));
        let active_get = Arc::clone(&active);
        let active_set = Arc::clone(&active);
        let mut hidden_def =
            crate::core::extensions::types::ToolDefinition::new("secret_tool", json!({}));
        hidden_def.exposure = RuntimeToolExposure::Hidden;
        let mut deferred_def =
            crate::core::extensions::types::ToolDefinition::new("mcp__docs__find", json!({}));
        deferred_def.exposure = RuntimeToolExposure::Deferred;
        deferred_def.description = "Find documentation pages".to_owned();
        host.set_actions(crate::core::extensions::types::ExtensionHostActions {
            get_all_tools: Some(Arc::new(move || {
                vec![deferred_def.clone(), hidden_def.clone()]
            })),
            get_active_tools: Some(Arc::new(move || {
                active_get.lock().unwrap_or_else(|e| e.into_inner()).clone()
            })),
            set_active_tools: Some(Arc::new(move |next| {
                *active_set.lock().unwrap_or_else(|e| e.into_inner()) = next;
                Ok(())
            })),
            ..Default::default()
        });
        let ext_ctx = crate::core::extensions::types::ExtensionContext {
            host,
            cwd: ".".into(),
            mode: crate::core::extensions::types::ExtensionMode::Print,
            ui: crate::core::extensions::types::ExtensionUi::no_op(
                crate::core::extensions::types::ExtensionMode::Print,
            ),
            bus: None,
            is_idle: Arc::new(|| true),
            is_project_trusted: Arc::new(|| true),
            has_pending_messages: Arc::new(|| false),
            get_context_usage: Arc::new(|| None),
            get_system_prompt: Arc::new(String::new),
            abort: Arc::new(|| {}),
            shutdown: Arc::new(|| {}),
            compact: Arc::new(|_| {}),
        };
        let tool_ctx = ExtensionToolContext::new(ext_ctx, "call-1");
        let outcome =
            execute_tool_search(json!({ "query": "documentation find" }), &tool_ctx).await?;
        assert!(!outcome.is_error);
        assert!(
            active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&"mcp__docs__find".to_owned())
        );
        Ok(())
    }
}
