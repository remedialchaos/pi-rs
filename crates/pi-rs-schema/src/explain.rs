// Errors of a failed value: a walk over the schema that reports each failing keyword at the value it failed on.
// It runs only for values that failed the check, using the compiler's checks for subschemas.

use crate::checks::{Bound, TypeName};
use crate::compiler::Compiler;
use crate::resolver::{DEFAULT_BASE, Documents, NodeId};
use crate::types::{CompileError, CompileOptions, Draft, JsonSchema, ValidationError};
use crate::value::{Value, js_number_to_string};
use std::collections::HashMap;
use std::sync::Arc;

/// Explains failures of one compiled schema.
pub struct Explainer {
    documents: Arc<Documents>,
    compiler: Compiler,
}

impl Explainer {
    /// An explainer of a schema.
    pub fn new(schema: Arc<JsonSchema>, options: CompileOptions) -> Result<Self, CompileError> {
        let documents = Arc::new(Documents::new(schema, options.schemas.as_ref()));
        let mut compiler = Compiler::new(documents.clone(), &options, false);
        compiler.root()?;
        Ok(Self {
            documents,
            compiler,
        })
    }

    /// The errors of `value`, at least one (`value` failed the check).
    pub fn errors(&mut self, value: &Value) -> Vec<ValidationError> {
        let mut out: Vec<ValidationError> = Vec::new();
        let mut memo: HashMap<(NodeId, usize), (String, Vec<ValidationError>)> = HashMap::new();
        self.walk(
            &NodeId::root(),
            DEFAULT_BASE,
            value,
            "",
            &mut out,
            &mut memo,
        );
        if out.is_empty() {
            out.push(ValidationError::new("", "schema", "invalid"));
        }
        out
    }

    fn ok(&mut self, id: &NodeId, base: &str, value: &Value) -> bool {
        match self.compiler.check(id, base) {
            Ok(check) => {
                let mut scope = Vec::new();
                self.compiler.tree.eval(check, value, &mut scope)
            }
            Err(_) => false,
        }
    }

    fn node(&self, id: &NodeId) -> &JsonSchema {
        self.documents.node_value(id)
    }

