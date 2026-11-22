// Coercion: turns a loosely typed value (a model's tool call arguments, form input, query strings) into the value a
// schema most likely meant, before it is checked.
//
// Three passes, in this order:
// 1. Optional nulls: a `null` in an optional property whose schema rejects null is dropped, as if omitted.
// 2. Conversion rules ported from TypeBox 1.3.27's Value.Convert (MIT), keyed on JSON Schema structure (`$ref`,
//    `const`, `enum`, `anyOf`/`oneOf`, `allOf`, `type`, `items`/`prefixItems`,
//    `properties`/`patternProperties`) instead of TypeBox's kind marks.
// 3. Primitive coercion by `type` (also type arrays), as in Ajv's `coerceTypes`.
// A value that cannot be coerced is left as it is; the check then reports it.
//
// Every pass is copy-on-write: an object or array is copied only when one of its members changes, so an unchanged
// value comes back as the same reference and the input is never modified.

use crate::compiler::Compiler;
use crate::regex;
use crate::resolver::{DEFAULT_BASE, Documents, NodeId};
use crate::types::{CompileError, CompileOptions, JsonSchema};
use crate::value::{Value, js_number_to_string};
use num_bigint::BigInt;
use std::collections::HashMap;
use std::sync::Arc;

/// Coerces values for one compiled schema.
pub struct Coercer {
    compiler: Compiler,
    /// The (schema node, value) pairs pass 2 is converting through a re-entry point, as a stack.
    entered: Vec<(NodeId, String)>,
    /// How often a re-entry was cut short in this coerce() call.
    cuts: usize,
    /// Results of `$ref` targets by (node, value) within one coerce() call.
    memo: Option<HashMap<(NodeId, String), Value>>,
}

impl Coercer {
    /// A coercer of a schema.
    pub fn new(documents: Arc<Documents>, options: &CompileOptions) -> Result<Self, CompileError> {
        let mut compiler = Compiler::new(documents, options, false);
        compiler.root()?;
        Ok(Self {
            compiler,
            entered: Vec::new(),
            cuts: 0,
            memo: None,
        })
    }

    /// `value` coerced: a copy along the changed paths only, or `value` itself when nothing changes.
    pub fn coerce(&mut self, value: &Value) -> Value {
        let base = DEFAULT_BASE;
        let without_nulls = self.drop_optional_nulls(value, &NodeId::root(), base);
        let converted = self.convert_only(&without_nulls);
        self.coerce_types(&converted, &NodeId::root(), base)
    }

    /// Pass 2 alone, as TypeBox's `Value.Convert`.
    pub fn convert_only(&mut self, value: &Value) -> Value {
        let result = self.convert(&NodeId::root(), DEFAULT_BASE, value);
        self.memo = None;
        self.cuts = 0;
        result
    }

    /// The documents, cloned so the schema of a node can be read while the compiler changes.
    fn documents(&self) -> Arc<Documents> {
        self.compiler.documents.clone()
    }

    /// `node` converted for `value` at a re-entry point: unchanged when that pair is already being converted.
    fn reenter(&mut self, id: &NodeId, base: &str, value: &Value, memoize: bool) -> Value {
        let key = value_key(value);
        for (entered_id, entered_value) in self.entered.iter() {
            if entered_id == id && *entered_value == key {
                self.cuts += 1;
                return value.clone();
            }
        }
        if memoize
            && let Some(memo) = &self.memo
            && let Some(result) = memo.get(&(id.clone(), key.clone()))
        {
            return result.clone();
        }
        let cuts = self.cuts;
        self.entered.push((id.clone(), key.clone()));
        let result = self.convert(id, base, value);
        self.entered.pop();
        if memoize && self.cuts == cuts {
            self.memo
                .get_or_insert_with(HashMap::new)
                .insert((id.clone(), key.clone()), result.clone());
        }
        result
    }

    /// Whether `value` passes the schema `node`; `None` when `node` cannot be compiled.
    fn passes(&mut self, id: &NodeId, base: &str, value: &Value) -> Option<bool> {
        let check = self.compiler.check(id, base).ok()?;
        let mut scope = Vec::new();
        Some(self.compiler.tree.eval(check, value, &mut scope))
    }

