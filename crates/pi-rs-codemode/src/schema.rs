//! JSON Schema declaration rendering. Reference expansion is bounded per rendering.
#![forbid(unsafe_code)]
use serde_json::{Map, Value};
use std::collections::HashSet;
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;
pub fn render_declarations(options: Value) -> Result<String, String> {
    let mut sections = Vec::new();
    if let Some(tools) = options
        .get("tools")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
    {
        let members = tools
            .iter()
            .map(|tool| {
                Ok(format!(
                    "{}  {}",
                    doc_comment(tool.get("description"), "  "),
                    render_tool_signature(tool.clone(), None)?
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        sections.push(format!(
            "declare const tools: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    let mut namespaces: Vec<(String, Vec<String>)> = Vec::new();
    for global in options
        .get("globals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = global
            .get("name")
            .and_then(Value::as_str)
            .ok_or("global name must be a string")?;
        if let Some((namespace, member)) = name.split_once('.') {
            let index = if let Some(index) = namespaces.iter().position(|(n, _)| n == namespace) {
                index
            } else {
                namespaces.push((namespace.into(), Vec::new()));
                namespaces.len() - 1
            };
            namespaces[index]
                .1
                .push(render_global(member, global, "  ")?);
        } else {
            sections.push(render_global(
                &format!("declare function {name}"),
                global,
                "",
            )?);
        }
    }
    for (namespace, members) in namespaces {
        sections.push(format!(
            "declare const {namespace}: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    Ok(sections.join("\n\n"))
}
pub fn render_tool_signature(tool: Value, options: Option<Value>) -> Result<String, String> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tool name must be a string")?;
    let identifier: String = if name.is_empty() {
        "_".into()
    } else {
        name.chars()
            .enumerate()
            .map(|(i, c)| {
                if c.is_ascii_alphabetic() || c == '_' || c == '$' || (i > 0 && c.is_ascii_digit())
                {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let max = options
        .as_ref()
        .and_then(|o| o.get("inputMaxChars"))
        .filter(|v| !v.is_null() && v.get("$undefined") != Some(&Value::Bool(true)))
        .cloned()
        .unwrap_or(Value::from(DEFAULT_INPUT_SCHEMA_MAX_CHARS));
    let input = tool
        .get("inputSchema")
        .filter(|v| v.get("$undefined") != Some(&Value::Bool(true)))
        .map_or_else(
            || Ok("unknown".into()),
            |schema| schema_to_type(schema.clone(), Some(serde_json::json!({"maxChars":max}))),
        )?;
    let output = render_tool_output_type(
        tool.get("outputSchema")
            .filter(|v| v.get("$undefined") != Some(&Value::Bool(true)))
            .cloned(),
    )?;
    Ok(format!("{identifier}(args: {input}): Promise<{output}>;"))
}
pub fn render_tool_sample(tool: Value, options: Option<Value>) -> Result<String, String> {
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    Ok(format!(
        "{description}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {} }};\n```",
        render_tool_signature(tool.clone(), options)?
    ))
}
fn render_global(head: &str, global: &Value, indent: &str) -> Result<String, String> {
    let comment = doc_comment(global.get("description"), indent);
    if let Some(signature) = global.get("signature").and_then(Value::as_str) {
        return Ok(format!("{comment}{indent}{head}{signature};"));
    }
    let input = render_tool_output_type(global.get("inputSchema").cloned())?;
    let output = render_tool_output_type(global.get("outputSchema").cloned())?;
    Ok(format!(
        "{comment}{indent}{head}(args: {input}): Promise<{output}>;"
    ))
}
fn doc_comment(description: Option<&Value>, indent: &str) -> String {
    let description = description.and_then(Value::as_str).unwrap_or("").trim();
    if description.is_empty() {
        return String::new();
    }
    let escaped = description.replace("*/", "*\\/");
    let lines: Vec<_> = escaped
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    format!(
        "{indent}/**\n{}\n{indent} */\n",
        lines
            .iter()
            .map(|line| if line.is_empty() {
                format!("{indent} *")
            } else {
                format!("{indent} * {line}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub fn render_tool_output_type(schema: Option<Value>) -> Result<String, String> {
    schema.map_or_else(
        || Ok("unknown".into()),
        |schema| schema_to_type(schema, None),
    )
}
pub fn schema_to_type(schema: Value, options: Option<Value>) -> Result<String, String> {
    if options.as_ref().is_some_and(Value::is_null) {
        return Err("Cannot read properties of null (reading 'maxChars')".into());
    }
    let render = |tags| {
        to_type(
            &schema,
            &mut SchemaContext {
                root: &schema,
                resolving: HashSet::new(),
                expansions: 0,
                tags,
            },
        )
    };
    let result = render(true);
    let max = options
        .as_ref()
        .and_then(|o| o.get("maxChars"))
        .filter(|v| v.get("$undefined") != Some(&Value::Bool(true)))
        .map(|v| match v {
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::String(s) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
            Value::Object(o) if o.contains_key("$num") => match o["$num"].as_str() {
                Some("Infinity") => f64::INFINITY,
                Some("-Infinity") => f64::NEG_INFINITY,
                Some("-0") => 0.0,
                _ => f64::NAN,
            },
            _ => v.as_f64().unwrap_or(f64::NAN),
        });
    if max.is_none_or(|max| result.encode_utf16().count() as f64 <= max) {
        return Ok(result);
    }
    let result = render(false);
    Ok(
        if max.is_some_and(|max| result.encode_utf16().count() as f64 <= max) {
            result
        } else {
            "unknown".into()
        },
    )
}
pub fn resolve_schema_ref(reference: String, root: Value) -> Result<Option<Value>, String> {
    Ok(resolve(&reference, &root).cloned())
}
fn resolve<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference
        .get(2..)
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
    {
        let mut bytes = Vec::new();
        let mut iter = segment.bytes();
        while let Some(c) = iter.next() {
            if c == b'%' {
                let hi = (iter.next()? as char).to_digit(16)?;
                let lo = (iter.next()? as char).to_digit(16)?;
                bytes.push((hi * 16 + lo) as u8);
            } else {
                bytes.push(c);
            }
        }
        let key = String::from_utf8(bytes)
            .ok()?
            .replace("~1", "/")
            .replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }
    (current.is_object() || current.is_boolean()).then_some(current)
}
struct SchemaContext<'a> {
    root: &'a Value,
    resolving: HashSet<String>,
    expansions: usize,
    tags: bool,
}
fn union(types: Vec<String>) -> String {
    let mut unique = Vec::new();
    for ty in types {
        if ty == "unknown" {
            return ty;
        }
        if !unique.contains(&ty) {
            unique.push(ty);
        }
    }
    if unique.is_empty() {
        "never".into()
    } else {
        unique.join(" | ")
    }
}
fn to_type(schema: &Value, context: &mut SchemaContext<'_>) -> String {
    if schema == &Value::Bool(false) {
        return "never".into();
    }
    let Some(object) = schema.as_object() else {
        return "unknown".into();
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if context.resolving.contains(reference) || context.expansions >= 32 {
            return "unknown".into();
        }
        let Some(target) = resolve(reference, context.root) else {
            return "unknown".into();
        };
        context.expansions += 1;
        context.resolving.insert(reference.into());
        let result = to_type(target, context);
        context.resolving.remove(reference);
        return result;
    }
    if let Some(value) = object.get("const") {
        return stringify(value).unwrap_or_else(|| "unknown".into());
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return union(
            values
                .iter()
                .map(|v| stringify(v).unwrap_or_else(|| "unknown".into()))
                .collect(),
        );
    }
    if let Some(values) = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| schema.get("oneOf").and_then(Value::as_array))
    {
        return union(values.iter().map(|v| to_type(v, context)).collect());
    }
    if let Some(values) = schema.get("allOf").and_then(Value::as_array) {
        let parts: Vec<_> = values
            .iter()
            .map(|v| to_type(v, context))
            .filter(|v| v != "unknown")
            .map(|v| {
                if v.contains(" | ") {
                    format!("({v})")
                } else {
                    v
                }
            })
            .collect();
        return if parts.is_empty() {
            "unknown".into()
        } else {
            parts.join(" & ")
        };
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array) {
        return union(
            types
                .iter()
                .map(|ty| {
                    let mut copy = schema.clone();
                    copy["type"] = ty.clone();
                    to_type(&copy, context)
                })
                .collect(),
        );
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => array_type(object, context),
        Some("object") => object_type(object, context),
        None if !object.contains_key("type") => {
            if ["properties", "additionalProperties", "required"]
                .iter()
                .any(|key| object.contains_key(*key))
            {
                object_type(object, context)
            } else if object.contains_key("items") || object.contains_key("prefixItems") {
                array_type(object, context)
            } else {
                "unknown".into()
            }
        }
        _ => "unknown".into(),
    }
}
fn array_type(schema: &Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    if let Some(items) = schema.get("items").filter(|v| !v.is_array()) {
        return format!("Array<{}>", to_type(items, context));
    }
    let tuple = schema
        .get("prefixItems")
        .and_then(Value::as_array)
        .or_else(|| schema.get("items").and_then(Value::as_array));
    if let Some(tuple) = tuple.filter(|t| !t.is_empty()) {
        return format!(
            "[{}]",
            tuple
                .iter()
                .map(|v| to_type(v, context))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    "unknown[]".into()
}
fn object_type(schema: &Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut names: Vec<_> = properties.keys().collect();
    names.sort_by_key(|s| s.encode_utf16().collect::<Vec<_>>());
    let required = schema.get("required").and_then(Value::as_array);
    let mut members: Vec<_> = names
        .iter()
        .map(|name| {
            let optional = if required.is_some_and(|r| r.contains(&Value::String((*name).clone())))
            {
                ""
            } else {
                "?"
            };
            let key = if is_identifier(name) {
                (*name).clone()
            } else {
                Value::String((*name).clone()).to_string()
            };
            format!("{key}{optional}: {};", to_type(&properties[*name], context))
        })
        .collect();
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => members.push("[key: string]: unknown;".into()),
        Some(value) => members.push(format!("[key: string]: {};", to_type(value, context))),
        None if names.is_empty() => members.push("[key: string]: unknown;".into()),
        None => {}
    }
    if members.is_empty() {
        return "{}".into();
    }
    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let mut lines: Vec<_> = properties[*name]
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .split(['\r', '\n', '\u{2028}', '\u{2029}'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if context.tags {
                let tags = constraint_tags(&properties[*name]);
                if !tags.is_empty() {
                    lines.push(tags);
                }
            }
            lines
        })
        .collect();
    if comments.iter().all(Vec::is_empty) {
        return format!("{{ {} }}", members.join(" "));
    }
    let mut lines = vec!["{".into()];
    for (index, comment) in comments.iter().enumerate() {
        lines.extend(comment.iter().map(|s| format!("  // {s}")));
        lines.push(format!("  {}", members[index].replace('\n', "\n  ")));
    }
    lines.extend(members[names.len()..].iter().map(|s| format!("  {s}")));
    lines.push("}".into());
    lines.join("\n")
}
fn constraint_tags(schema: &Value) -> String {
    let Some(property) = schema.as_object() else {
        return String::new();
    };
    let mut schemas = vec![property.clone()];
    if let Some(variants) = property
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| property.get("oneOf").and_then(Value::as_array))
    {
        let branches: Vec<_> = variants
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if branches.len() == 1
            && let Some(branch) = branches[0].as_object()
        {
            schemas.push(branch.clone());
        }
    }
    for schema in &mut schemas {
        for (exclusive, inclusive) in [
            ("exclusiveMinimum", "minimum"),
            ("exclusiveMaximum", "maximum"),
        ] {
            if schema.get(exclusive) == Some(&Value::Bool(true)) {
                if let Some(value) = schema.remove(inclusive) {
                    schema.insert(exclusive.into(), value);
                } else {
                    schema.remove(exclusive);
                }
            }
        }
    }
    let mut tags = Vec::new();
    if schemas.iter().any(|s| {
        s.get("type").is_some_and(|v| {
            v == "integer"
                || v.as_array()
                    .is_some_and(|a| a.contains(&Value::String("integer".into())))
        })
    }) {
        tags.push("@integer".into());
    }
    for keyword in [
        "minimum",
        "exclusiveMinimum",
        "maximum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "minItems",
        "maxItems",
    ] {
        let lower = ["minimum", "exclusiveMinimum", "minLength", "minItems"].contains(&keyword);
        let upper = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"].contains(&keyword);
        if lower || upper {
            let selected = schemas
                .iter()
                .filter_map(|s| s.get(keyword))
                .filter_map(Value::as_f64)
                .reduce(|a, b| if lower { a.max(b) } else { a.min(b) });
            if let Some(n) = selected {
                tags.push(format!("@{keyword} {n}"));
            }
        } else {
            let mut seen = Vec::new();
            for value in schemas
                .iter()
                .filter_map(|s| s.get(keyword))
                .filter(|v| v.is_number() || v.is_string())
            {
                if !seen.contains(&value) {
                    tags.push(format!(
                        "@{keyword} {}",
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    ));
                    seen.push(value);
                }
            }
        }
    }
    if schemas
        .iter()
        .any(|s| s.get("uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".into());
    }
    if let Some(value) = property.get("default").filter(|v| !v.is_null()) {
        tags.push(format!(
            "@default {}",
            stringify(value).unwrap_or_else(|| "undefined".into())
        ));
    }
    if property.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".into());
    }
    tags.into_iter()
        .filter(|s| s.encode_utf16().count() <= 80)
        .map(|s| {
            s.replace('\r', "\\u000d")
                .replace('\n', "\\u000a")
                .replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029")
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.chars().enumerate().all(|(i, c)| {
            c.is_ascii_alphabetic() || c == '_' || c == '$' || (i > 0 && c.is_ascii_digit())
        })
}
fn stringify(value: &Value) -> Option<String> {
    if value.get("$undefined") == Some(&Value::Bool(true)) {
        return None;
    }
    fn clean(value: &Value) -> Value {
        match value {
            Value::Object(o) if o.len() == 1 && o.contains_key("$num") => {
                if o["$num"] == "-0" {
                    Value::from(0)
                } else {
                    Value::Null
                }
            }
            Value::Object(o) if o.len() == 1 && o.contains_key("$undefined") => Value::Null,
            Value::Array(a) => Value::Array(a.iter().map(clean).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .filter(|(_, v)| v.get("$undefined") != Some(&Value::Bool(true)))
                    .map(|(k, v)| (k.clone(), clean(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    Some(clean(value).to_string())
}