    // For $dynamicRef schemas this check runs outside the original dynamic scope, so their errors are
    // approximate.
    fn walk(
        &mut self,
        id: &NodeId,
        base: &str,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        memo: &mut HashMap<(NodeId, usize), (String, Vec<ValidationError>)>,
    ) {
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        if let JsonSchema::Bool(allowed) = node {
            if !*allowed {
                out.push(ValidationError::new(
                    path,
                    "false",
                    "no value is allowed here",
                ));
            }
            return;
        }
        if !matches!(node, JsonSchema::Object(_)) {
            return;
        }
        let own_base = self.compiler.resolver.base_for(id, node, base);
        if self.ok(id, &own_base, value) {
            return;
        }
        let start = out.len();
        if self.compiler.draft == Draft::V7
            && let Some(reference) = node.get("$ref").and_then(JsonSchema::as_str)
        {
            self.walk_ref(reference, &own_base, value, path, out, memo);
            return;
        }
        if let Some(type_value) = node.get("type") {
            let types: Vec<&JsonSchema> = match type_value {
                JsonSchema::Array(items) => items.iter().collect(),
                other => vec![other],
            };
            let matches = types.iter().any(|entry| {
                entry
                    .as_str()
                    .and_then(TypeName::from_name)
                    .is_some_and(|name| name.matches(value))
            });
            if !matches {
                let names: Vec<String> = types.iter().map(|entry| type_name_of(entry)).collect();
                out.push(ValidationError::new(
                    path,
                    "type",
                    format!("expected {}, got {}", names.join(" or "), type_name(value)),
                ));
                return;
            }
        }
        if let Some(JsonSchema::Array(values)) = node.get("enum")
            && !values.iter().any(|entry| Value::from(entry) == *value)
        {
            out.push(ValidationError::new(
                path,
                "enum",
                format!("expected one of {}", list_values(values)),
            ));
        }
        if let Some(constant) = node.get("const")
            && Value::from(constant) != *value
        {
            out.push(ValidationError::new(
                path,
                "const",
                format!("expected {}", json_of(constant)),
            ));
        }
        if let Value::String(text) = value
            && node.get("type").and_then(JsonSchema::as_str) != Some("bytes")
        {
            self.walk_string(node, text, path, out);
        }
        if let Value::Bytes(bytes) = value
            && node.get("type").and_then(JsonSchema::as_str) == Some("bytes")
        {
            self.walk_bytes(node, bytes.len(), path, out);
        }
        if value.is_numeric() {
            self.walk_number(node, value, path, out);
        }
        if let Value::Object(_) = value {
            self.walk_object(id, node, &own_base, value, path, out, memo);
        }
        if let Value::Array(_) = value {
            self.walk_array(id, node, &own_base, value, path, out, memo);
        }
        if let Some(JsonSchema::Array(members)) = node.get("allOf") {
            for index in 0..members.len() {
                self.walk(
                    &id.element("allOf", index),
                    &own_base,
                    value,
                    path,
                    out,
                    memo,
                );
            }
        }
        if let Some(JsonSchema::Array(branches)) = node.get("anyOf") {
            self.walk_union(
                id, "anyOf", &own_base, branches, value, path, out, memo, false,
            );
        }
        if let Some(JsonSchema::Array(branches)) = node.get("oneOf") {
            self.walk_union(
                id, "oneOf", &own_base, branches, value, path, out, memo, true,
            );
        }
        if node.get("not").is_some() && self.ok(&id.child("not"), &own_base, value) {
            out.push(ValidationError::new(
                path,
                "not",
                "must not match the schema in not",
            ));
        }
        if node.get("if").is_some() {
            let branch = if self.ok(&id.child("if"), &own_base, value) {
                node.get("then").map(|_| id.child("then"))
            } else {
                node.get("else").map(|_| id.child("else"))
            };
            if let Some(branch) = branch {
                self.walk(&branch, &own_base, value, path, out, memo);
            }
        }
        if let Some(reference) = node.get("$ref").and_then(JsonSchema::as_str) {
            self.walk_ref(reference, &own_base, value, path, out, memo);
        }
        if out.len() == start
            && (node.get("unevaluatedProperties").is_some()
                || node.get("unevaluatedItems").is_some())
        {
            self.walk_unevaluated(id, node, &own_base, value, path, out, memo);
        }
        if out.len() == start && node.get("$dynamicRef").is_some() {
            out.push(ValidationError::new(
                path,
                "$dynamicRef",
                "does not match the dynamically referenced schema",
            ));
        }
    }

    fn walk_ref(
        &mut self,
        reference: &str,
        base: &str,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        memo: &mut HashMap<(NodeId, usize), (String, Vec<ValidationError>)>,
    ) {
        let target = match self.compiler.resolver.resolve(reference, base) {
            Ok(target) => target,
            Err(_) => return,
        };
        if matches!(self.node(&target.id), JsonSchema::Bool(false))
            && self.compiler.resolver.is_unresolved(reference, base)
        {
            out.push(ValidationError::new(
                path,
                "$ref",
                format!("unresolved reference {reference}"),
            ));
            return;
        }
        if !matches!(self.node(&target.id), JsonSchema::Object(_))
            || !matches!(value, Value::Object(_))
        {
            self.walk(&target.id, &target.base, value, path, out, memo);
            return;
        }
        let key = (target.id.clone(), value as *const Value as usize);
        if let Some((known_path, errors)) = memo.get(&key)
            && known_path == path
        {
            out.extend(errors.clone());
            return;
        }
        let mut errors: Vec<ValidationError> = Vec::new();
        self.walk(&target.id, &target.base, value, path, &mut errors, memo);
        memo.insert(key, (path.to_string(), errors.clone()));
        out.extend(errors);
    }

    fn walk_string(
        &mut self,
        node: &JsonSchema,
        text: &str,
        path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        let points = crate::value::code_points(text) as f64;
        if let Some(min) = node.get("minLength").and_then(JsonSchema::as_f64)
            && points < min
        {
            out.push(ValidationError::new(
                path,
                "minLength",
                format!("expected at least {} characters", js_number_to_string(min)),
            ));
        }
        if let Some(max) = node.get("maxLength").and_then(JsonSchema::as_f64)
            && points > max
        {
            out.push(ValidationError::new(
                path,
                "maxLength",
                format!("expected at most {} characters", js_number_to_string(max)),
            ));
        }
        if let Some(pattern) = node.get("pattern").and_then(JsonSchema::as_str)
            && !crate::regex::compile(pattern)
                .map(|compiled| compiled.is_match(text))
                .unwrap_or(true)
        {
            out.push(ValidationError::new(
                path,
                "pattern",
                format!("expected to match {pattern}"),
            ));
        }
        let formats = self.compiler.formats.clone();
        if let Some(formats) = formats
            && let Some(name) = node.get("format").and_then(JsonSchema::as_str)
            && let Some(format) = formats.get(name)
            && !format(text)
        {
            out.push(ValidationError::new(
                path,
                "format",
                format!("expected format {name}"),
            ));
        }
    }

