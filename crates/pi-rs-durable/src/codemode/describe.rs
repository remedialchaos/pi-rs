//! Prompt and script-visible tool declarations.

use std::collections::{BTreeMap, HashMap, HashSet};

use pi_rs_codemode::{
    identifier::to_codemode_identifier, schema::render_tool_sample, source::js_whitespace,
};
use serde_json::{Value, json};

use super::{
    CODEMODE_TOOL_NAME, CodemodeError,
    models::{CODEMODE_INTERNAL_PREFIX, MODELS_DESCRIPTION},
};

pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3_000;

const INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.\n\
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.\n\
- The script survives a restart: it resumes where it was, without running finished calls again.\n\
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

const GLOBALS: &str = "Globals:\n\
- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.\n\
- `store(key, value)` and `load(key)` keep JSON values across codemode calls.\n\
- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find tools the `codemode` section does not list.";

const OUTRO: &str = "The `codemode` section of the system prompt lists the tools scripts can call, by namespace, as far as space allows.";

pub fn codemode_description(models: bool) -> String {
    if models {
        [INTRO, GLOBALS, MODELS_DESCRIPTION, OUTRO].join("\n\n")
    } else {
        [INTRO, GLOBALS, OUTRO].join("\n\n")
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct CodemodeToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub structured_output_schema: Option<Value>,
    pub deferred: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ToolNamespace {
    pub name: String,
    pub description: String,
    pub instructions: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CodemodeExtensionSpec {
    pub namespace: Option<ToolNamespace>,
    /// Tool names in registration order, including tools that are currently unavailable.
    pub tools: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CodemodeCatalog {
    pub callable: Vec<CodemodeToolSpec>,
    pub extensions: Vec<CodemodeExtensionSpec>,
}

pub fn declaration_of(tool: &CodemodeToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.parameters,
        "outputSchema": tool.structured_output_schema.as_ref().map_or_else(|| json!({"type": "string"}), Value::clone),
    })
}

pub fn sample_of(tool: &CodemodeToolSpec) -> Result<String, CodemodeError> {
    render_tool_sample(declaration_of(tool), None).map_err(CodemodeError::Declaration)
}

pub(super) fn identifier(name: &str) -> Result<String, CodemodeError> {
    to_codemode_identifier(name.to_owned()).map_err(CodemodeError::Declaration)
}

/// The first extension claiming a tool owns it even if that extension has no namespace.
pub fn namespaces_of(extensions: &[CodemodeExtensionSpec]) -> HashMap<&str, &ToolNamespace> {
    let mut seen = HashSet::new();
    let mut namespaces = HashMap::new();
    for extension in extensions {
        for name in &extension.tools {
            if seen.insert(name.as_str())
                && let Some(namespace) = &extension.namespace
            {
                namespaces.insert(name.as_str(), namespace);
            }
        }
    }
    namespaces
}

pub fn declared_namespaces(extensions: &[CodemodeExtensionSpec]) -> Vec<&ToolNamespace> {
    let mut unique = BTreeMap::new();
    for namespace in extensions
        .iter()
        .filter_map(|extension| extension.namespace.as_ref())
    {
        unique.entry(namespace.name.as_str()).or_insert(namespace);
    }
    let mut namespaces: Vec<_> = unique.into_values().collect();
    namespaces.sort_by_cached_key(|namespace| {
        (
            namespace.name.to_lowercase(),
            namespace
                .name
                .chars()
                .map(char::is_uppercase)
                .collect::<Vec<_>>(),
        )
    });
    namespaces
}

struct CatalogEntry<'a> {
    name: &'a str,
    section: String,
    cost: usize,
}

pub fn codemode_section(
    catalog: &CodemodeCatalog,
    budget: usize,
) -> Result<Option<String>, CodemodeError> {
    let namespaces = declared_namespaces(&catalog.extensions);
    let ownership = namespaces_of(&catalog.extensions);
    let groups_by_name: HashMap<_, _> = namespaces
        .iter()
        .enumerate()
        .map(|(idx, ns)| (ns.name.as_str(), idx + 1))
        .collect();
    let mut groups: Vec<Vec<CatalogEntry<'_>>> =
        (0..=namespaces.len()).map(|_| Vec::new()).collect();
    let mut tools_count = 0;
    for tool in &catalog.callable {
        if tool.deferred
            || tool.name == CODEMODE_TOOL_NAME
            || tool.name.starts_with(CODEMODE_INTERNAL_PREFIX)
        {
            continue;
        }
        let id = identifier(&tool.name)?;
        let raw = &tool.name;
        let sample = sample_of(tool)?;
        let sample = sample.trim_matches(js_whitespace);
        let section = if &id == raw {
            format!("### `{id}`\n{sample}")
        } else {
            format!("### `{id}` (`{raw}`)\n{sample}")
        };
        let group = ownership
            .get(raw.as_str())
            .and_then(|ns| groups_by_name.get(ns.name.as_str()))
            .copied()
            .unwrap_or(0);
        let cost = section.encode_utf16().count().div_ceil(4);
        groups[group].push(CatalogEntry {
            name: raw,
            section,
            cost,
        });
        tools_count += 1;
    }
    if tools_count == 0 && namespaces.is_empty() {
        return Ok(None);
    }
    // Round-robin the cheapest remaining tool from each group, but render in registration order.
    let mut remaining = budget;
    let mut shown = HashSet::new();
    let mut active: Vec<_> = groups
        .iter()
        .filter(|group| !group.is_empty())
        .map(|group| {
            let mut queue: Vec<_> = group.iter().collect();
            queue.sort_by_key(|entry| entry.cost);
            queue.into_iter()
        })
        .collect();
    while !active.is_empty() {
        active.retain_mut(|group| {
            let Some(entry) = group.next() else {
                return false;
            };
            if entry.cost > remaining {
                return false;
            }
            remaining -= entry.cost;
            shown.insert(entry.name);
            group.len() > 0
        });
    }
    let mut result = String::from("Tools scripts can call (`await tools.<name>(args)`):");
    if shown.len() < tools_count {
        result.push_str("\n\nSome tools are not listed: find them with `searchTools()`.");
    }
    for (idx, entries) in groups.into_iter().enumerate() {
        if idx > 0 {
            let namespace = namespaces[idx - 1];
            result.push_str("\n\n## ");
            result.push_str(&namespace.name);
            result.push('\n');
            result.push_str(namespace.description.trim_matches(js_whitespace));
        }
        for entry in entries {
            if shown.contains(entry.name) {
                result.push_str("\n\n");
                result.push_str(&entry.section);
            }
        }
    }
    Ok(Some(result))
}