    fn base_of(&self, id: &NodeId, base: &str) -> String {
        let documents = self.documents();
        let node = documents.node_value(id);
        self.compiler.resolver.base_for(id, node, base)
    }

    // -----------------------------------------------------------------------------------------------------------
    // Pass 1: optional nulls
    // -----------------------------------------------------------------------------------------------------------

    fn drop_optional_nulls(&mut self, value: &Value, id: &NodeId, base: &str) -> Value {
        let documents = self.documents();
        let node = documents.node_value(id);
        let JsonSchema::Object(map) = node else {
            return value.clone();
        };
        let b = self.base_of(id, base);
        if let Value::Array(items) = value {
            let items = items.clone();
            let converted: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(index, item)| match item_schema_at(id, map, index) {
                    Some(child) => self.drop_optional_nulls(item, &child, &b),
                    None => item.clone(),
                })
                .collect();
            return Value::Array(converted);
        }
        let Value::Object(object) = value else {
            return value.clone();
        };
        let has_properties = matches!(map.get("properties"), Some(JsonSchema::Object(_)));
        let patterns: Vec<(regex::Pattern, NodeId)> = match map.get("patternProperties") {
            Some(JsonSchema::Object(entries)) => entries
                .keys()
                .filter_map(|key| {
                    regex::compile(key)
                        .ok()
                        .map(|pattern| (pattern, id.member("patternProperties", key)))
                })
                .collect(),
            _ => Vec::new(),
        };
        let additional = match map.get("additionalProperties") {
            Some(JsonSchema::Object(_)) => Some(id.child("additionalProperties")),
            _ => None,
        };
        if patterns.is_empty() && additional.is_none() && !has_properties {
            return value.clone();
        }
        let required: Vec<String> = match map.get("required") {
            Some(JsonSchema::Array(names)) => names
                .iter()
                .filter_map(JsonSchema::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let mut out: Option<indexmap::IndexMap<String, Value>> = None;
        for (key, property) in object_of(map, "properties").unwrap_or(&serde_json::Map::new()) {
            let Some(item) = object.get(key) else {
                continue;
            };
            let child = id.member("properties", key);
            let is_ref = property.get("$ref").and_then(JsonSchema::as_str).is_some();
            if item.is_null()
                && !required.contains(key)
                && !is_ref
                && self.passes(&child, &b, &Value::Null) == Some(false)
            {
                let target = out.get_or_insert_with(|| object.clone());
                target.shift_remove(key);
                continue;
            }
            let next = self.drop_optional_nulls(item, &child, &b);
            if next != *item {
                let target = out.get_or_insert_with(|| object.clone());
                target.insert(key.to_string(), next);
            }
        }
        if !patterns.is_empty() || additional.is_some() {
            for (key, item) in object {
                let mut matched = has_properties
                    && object_of(map, "properties")
                        .is_some_and(|properties| properties.contains_key(key));
                for (pattern, child) in &patterns {
                    if !pattern.is_match(key) {
                        continue;
                    }
                    matched = true;
                    let current = out
                        .as_ref()
                        .and_then(|target| target.get(key))
                        .cloned()
                        .unwrap_or_else(|| item.clone());
                    let next = self.drop_optional_nulls(&current, child, &b);
                    if next != current {
                        let target = out.get_or_insert_with(|| object.clone());
                        target.insert(key.to_string(), next);
                    }
                }
                if !matched && let Some(child) = &additional {
                    let next = self.drop_optional_nulls(item, child, &b);
                    if next != *item {
                        let target = out.get_or_insert_with(|| object.clone());
                        target.insert(key.clone(), next);
                    }
                }
            }
        }
        match out {
            Some(object) => Value::Object(object),
            None => value.clone(),
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Pass 2: TypeBox's conversion rules
    // -----------------------------------------------------------------------------------------------------------

    fn convert(&mut self, id: &NodeId, base: &str, value: &Value) -> Value {
        let documents = self.documents();
        let node = documents.node_value(id);
        let JsonSchema::Object(map) = node else {
            return value.clone();
        };
        let b = self.base_of(id, base);
        if let Some(reference) = map.get("$ref").and_then(JsonSchema::as_str) {
            let target = match self.compiler.resolver.resolve(reference, &b) {
                Ok(target) => target,
                Err(_) => return value.clone(),
            };
            if !matches!(documents.node_value(&target.id), JsonSchema::Object(_)) {
                return value.clone();
            }
            return self.reenter(&target.id, &target.base, value, true);
        }
        self.convert_node(id, &b, value)
    }

    /// One schema node's conversion: its first keyword among `const`, `enum`, the unions, `allOf` and `type`.
    fn convert_node(&mut self, id: &NodeId, b: &str, value: &Value) -> Value {
        let documents = self.documents();
        let node = documents.node_value(id);
        let JsonSchema::Object(map) = node else {
            return value.clone();
        };
        if let Some(constant) = map.get("const") {
            return convert_literal(&Value::from(constant), value);
        }
        if let Some(JsonSchema::Array(values)) = map.get("enum") {
            return self.convert_enum(id, map, values, b, value);
        }
        if let Some(JsonSchema::Array(branches)) = map.get("anyOf") {
            return self.convert_union(id, "anyOf", map, branches, b, value);
        }
        if let Some(JsonSchema::Array(branches)) = map.get("oneOf") {
            return self.convert_union(id, "oneOf", map, branches, b, value);
        }
        if let Some(JsonSchema::Array(members)) = map.get("allOf") {
            let mut next = value.clone();
            for index in 0..members.len() {
                next = self.convert(&id.element("allOf", index), b, &next);
            }
            return self.convert_members(id, map, b, &next);
        }
        match map.get("type") {
            Some(JsonSchema::String(name)) => match name.as_str() {
                "string" => or_value(try_string(value), value),
                "number" => or_value(try_number(value).map(Value::Number), value),
                "integer" => match try_number(value) {
                    Some(number) => Value::Number(number.trunc()),
                    None => value.clone(),
                },
                "boolean" => or_value(try_boolean(value), value),
                "null" => or_value(try_null(value), value),
                "bigint" => or_value(try_bigint(value), value),
                "undefined" => or_value(try_undefined(value), value),
                "array" => self.convert_array(id, map, b, value),
                "object" => self.convert_object(id, map, b, value),
                _ => value.clone(),
            },
            // A type array converts nothing: the value matches one of the types already.
            Some(_) => value.clone(),
            None => {
                if map.get("properties").is_some() || map.get("patternProperties").is_some() {
                    self.convert_object(id, map, b, value)
                } else if map.get("items").is_some() || map.get("prefixItems").is_some() {
                    self.convert_array(id, map, b, value)
                } else {
                    value.clone()
                }
            }
        }
    }

    /// An enum is a union of its values: a member stays, else the first value the input converts to.
    fn convert_enum(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        values: &[JsonSchema],
        base: &str,
        value: &Value,
    ) -> Value {
        let members: Vec<Value> = values.iter().map(Value::from).collect();
        if members.contains(value) {
            return value.clone();
        }
        for member in &members {
            let candidate = convert_literal(member, value);
            if candidate != *value && self.passes(id, base, &candidate) == Some(true) {
                return candidate;
            }
        }
        let _ = map;
        value.clone()
    }

    /// A value one branch accepts stays; else the first branch's conversion that the whole union accepts.
    fn convert_union(
        &mut self,
        id: &NodeId,
        keyword: &str,
        map: &serde_json::Map<String, JsonSchema>,
        branches: &[JsonSchema],
        base: &str,
        value: &Value,
    ) -> Value {
        for index in 0..branches.len() {
            if self.passes(&id.element(keyword, index), base, value) == Some(true) {
                return self.convert_members(id, map, base, value);
            }
        }
        for index in 0..branches.len() {
            let converted = self.convert(&id.element(keyword, index), base, value);
            let candidate = self.convert_members(id, map, base, &converted);
            if self.passes(id, base, &candidate) == Some(true) {
                return candidate;
            }
        }
        self.convert_members(id, map, base, value)
    }

    /// The object members of a union or intersection node, when it declares any.
    fn convert_members(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        value: &Value,
    ) -> Value {
        let declares = map.get("properties").is_some()
            || map.get("patternProperties").is_some()
            || map.get("additionalProperties").is_some();
        if declares {
            self.convert_object(id, map, base, value)
        } else {
            value.clone()
        }
    }

    fn convert_array(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        value: &Value,
    ) -> Value {
        let items_is_array = matches!(map.get("items"), Some(JsonSchema::Array(_)));
        let prefix: Option<Vec<NodeId>> = if items_is_array {
            match map.get("items") {
                Some(JsonSchema::Array(items)) => Some(
                    (0..items.len())
                        .map(|index| id.element("items", index))
                        .collect(),
                ),
                _ => None,
            }
        } else {
            match map.get("prefixItems") {
                Some(JsonSchema::Array(items)) => Some(
                    (0..items.len())
                        .map(|index| id.element("prefixItems", index))
                        .collect(),
                ),
                _ => None,
            }
        };
        if let Some(prefix) = prefix {
            // A tuple converts its items and never wraps a single value.
            let Value::Array(items) = value else {
                return value.clone();
            };
            let rest = if items_is_array {
                None
            } else {
                map.get("items").map(|_| id.child("items"))
            };
            let items = items.clone();
            let converted: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let child = if index < prefix.len() {
                        Some(&prefix[index])
                    } else {
                        rest.as_ref()
                    };
                    match child {
                        Some(child) => self.convert(child, base, item),
                        None => item.clone(),
                    }
                })
                .collect();
            return Value::Array(converted);
        }
        if let Value::Array(items) = value {
            let items = items.clone();
            return match map.get("items") {
                None => value.clone(),
                Some(_) => {
                    let child = id.child("items");
                    let converted: Vec<Value> = items
                        .iter()
                        .map(|item| self.convert(&child, base, item))
                        .collect();
                    Value::Array(converted)
                }
            };
        }
        // The wrapped single value is converted by the item schema with the same value: a re-entry point.
        match map.get("items") {
            Some(JsonSchema::Object(_)) => {
                let child = id.child("items");
                Value::Array(vec![self.reenter(&child, base, value, false)])
            }
            _ => Value::Array(vec![value.clone()]),
        }
    }

    fn convert_object(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        value: &Value,
    ) -> Value {
        let Value::Object(object) = value else {
            return value.clone();
        };
        let properties = object_of(map, "properties");
        let patterns: Vec<(regex::Pattern, NodeId)> = match map.get("patternProperties") {
            Some(JsonSchema::Object(entries)) => entries
                .keys()
                .filter_map(|key| {
                    regex::compile(key)
                        .ok()
                        .map(|pattern| (pattern, id.member("patternProperties", key)))
                })
                .collect(),
            _ => Vec::new(),
        };
        let additional = match map.get("additionalProperties") {
            Some(JsonSchema::Object(_)) => Some(id.child("additionalProperties")),
            _ => None,
        };
        let mut out: Option<indexmap::IndexMap<String, Value>> = None;
        for (key, item) in object {
            // An undefined property counts as absent.
            if matches!(item, Value::Undefined) {
                continue;
            }
            let mut next = item.clone();
            let mut matched = properties.is_some_and(|entries| entries.contains_key(key));
            if matched {
                let child = id.member("properties", key);
                next = self.convert(&child, base, &next);
            }
            for (pattern, child) in &patterns {
                if !pattern.is_match(key) {
                    continue;
                }
                matched = true;
                next = self.convert(child, base, &next);
            }
            if !matched && let Some(child) = &additional {
                next = self.convert(child, base, &next);
            }
            if next != *item {
                let target = out.get_or_insert_with(|| object.clone());
                target.insert(key.to_string(), next);
            }
        }
        match out {
            Some(object) => Value::Object(object),
            None => value.clone(),
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Pass 3: primitive coercion by type
    // -----------------------------------------------------------------------------------------------------------

    fn coerce_types(&mut self, value: &Value, id: &NodeId, base: &str) -> Value {
        let documents = self.documents();
        let node = documents.node_value(id);
        let JsonSchema::Object(map) = node else {
            return value.clone();
        };
        let b = self.base_of(id, base);
        let mut next = value.clone();
        if let Some(JsonSchema::Array(members)) = map.get("allOf") {
            for index in 0..members.len() {
                next = self.coerce_types(&next, &id.element("allOf", index), &b);
            }
        }
        if let Some(JsonSchema::Array(branches)) = map.get("anyOf") {
            next = self.coerce_union(&next, id, "anyOf", branches, &b);
        }
        if let Some(JsonSchema::Array(branches)) = map.get("oneOf") {
            next = self.coerce_union(&next, id, "oneOf", branches, &b);
        }
        let types = types_of(map);
        let matches_union_member =
            types.len() > 1 && types.iter().any(|kind| matches_type(&next, kind));
        if !types.is_empty() && !matches_union_member {
            for kind in &types {
                let candidate = coerce_primitive(&next, kind);
                if candidate != next {
                    next = candidate;
                    break;
                }
            }
        }
        if types.iter().any(|kind| kind == "object") && matches!(next, Value::Object(_)) {
            next = self.coerce_properties(&next, id, map, &b);
        }
        if types.iter().any(|kind| kind == "array") && matches!(next, Value::Array(_)) {
            next = self.coerce_items(&next, id, map, &b);
        }
        next
    }

    fn coerce_union(
        &mut self,
        value: &Value,
        id: &NodeId,
        keyword: &str,
        branches: &[JsonSchema],
        base: &str,
    ) -> Value {
        for index in 0..branches.len() {
            if self.passes(&id.element(keyword, index), base, value) == Some(true) {
                return value.clone();
            }
        }
        for index in 0..branches.len() {
            let coerced = self.coerce_types(value, &id.element(keyword, index), base);
            if self.passes(&id.element(keyword, index), base, &coerced) == Some(true) {
                return coerced;
            }
        }
        value.clone()
    }

    fn coerce_properties(
        &mut self,
        value: &Value,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
    ) -> Value {
        let Value::Object(object) = value else {
            return value.clone();
        };
        let properties = object_of(map, "properties");
        let patterns: Vec<(regex::Pattern, NodeId)> = match map.get("patternProperties") {
            Some(JsonSchema::Object(entries)) => entries
                .keys()
                .filter_map(|key| {
                    regex::compile(key)
                        .ok()
                        .map(|pattern| (pattern, id.member("patternProperties", key)))
                })
                .collect(),
            _ => Vec::new(),
        };
        let additional = match map.get("additionalProperties") {
            Some(JsonSchema::Object(_)) => Some(id.child("additionalProperties")),
            _ => None,
        };
        let mut out: Option<indexmap::IndexMap<String, Value>> = None;
        if let Some(properties) = properties {
            for (key, _property) in properties {
                let Some(item) = object.get(key) else {
                    continue;
                };
                let child = id.member("properties", key);
                let next = self.coerce_types(item, &child, base);
                if next != *item {
                    let target = out.get_or_insert_with(|| object.clone());
                    target.insert(key.to_string(), next);
                }
            }
        }
        for (key, item) in object {
            let mut matched = properties.is_some_and(|entries| entries.contains_key(key));
            for (pattern, child) in &patterns {
                if !pattern.is_match(key) {
                    continue;
                }
                matched = true;
                let current = out
                    .as_ref()
                    .and_then(|target| target.get(key))
                    .cloned()
                    .unwrap_or_else(|| item.clone());
                let next = self.coerce_types(&current, child, base);
                if next != current {
                    let target = out.get_or_insert_with(|| object.clone());
                    target.insert(key.to_string(), next);
                }
            }
            if !matched && let Some(child) = &additional {
                let next = self.coerce_types(item, child, base);
                if next != *item {
                    let target = out.get_or_insert_with(|| object.clone());
                    target.insert(key.to_string(), next);
                }
            }
        }
        match out {
            Some(object) => Value::Object(object),
            None => value.clone(),
        }
    }

    fn coerce_items(
        &mut self,
        value: &Value,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
    ) -> Value {
        let Value::Array(items) = value else {
            return value.clone();
        };
        let items = items.clone();
        let converted: Vec<Value> = items
            .iter()
            .enumerate()
            .map(|(index, item)| match item_schema_at(id, map, index) {
                Some(child) => self.coerce_types(item, &child, base),
                None => item.clone(),
            })
            .collect();
        Value::Array(converted)
    }
}

/// The schema of an array's item at `index`: a tuple's (`prefixItems`, or a draft-7 `items` array) for its
/// prefix, `items` after `prefixItems`, or `items` for every item.
fn item_schema_at(
    id: &NodeId,
    map: &serde_json::Map<String, JsonSchema>,
    index: usize,
) -> Option<NodeId> {
    if let Some(JsonSchema::Array(items)) = map.get("items") {
        return items.get(index).map(|_| id.element("items", index));
    }
    if let Some(JsonSchema::Array(items)) = map.get("prefixItems")
        && index < items.len()
    {
        return Some(id.element("prefixItems", index));
    }
    map.get("items").map(|_| id.child("items"))
}

/// The key of a value in the coercion's stacks: objects and arrays by their identity, primitives by their value.
fn value_key(value: &Value) -> String {
    match value {
        Value::Object(_) | Value::Array(_) => format!("@{}", value as *const Value as usize),
        other => format!("{other:?}"),
    }
}

/// The map a keyword holds, when it holds one.
fn object_of<'a>(
    map: &'a serde_json::Map<String, JsonSchema>,
    keyword: &str,
) -> Option<&'a serde_json::Map<String, JsonSchema>> {
    match map.get(keyword) {
        Some(JsonSchema::Object(entries)) => Some(entries),
        _ => None,
    }
}

/// The types a schema names.
fn types_of(map: &serde_json::Map<String, JsonSchema>) -> Vec<String> {
    match map.get("type") {
        Some(JsonSchema::String(name)) => vec![name.clone()],
        Some(JsonSchema::Array(names)) => names
            .iter()
            .filter_map(JsonSchema::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a value has a type.
fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => matches!(value, Value::Number(number) if number.is_finite()),
        "integer" => {
            matches!(value, Value::Number(number) if number.fract() == 0.0 && number.is_finite())
        }
        "boolean" => matches!(value, Value::Bool(_)),
        "string" => matches!(value, Value::String(_)),
        "null" => matches!(value, Value::Null),
        "bigint" => matches!(value, Value::BigInt(_)),
        "undefined" => matches!(value, Value::Undefined),
        "array" => matches!(value, Value::Array(_)),
        "object" => matches!(value, Value::Object(_)),
        _ => false,
    }
}

/// The primitive coercion by type, as Ajv's `coerceTypes` does it.
fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match kind {
        "number" | "integer" => {
            if value.is_null() {
                return Value::Number(0.0);
            }
            if let Value::String(text) = value
                && !text.trim().is_empty()
            {
                let parsed = js_to_number(text);
                if kind == "number" {
                    if parsed.is_finite() {
                        return Value::Number(parsed);
                    }
                } else if parsed.is_finite() && parsed.fract() == 0.0 {
                    return Value::Number(parsed);
                }
            }
            if let Value::Bool(flag) = value {
                return Value::Number(if *flag { 1.0 } else { 0.0 });
            }
            value.clone()
        }
        "boolean" => {
            if value.is_null() {
                return Value::Bool(false);
            }
            if value == &Value::String("true".to_string()) || value == &Value::Number(1.0) {
                return Value::Bool(true);
            }
            if value == &Value::String("false".to_string()) || value == &Value::Number(0.0) {
                return Value::Bool(false);
            }
            value.clone()
        }
        "string" => {
            if value.is_null() {
                return Value::String(String::new());
            }
            match value {
                Value::Number(number) => return Value::String(js_number_to_string(*number)),
                Value::Bool(flag) => {
                    return Value::String(if *flag { "true" } else { "false" }.to_string());
                }
                _ => {}
            }
            value.clone()
        }
        "null" => {
            if value == &Value::String(String::new())
                || value == &Value::Number(0.0)
                || value == &Value::Bool(false)
            {
                return Value::Null;
            }
            value.clone()
        }
        _ => value.clone(),
    }
}

// -------------------------------------------------------------------------------------------------------------------
// TypeBox's conversions of one value: a result, or none when there is none
// -------------------------------------------------------------------------------------------------------------------

/// The result of a conversion: the converted value, or none when there is none.
fn or_value(result: Option<Value>, value: &Value) -> Value {
    result.unwrap_or_else(|| value.clone())
}

/// A literal (`const`, an enum value) taken from a value of another type that converts to exactly it.
fn convert_literal(literal: &Value, value: &Value) -> Value {
    if literal == value {
        return value.clone();
    }
    let result = match literal {
        Value::String(_) => try_string(value),
        Value::Number(_) => try_number(value).map(Value::Number),
        Value::Bool(_) => try_boolean(value),
        Value::BigInt(_) => try_bigint(value),
        _ => None,
    };
    match result {
        Some(result) if result == *literal => result,
        _ => value.clone(),
    }
}

fn try_string(value: &Value) -> Option<Value> {
    match value {
        Value::BigInt(big) => Some(Value::String(big.to_string())),
        Value::Bool(flag) => Some(Value::String(
            if *flag { "true" } else { "false" }.to_string(),
        )),
        Value::Number(number) if number.is_finite() => {
            Some(Value::String(js_number_to_string(*number)))
        }
        Value::Null => Some(Value::String("null".to_string())),
        Value::String(text) => Some(Value::String(text.clone())),
        Value::Undefined => Some(Value::String(String::new())),
        _ => None,
    }
}

/// The greatest integer an exact JavaScript number holds.
const MAX_BIGINT: u64 = 9_007_199_254_740_991;

fn try_number(value: &Value) -> Option<f64> {
    match value {
        Value::BigInt(big) => {
            let number = bigint_to_f64(big);
            if number.is_finite() && within_safe_integer(big) {
                Some(number)
            } else {
                None
            }
        }
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) if number.is_finite() => Some(*number),
        Value::Null | Value::Undefined => Some(0.0),
        Value::String(text) => {
            let coerced = js_to_number(text);
            if coerced.is_finite() {
                return Some(coerced);
            }
            let lowercase = text.to_lowercase();
            if lowercase == "false" {
                return Some(0.0);
            }
            if lowercase == "true" {
                return Some(1.0);
            }
            match bigint_of_string(text) {
                Some(big) if within_safe_integer(&big) => Some(bigint_to_f64(&big)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// TypeBox's `TryBigInt` of a string: `12n`, `12.5` (truncated) or `12`.
fn bigint_of_string(value: &str) -> Option<BigInt> {
    let (negative, body) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let digits = body.strip_suffix('n').unwrap_or(body);
    // A fraction is truncated: only its whole part is the bigint.
    let whole = match digits.split_once('.') {
        Some((whole, fraction)) => {
            if fraction.is_empty() || !fraction.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            whole
        }
        None => digits,
    };
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if whole.len() > 1 && whole.starts_with('0') {
        return None;
    }
    let big = BigInt::parse_bytes(whole.as_bytes(), 10)?;
    Some(if negative { -big } else { big })
}

/// TypeBox's `TryBigInt`: booleans to 1n/0n, numbers truncated, null and undefined to 0n, bigint-like strings.
fn try_bigint(value: &Value) -> Option<Value> {
    match value {
        Value::BigInt(big) => Some(Value::BigInt(big.clone())),
        Value::Bool(flag) => Some(Value::BigInt(BigInt::from(if *flag { 1 } else { 0 }))),
        Value::Number(number) if number.is_finite() => {
            Some(Value::BigInt(BigInt::from(number.trunc() as i64)))
        }
        Value::Null | Value::Undefined => Some(Value::BigInt(BigInt::from(0))),
        Value::String(text) => match bigint_of_string(text) {
            Some(big) => Some(Value::BigInt(big)),
            None => {
                let lowercase = text.to_lowercase();
                if lowercase == "false" {
                    Some(Value::BigInt(BigInt::from(0)))
                } else if lowercase == "true" {
                    Some(Value::BigInt(BigInt::from(1)))
                } else {
                    None
                }
            }
        },
        _ => None,
    }
}

/// TypeBox's `TryUndefined`: 0n, false, 0, null, `"undefined"`, `"null"`, `""` and `"0"` to undefined.
fn try_undefined(value: &Value) -> Option<Value> {
    match value {
        Value::Undefined | Value::Null => Some(Value::Undefined),
        Value::Bool(false) => Some(Value::Undefined),
        Value::Number(number) if *number == 0.0 => Some(Value::Undefined),
        Value::BigInt(big) if big == &BigInt::from(0) => Some(Value::Undefined),
        Value::String(text) => {
            let lowercase = text.to_lowercase();
            if lowercase == "undefined" || lowercase == "null" || text.is_empty() || text == "0" {
                Some(Value::Undefined)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn try_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::BigInt(big) => {
            if big == &BigInt::from(0) {
                Some(Value::Bool(false))
            } else if big == &BigInt::from(1) {
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        Value::Bool(flag) => Some(Value::Bool(*flag)),
        Value::Number(number) if number.is_finite() => {
            if *number == 0.0 {
                Some(Value::Bool(false))
            } else if *number == 1.0 {
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        Value::Null | Value::Undefined => Some(Value::Bool(false)),
        Value::String(text) => {
            let lowercase = text.to_lowercase();
            if lowercase == "false" || text == "0" {
                Some(Value::Bool(false))
            } else if lowercase == "true" || text == "1" {
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn try_null(value: &Value) -> Option<Value> {
    match value {
        Value::BigInt(big) => {
            if big == &BigInt::from(0) {
                Some(Value::Null)
            } else {
                None
            }
        }
        Value::Bool(false) => Some(Value::Null),
        Value::Number(number) if number.is_finite() && *number == 0.0 => Some(Value::Null),
        Value::Null | Value::Undefined => Some(Value::Null),
        Value::String(text) => {
            let lowercase = text.to_lowercase();
            if lowercase == "undefined" || lowercase == "null" || text.is_empty() || text == "0" {
                Some(Value::Null)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The value of a bigint as a number.
fn bigint_to_f64(big: &BigInt) -> f64 {
    big.to_string().parse::<f64>().unwrap_or(f64::INFINITY)
}

/// Whether a bigint is within the range of an exact JavaScript number.
fn within_safe_integer(big: &BigInt) -> bool {
    big.magnitude() <= &num_bigint::BigUint::from(MAX_BIGINT)
}

/// The number a string denotes, as JavaScript's `Number` does.
pub(crate) fn js_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return 0.0;
    }
    if trimmed == "Infinity" || trimmed == "+Infinity" {
        return f64::INFINITY;
    }
    if trimmed == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u128::from_str_radix(hex, 16)
            .map(|value| value as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(octal) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        return u128::from_str_radix(octal, 8)
            .map(|value| value as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(binary) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        return u128::from_str_radix(binary, 2)
            .map(|value| value as f64)
            .unwrap_or(f64::NAN);
    }
    if !is_decimal_literal(trimmed) {
        return f64::NAN;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

/// Whether a string is a decimal number, as JavaScript's `Number` reads it.
fn is_decimal_literal(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if body.is_empty() {
        return false;
    }
    let parts: Vec<&str> = body.split(['e', 'E']).collect();
    let mantissa = parts.first().copied().unwrap_or("");
    if parts.len() > 2 {
        return false;
    }
    if parts.len() == 2 {
        let exponent = parts[1];
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    let mut digits = 0;
    let mut dots = 0;
    for c in mantissa.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' => dots += 1,
            _ => return false,
        }
    }
    digits > 0 && dots <= 1
}