    fn walk_bytes(
        &mut self,
        node: &JsonSchema,
        length: usize,
        path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        let length = length as f64;
        if let Some(min) = node.get("minLength").and_then(JsonSchema::as_f64)
            && length < min
        {
            out.push(ValidationError::new(
                path,
                "minLength",
                format!("expected at least {} bytes", js_number_to_string(min)),
            ));
        }
        if let Some(max) = node.get("maxLength").and_then(JsonSchema::as_f64)
            && length > max
        {
            out.push(ValidationError::new(
                path,
                "maxLength",
                format!("expected at most {} bytes", js_number_to_string(max)),
            ));
        }
    }

    // Numbers and bigints alike: bounds of either kind compare with values of either kind.
    fn walk_number(
        &mut self,
        node: &JsonSchema,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        let bound = match value {
            Value::Number(number) => Bound::Number(*number),
            Value::BigInt(big) => Bound::BigInt(big.clone()),
            _ => return,
        };
        if let Some(minimum) = bound_value(node.get("minimum"))
            && !at_least(&bound, &minimum)
        {
            out.push(ValidationError::new(
                path,
                "minimum",
                format!("expected >= {}", bound_text(&minimum)),
            ));
        }
        if let Some(maximum) = bound_value(node.get("maximum"))
            && !at_most(&bound, &maximum)
        {
            out.push(ValidationError::new(
                path,
                "maximum",
                format!("expected <= {}", bound_text(&maximum)),
            ));
        }
        if let Some(exclusive_minimum) = bound_value(node.get("exclusiveMinimum"))
            && !greater(&bound, &exclusive_minimum)
        {
            out.push(ValidationError::new(
                path,
                "exclusiveMinimum",
                format!("expected > {}", bound_text(&exclusive_minimum)),
            ));
        }
        if let Some(exclusive_maximum) = bound_value(node.get("exclusiveMaximum"))
            && !less(&bound, &exclusive_maximum)
        {
            out.push(ValidationError::new(
                path,
                "exclusiveMaximum",
                format!("expected < {}", bound_text(&exclusive_maximum)),
            ));
        }
        if let Some(multiple_of) = bound_value(node.get("multipleOf"))
            && !crate::checks::is_multiple(&bound, &multiple_of)
        {
            out.push(ValidationError::new(
                path,
                "multipleOf",
                format!("expected a multiple of {}", bound_text(&multiple_of)),
            ));
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the keywords of an object schema, as the upstream names them"
    )]
    fn walk_object(
        &mut self,
        id: &NodeId,
        node: &JsonSchema,
        base: &str,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        memo: &mut HashMap<(NodeId, usize), (String, Vec<ValidationError>)>,
    ) {
        let Value::Object(map) = value else {
            return;
        };
        let empty = serde_json::Map::new();
        let properties = match node.get("properties") {
            Some(JsonSchema::Object(properties)) => properties,
            _ => &empty,
        };
        let required: Vec<String> = match node.get("required") {
            Some(JsonSchema::Array(names)) => names
                .iter()
                .filter_map(JsonSchema::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        for key in &required {
            // Present and set to undefined: missing unless its schema takes undefined.
            let accepted = map.get(key) == Some(&Value::Undefined)
                && properties.contains_key(key)
                && self.ok(&id.member("properties", key), base, &Value::Undefined);
            if !map.contains_key(key) || (map.get(key) == Some(&Value::Undefined) && !accepted) {
                out.push(ValidationError::new(
                    pointer(path, key),
                    "required",
                    "missing",
                ));
            }
        }
        for key in properties.keys() {
            if let Some(item) = map.get(key)
                && *item != Value::Undefined
            {
                self.walk(
                    &id.member("properties", key),
                    base,
                    item,
                    &pointer(path, key),
                    out,
                    memo,
                );
            }
        }
        let patterns = match node.get("patternProperties") {
            Some(JsonSchema::Object(patterns)) => patterns,
            _ => &empty,
        };
        for (key, item) in map {
            let mut matched = properties.contains_key(key);
            for pattern in patterns.keys() {
                if crate::regex::compile(pattern)
                    .map(|compiled| compiled.is_match(key))
                    .unwrap_or(false)
                {
                    matched = true;
                    self.walk(
                        &id.member("patternProperties", pattern),
                        base,
                        item,
                        &pointer(path, key),
                        out,
                        memo,
                    );
                }
            }
            if !matched && let Some(additional) = node.get("additionalProperties") {
                if matches!(additional, JsonSchema::Bool(false)) {
                    out.push(ValidationError::new(
                        pointer(path, key),
                        "additionalProperties",
                        "unknown field",
                    ));
                } else {
                    self.walk(
                        &id.child("additionalProperties"),
                        base,
                        item,
                        &pointer(path, key),
                        out,
                        memo,
                    );
                }
            }
            if let Some(_property_names) = node.get("propertyNames")
                && !self.ok(
                    &id.child("propertyNames"),
                    base,
                    &Value::String(key.clone()),
                )
            {
                out.push(ValidationError::new(
                    pointer(path, key),
                    "propertyNames",
                    "invalid property name",
                ));
            }
        }
        let count = map.len() as f64;
        if let Some(min) = node.get("minProperties").and_then(JsonSchema::as_f64)
            && count < min
        {
            out.push(ValidationError::new(
                path,
                "minProperties",
                format!("expected at least {} properties", js_number_to_string(min)),
            ));
        }
        if let Some(max) = node.get("maxProperties").and_then(JsonSchema::as_f64)
            && count > max
        {
            out.push(ValidationError::new(
                path,
                "maxProperties",
                format!("expected at most {} properties", js_number_to_string(max)),
            ));
        }
        let mut dependencies: Vec<(String, JsonSchema)> = Vec::new();
        if self.compiler.draft != Draft::V7
            && let Some(JsonSchema::Object(entries)) = node.get("dependentRequired")
        {
            for (key, list) in entries {
                dependencies.push((key.clone(), list.clone()));
            }
        }
        if let Some(JsonSchema::Object(entries)) = node.get("dependencies") {
            for (key, dependency) in entries {
                dependencies.push((key.clone(), dependency.clone()));
            }
        }
        for (key, dependency) in dependencies {
            if !map.contains_key(&key) {
                continue;
            }
            match dependency {
                JsonSchema::Array(names) => {
                    for name in names.iter().filter_map(JsonSchema::as_str) {
                        if !map.contains_key(name) {
                            out.push(ValidationError::new(
                                pointer(path, name),
                                "dependentRequired",
                                format!("required when {key} is present"),
                            ));
                        }
                    }
                }
                other => {
                    self.walk(
                        &id.member("dependencies", &key),
                        base,
                        value,
                        path,
                        out,
                        memo,
                    );
                    let _ = other;
                }
            }
        }
        if self.compiler.draft != Draft::V7
            && let Some(JsonSchema::Object(entries)) = node.get("dependentSchemas")
        {
            for key in entries.keys() {
                if map.contains_key(key) {
                    self.walk(
                        &id.member("dependentSchemas", key),
                        base,
                        value,
                        path,
                        out,
                        memo,
                    );
                }
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the keywords of an array schema, as the upstream names them"
    )]
    fn walk_array(
        &mut self,
        id: &NodeId,
        node: &JsonSchema,
        base: &str,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        memo: &mut HashMap<(NodeId, usize), (String, Vec<ValidationError>)>,
    ) {
        let Value::Array(items) = value else {
            return;
        };
        let mut prefix: Vec<NodeId> = Vec::new();
        let mut rest: Option<NodeId> = None;
        if let Some(JsonSchema::Array(items_schema)) = node.get("items") {
            for index in 0..items_schema.len() {
                prefix.push(id.element("items", index));
            }
            if node.get("additionalItems").is_some() {
                rest = Some(id.child("additionalItems"));
            }
        } else {
            if self.compiler.draft != Draft::V7
                && let Some(JsonSchema::Array(items_schema)) = node.get("prefixItems")
            {
                for index in 0..items_schema.len() {
                    prefix.push(id.element("prefixItems", index));
                }
            }
            if node.get("items").is_some() {
                rest = Some(id.child("items"));
            }
        }
        for (index, item) in items.iter().enumerate() {
            let schema = if index < prefix.len() {
                Some(&prefix[index])
            } else {
                rest.as_ref()
            };
            if let Some(schema) = schema {
                self.walk(schema, base, item, &format!("{path}/{index}"), out, memo);
            }
        }
        let count = items.len() as f64;
        if let Some(min) = node.get("minItems").and_then(JsonSchema::as_f64)
            && count < min
        {
            out.push(ValidationError::new(
                path,
                "minItems",
                format!("expected at least {} items", js_number_to_string(min)),
            ));
        }
        if let Some(max) = node.get("maxItems").and_then(JsonSchema::as_f64)
            && count > max
        {
            out.push(ValidationError::new(
                path,
                "maxItems",
                format!("expected at most {} items", js_number_to_string(max)),
            ));
        }
        if matches!(node.get("uniqueItems"), Some(JsonSchema::Bool(true))) {
            let mut unique = true;
            for index in 1..items.len() {
                for other in 0..index {
                    if items[index] == items[other] {
                        unique = false;
                    }
                }
            }
            if !unique {
                out.push(ValidationError::new(
                    path,
                    "uniqueItems",
                    "expected unique items",
                ));
            }
        }
        if node.get("contains").is_some() {
            let draft7 = self.compiler.draft == Draft::V7;
            let min = if draft7 {
                1.0
            } else {
                node.get("minContains")
                    .and_then(JsonSchema::as_f64)
                    .unwrap_or(1.0)
            };
            let max = if draft7 {
                f64::INFINITY
            } else {
                node.get("maxContains")
                    .and_then(JsonSchema::as_f64)
                    .unwrap_or(f64::INFINITY)
            };
            let mut matched = 0;
            for item in items {
                if self.ok(&id.child("contains"), base, item) {
                    matched += 1;
                }
            }
            if (matched as f64) < min || (matched as f64) > max {
                out.push(ValidationError::new(
                    path,
                    "contains",
                    contains_message(min, max),
                ));
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the branches of a union and where its errors go"
    )]
    fn walk_union(
        &mut self,
        id: &NodeId,
        keyword: &str,
        base: &str,
        branches: &[JsonSchema],
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        memo: &mut HashMap<(NodeId, usize), (String, Vec<ValidationError>)>,
        exactly_one: bool,
    ) {
        let mut matched = 0;
        for index in 0..branches.len() {
            if self.ok(&id.element(keyword, index), base, value) {
                matched += 1;
            }
        }
        if matched == 1 || (!exactly_one && matched > 0) {
            return;
        }
        if matched > 1 {
            out.push(ValidationError::new(
                path,
                keyword,
                "matches more than one alternative",
            ));
            return;
        }
        // A tagged union reports the branch its tag selects; otherwise the branch whose first error is deepest.
        if let Some((key, values)) = self.tag_of(id, keyword, base, branches)
            && let Value::Object(map) = value
        {
            let position = map
                .get(&key)
                .and_then(|tag| values.iter().position(|candidate| candidate == tag));
            match position {
                None => out.push(ValidationError::new(
                    pointer(path, &key),
                    keyword,
                    format!("expected one of {}", list_value_text(&values)),
                )),
                Some(index) => {
                    self.walk(&id.element(keyword, index), base, value, path, out, memo);
                }
            }
            return;
        }
        // An object is blamed on the branch that declares most of its properties (the one it was most likely
        // meant for), then on the branch whose first error is deepest.
        let mut best: Option<(Vec<ValidationError>, usize)> = None;
        for (index, _) in branches.iter().enumerate() {
            let mut errors: Vec<ValidationError> = Vec::new();
            self.walk(
                &id.element(keyword, index),
                base,
                value,
                path,
                &mut errors,
                memo,
            );
            if errors.is_empty() {
                continue;
            }
            let declared = self.declared_keys(&id.element(keyword, index), base, value);
            let better = match &best {
                None => true,
                Some((best_errors, best_declared)) => {
                    declared > *best_declared
                        || (declared == *best_declared
                            && errors[0].path.len() > best_errors[0].path.len())
                }
            };
            if better {
                best = Some((errors, declared));
            }
        }
        match best {
            Some((errors, _)) => out.extend(errors),
            None => out.push(ValidationError::new(
                path,
                keyword,
                "matches no alternative",
            )),
        }
    }

    /// The tag of a union: the key and the values its branches declare.
    fn tag_of(
        &mut self,
        id: &NodeId,
        keyword: &str,
        base: &str,
        branches: &[JsonSchema],
    ) -> Option<(String, Vec<Value>)> {
        let mut resolved: Vec<JsonSchema> = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            let node = match branch.get("$ref").and_then(JsonSchema::as_str) {
                Some(reference) => {
                    let branch_base =
                        self.compiler
                            .resolver
                            .base_for(&id.element(keyword, index), branch, base);
                    let target = self
                        .compiler
                        .resolver
                        .resolve(reference, &branch_base)
                        .ok()?;
                    self.node(&target.id).clone()
                }
                None => branch.clone(),
            };
            resolved.push(node);
        }
        let first = resolved.first()?;
        let JsonSchema::Object(properties) = first.get("properties")? else {
            return None;
        };
        for key in properties.keys() {
            let mut values: Vec<Value> = Vec::new();
            let mut primitive = true;
            let mut distinct = true;
            for node in &resolved {
                let JsonSchema::Object(map) = node else {
                    primitive = false;
                    break;
                };
                let Some(JsonSchema::Object(properties)) = map.get("properties") else {
                    primitive = false;
                    break;
                };
                let Some(JsonSchema::Object(tag)) = properties.get(key) else {
                    primitive = false;
                    break;
                };
                let Some(constant) = tag.get("const") else {
                    primitive = false;
                    break;
                };
                let value = Value::from(constant);
                if matches!(value, Value::Object(_) | Value::Array(_)) {
                    primitive = false;
                    break;
                }
                if values.contains(&value) {
                    distinct = false;
                }
                values.push(value);
            }
            if primitive && distinct {
                return Some((key.clone(), values));
            }
        }
        None
    }

    /// How many of the keys of `value` the branch declares in `properties`.
    fn declared_keys(&mut self, id: &NodeId, base: &str, value: &Value) -> usize {
        let Value::Object(map) = value else {
            return 0;
        };
        let branch_node = self.node(id).clone();
        let node = match branch_node.get("$ref").and_then(JsonSchema::as_str) {
            Some(reference) => {
                let branch_base = self.compiler.resolver.base_for(id, self.node(id), base);
                match self.compiler.resolver.resolve(reference, &branch_base) {
                    Ok(target) => self.node(&target.id).clone(),
                    Err(_) => return 0,
                }
            }
            None => self.node(id).clone(),
        };
        let Some(JsonSchema::Object(properties)) = node.get("properties") else {
            return 0;
        };
        map.keys()
            .filter(|key| properties.contains_key(*key))
            .count()
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the keywords of a schema, as the upstream names them"
    )]
    fn walk_unevaluated(
        &mut self,
        id: &NodeId,
        node: &JsonSchema,
        base: &str,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        memo: &mut HashMap<(NodeId, usize), (String, Vec<ValidationError>)>,
    ) {
        let mut annotations = crate::checks::Annotations::new();
        if let Ok(check) = self.compiler.annotating_stripped(id, base) {
            let mut scope = Vec::new();
            self.compiler
                .tree
                .eval_annotating(check, value, &mut annotations, &mut scope);
        }
        if let Value::Object(map) = value
            && let Some(unevaluated) = node.get("unevaluatedProperties")
            && !annotations.all_props()
        {
            for (key, item) in map {
                if annotations.has_prop(key) {
                    continue;
                }
                if matches!(unevaluated, JsonSchema::Bool(false)) {
                    out.push(ValidationError::new(
                        pointer(path, key),
                        "unevaluatedProperties",
                        "unknown field",
                    ));
                } else {
                    self.walk(
                        &id.child("unevaluatedProperties"),
                        base,
                        item,
                        &pointer(path, key),
                        out,
                        memo,
                    );
                }
            }
        }
        if let Value::Array(items) = value
            && let Some(unevaluated) = node.get("unevaluatedItems")
            && !annotations.all_items()
        {
            for (index, _) in items.iter().enumerate().skip(annotations.items()) {
                if annotations.has_index(index) {
                    continue;
                }
                if matches!(unevaluated, JsonSchema::Bool(false)) {
                    out.push(ValidationError::new(
                        format!("{path}/{index}"),
                        "unevaluatedItems",
                        "unexpected item",
                    ));
                } else {
                    self.walk(
                        &id.child("unevaluatedItems"),
                        base,
                        &items[index],
                        &format!("{path}/{index}"),
                        out,
                        memo,
                    );
                }
            }
        }
    }
}

/// Whether `a` is at least `b`.
fn at_least(a: &Bound, b: &Bound) -> bool {
    matches!(
        crate::checks::compare_bound(a, b),
        Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal)
    )
}

/// Whether `a` is at most `b`.
fn at_most(a: &Bound, b: &Bound) -> bool {
    matches!(
        crate::checks::compare_bound(a, b),
        Some(std::cmp::Ordering::Less) | Some(std::cmp::Ordering::Equal)
    )
}

/// Whether `a` is greater than `b`.
fn greater(a: &Bound, b: &Bound) -> bool {
    crate::checks::compare_bound(a, b) == Some(std::cmp::Ordering::Greater)
}

/// Whether `a` is less than `b`.
fn less(a: &Bound, b: &Bound) -> bool {
    crate::checks::compare_bound(a, b) == Some(std::cmp::Ordering::Less)
}

/// A number keyword's bound: a number, or a bigint written as a decimal string.
fn bound_value(value: Option<&JsonSchema>) -> Option<Bound> {
    match value {
        Some(JsonSchema::Number(number)) => {
            Some(Bound::Number(number.as_f64().unwrap_or(f64::NAN)))
        }
        Some(JsonSchema::String(text)) => {
            text.parse::<num_bigint::BigInt>().ok().map(Bound::BigInt)
        }
        _ => None,
    }
}

/// The text of a bound, as JavaScript writes it.
fn bound_text(bound: &Bound) -> String {
    match bound {
        Bound::Number(number) => js_number_to_string(*number),
        Bound::BigInt(big) => big.to_string(),
    }
}

/// The name of a type in a schema.
fn type_name_of(value: &JsonSchema) -> String {
    match value {
        JsonSchema::String(name) => name.clone(),
        other => json_of(other),
    }
}

/// The name of the type of a value, as JavaScript writes it.
fn type_name(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Bytes(_) => "bytes".to_string(),
        Value::BigInt(_) => "bigint".to_string(),
        Value::Number(number) => {
            if number.fract() == 0.0 && number.is_finite() {
                "integer".to_string()
            } else if !number.is_finite() {
                js_number_to_string(*number)
            } else {
                "number".to_string()
            }
        }
        Value::Bool(_) => "boolean".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Undefined => "undefined".to_string(),
        Value::Object(_) => "object".to_string(),
    }
}

