use serde_json::{Value, json};
use std::borrow::Cow;

use crate::error::{Error, Result};
use crate::types::{ConstrainedSampling, ConstrainedSamplingConfig, StrictPreference, Tool};

const UNSUPPORTED: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn allows_null(schema: &Value) -> bool {
    schema["type"] == "null"
        || schema["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|t| t == "null"))
        || schema.get("const") == Some(&Value::Null)
        || schema["enum"]
            .as_array()
            .is_some_and(|v| v.contains(&Value::Null))
        || schema["anyOf"]
            .as_array()
            .is_some_and(|v| v.iter().any(allows_null))
}
fn structured(schema: &Value) -> bool {
    matches!(schema["type"].as_str(), Some("object" | "array"))
        || schema["type"].as_array().is_some_and(|types| {
            types
                .iter()
                .any(|t| matches!(t.as_str(), Some("object" | "array")))
        })
        || schema.get("properties").is_some()
        || schema.get("items").is_some()
}
fn walk(schema: &mut Value, reject: &impl Fn(&str, &Value) -> bool) -> Result<(), String> {
    let s = schema
        .as_object_mut()
        .ok_or_else(|| "boolean schemas are unsupported".to_owned())?;
    for key in UNSUPPORTED {
        if s.contains_key(*key) {
            return Err(format!("{key} schemas are unsupported"));
        }
    }
    for (key, value) in &*s {
        if reject(key, value) {
            return Err(format!("{key}: {value} is unsupported"));
        }
    }
    if !s.contains_key("type") && !s.contains_key("anyOf") && !s.contains_key("enum") {
        return Err("schemas without a type are unsupported".into());
    }
    if let Some(variants) = s.get_mut("anyOf") {
        let variants = variants
            .as_array_mut()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "anyOf must contain at least one schema".to_owned())?;
        for v in variants {
            if structured(v) {
                return Err("object and array unions are unsupported".into());
            }
            walk(v, reject)?;
        }
    }
    if let Some(items) = s.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        walk(items, reject)?;
    }
    let is_object = s.get("type").is_some_and(|t| t == "object");
    if !is_object {
        if s.contains_key("properties") {
            return Err("properties require type object".into());
        }
        return Ok(());
    }
    if s.get("additionalProperties")
        .is_some_and(|v| *v != Value::Bool(false))
    {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    let required = s
        .get("required")
        .map(|v| {
            v.as_array()
                .and_then(|a| a.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
                .ok_or_else(|| "object required must be a string array".to_owned())
        })
        .transpose()?
        .unwrap_or_default();
    let property_names = s
        .get("properties")
        .map(|v| {
            v.as_object()
                .map(|map| map.keys().cloned().collect::<Vec<_>>())
                .ok_or_else(|| "object properties must be a schema map".to_owned())
        })
        .transpose()?
        .unwrap_or_default();
    if required
        .iter()
        .any(|key| !property_names.iter().any(|p| p == key))
    {
        return Err("required contains an unknown property".into());
    }
    let non_nullable: Vec<_> = property_names
        .iter()
        .map(|k| required.contains(&k.as_str()))
        .collect();
    if let Some(properties) = s.get_mut("properties").and_then(Value::as_object_mut) {
        for ((_, property), required) in properties.iter_mut().zip(non_nullable) {
            walk(property, reject)?;
            if !required && !allows_null(property) {
                *property = json!({"anyOf":[property.take(),{"type":"null"}]});
            }
        }
    }
    s.insert("required".into(), json!(property_names));
    s.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}
pub fn make_strict_json_schema(
    schema: &Value,
    reject: impl Fn(&str, &Value) -> bool,
) -> Result<Value, String> {
    if !schema.is_object() {
        return Err("root schema must have type object".into());
    }
    let mut strict = schema.clone();
    walk(&mut strict, &reject)?;
    if strict["type"] != "object" {
        return Err("root schema must have type object".into());
    }
    Ok(strict)
}

pub fn strict_parameters(
    tool: &Tool,
    supported: bool,
    reject: impl Fn(&str, &Value) -> bool,
) -> Result<(Cow<'_, Value>, Option<bool>)> {
    let Some(ConstrainedSampling::Enabled(ConstrainedSamplingConfig::JsonSchema { strict })) =
        &tool.constrained_sampling
    else {
        return Ok((Cow::Borrowed(&tool.parameters), None));
    };
    let schema = if supported {
        make_strict_json_schema(&tool.parameters, reject)
    } else {
        Err("strict tools are unsupported".into())
    };
    match schema {
        Ok(s) => Ok((Cow::Owned(s), Some(true))),
        Err(reason) if *strict == StrictPreference::Require => Err(Error::Protocol(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but {reason}.",
            tool.name
        ))),
        Err(_) => Ok((Cow::Borrowed(&tool.parameters), None)),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Grammar<'a> {
    pub syntax: &'static str,
    pub definition: &'a str,
    pub property: &'a str,
}
pub fn grammar(tool: &Tool, supported: bool) -> Result<Option<Grammar<'_>>> {
    let Some(ConstrainedSampling::Enabled(ConstrainedSamplingConfig::Grammar { variants })) =
        &tool.constrained_sampling
    else {
        return Ok(None);
    };
    if !supported {
        return Ok(None);
    }
    let error = |why: &str| {
        Error::Protocol(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {why}.",
            tool.name
        ))
    };
    let (syntax, definition) = [("openai_lark", "lark"), ("openai_regex", "regex")]
        .iter()
        .find_map(|(k, s)| {
            variants
                .get(*k)
                .filter(|v| !v.trim().is_empty())
                .map(|v| (*s, v))
        })
        .ok_or_else(|| error("no supported grammar variant was provided"))?;
    let schema = &tool.parameters;
    if schema["type"] != "object" {
        return Err(error(
            "grammar constrained sampling requires an object parameter schema",
        ));
    }
    let property = match schema["required"].as_array().map(Vec::as_slice) {
        Some([v]) => v.as_str(),
        _ => None,
    }
    .ok_or_else(|| {
        error("grammar constrained sampling requires exactly one required string property")
    })?;
    let param = schema["properties"].get(property).ok_or_else(|| {
        error(&format!(
            "grammar constrained sampling requires a properties entry for {property}"
        ))
    })?;
    if param["type"] != "string" {
        return Err(error(&format!(
            "grammar constrained sampling property {property} must have type string"
        )));
    }
    Ok(Some(Grammar {
        syntax,
        definition,
        property,
    }))
}