/// The JSON form of a value, as JavaScript's `JSON.stringify` writes it.
fn json_of(value: &JsonSchema) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// The number of enum values an error message lists.
const LISTED_VALUES: usize = 20;

/// The values of an enum, as JSON, at most `LISTED_VALUES` of them, then how many more there are.
fn list_values(values: &[JsonSchema]) -> String {
    if values.len() <= LISTED_VALUES {
        return serde_json::to_string(values).unwrap_or_default();
    }
    let listed = serde_json::to_string(&values[..LISTED_VALUES]).unwrap_or_default();
    format!(
        "{}, ... and {} more]",
        &listed[..listed.len() - 1],
        values.len() - LISTED_VALUES
    )
}

/// The values of a tag, as JSON.
fn list_value_text(values: &[Value]) -> String {
    let text: Vec<String> = values.iter().map(value_json).collect();
    format!("[{}]", text.join(","))
}

/// The JSON form of a value, as JavaScript's `JSON.stringify` writes it.
fn value_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => js_number_to_string(*number),
        Value::BigInt(big) => big.to_string(),
        Value::String(text) => serde_json::to_string(text).unwrap_or_default(),
        Value::Undefined => "undefined".to_string(),
        Value::Bytes(bytes) => format!(
            "[{}]",
            bytes
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(value_json).collect::<Vec<_>>().join(",")
            )
        }
        Value::Object(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(key, item)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        value_json(item)
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
    }
}

/// The message of a `contains` failure.
fn contains_message(min: f64, max: f64) -> String {
    if max == f64::INFINITY {
        return format!(
            "expected at least {} matching {}",
            js_number_to_string(min),
            if min == 1.0 { "item" } else { "items" }
        );
    }
    if min == 0.0 {
        return format!(
            "expected at most {} matching {}",
            js_number_to_string(max),
            if max == 1.0 { "item" } else { "items" }
        );
    }
    format!(
        "expected between {} and {} matching items",
        js_number_to_string(min),
        js_number_to_string(max)
    )
}

/// The JSON Pointer of a member of a value.
fn pointer(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}