#[derive(Default)]
pub(crate) struct JsonGrammarInput {
    contents: String,
    started: bool,
    closed: bool,
}
impl JsonGrammarInput {
    pub fn delta(&mut self, field: &str, delta: &str) -> Result<String> {
        if self.closed {
            return Err(Error::Protocol(format!(
                "grammar tool input for property \"{field}\" changed after it was closed"
            )));
        }
        if delta.is_empty() {
            return Ok(String::new());
        }
        self.contents.push_str(delta);
        self.encode(field, delta, false)
    }
    pub fn close(&mut self, field: &str, full: Option<&str>) -> Result<String> {
        if self.closed {
            if full.is_none_or(|s| s == self.contents) {
                return Ok(String::new());
            }
            return Err(Error::Protocol(format!(
                "grammar tool input for property \"{field}\" changed after it was closed"
            )));
        }
        let rest = full
            .unwrap_or(&self.contents)
            .strip_prefix(&self.contents)
            .ok_or_else(|| {
                Error::Protocol(format!(
                    "grammar tool input for property \"{field}\" changed non-monotonically"
                ))
            })?
            .to_owned();
        self.contents.push_str(&rest);
        self.closed = true;
        self.encode(field, &rest, true)
    }
    fn encode(&mut self, field: &str, text: &str, close: bool) -> Result<String> {
        let mut delta = String::new();
        if !self.started {
            self.started = true;
            delta.push('{');
            delta.push_str(&serde_json::to_string(field)?);
            delta.push_str(":\"");
        }
        let encoded = serde_json::to_string(text)?;
        delta.push_str(&encoded[1..encoded.len() - 1]);
        if close {
            delta.push_str("\"}");
        }
        Ok(delta)
    }
}
