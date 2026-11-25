// The compiler: a schema to one check, built once. Keywords group by the type they constrain, so a value is
// classified once per schema, and the common shapes (closed objects, arrays of one item schema, tagged unions,
// leaf schemas) each get one node.

use crate::checks::{
    AnnotatingNode, Bound, CheckNode, CheckTree, Guard, NumberMode, ObjectEntry, TypeName,
};
use crate::regex;
use crate::resolver::{Documents, NodeId, Resolver, is_schema_object, resolve_uri};
use crate::types::{CompileError, CompileOptions, Draft, FormatCheck, JsonSchema};
use crate::value::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Why a compilation did not finish.
pub(crate) enum Fail {
    /// The schema is malformed.
    Invalid(CompileError),
    /// The schema needs the dynamic scope: compile again with it.
    Restart,
}

impl From<Fail> for CompileError {
    fn from(fail: Fail) -> Self {
        match fail {
            Fail::Invalid(error) => error,
            Fail::Restart => CompileError::UnresolvedRef("\u{0}restart".to_string()),
        }
    }
}

/// The result of a step of a compilation.
pub(crate) type Build<T> = Result<T, Fail>;

/// The keywords that never affect a check.
const NEUTRAL_KEYWORDS: &[&str] = &[
    "$schema",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "$defs",
    "definitions",
    "contentEncoding",
    "contentMediaType",
    "minContains",
    "maxContains",
    "$vocabulary",
];

/// The object keywords besides the core (properties, required, patternProperties, additionalProperties).
const OBJECT_EXTRAS: &[&str] = &[
    "propertyNames",
    "minProperties",
    "maxProperties",
    "dependentRequired",
    "dependentSchemas",
    "dependencies",
];

/// The keywords an annotating check handles itself; every other keyword compiles into its plain part.
const ANNOTATING_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "additionalProperties",
    "prefixItems",
    "items",
    "additionalItems",
    "contains",
    "minContains",
    "maxContains",
    "allOf",
    "anyOf",
    "oneOf",
    "if",
    "then",
    "else",
    "$ref",
    "$dynamicRef",
    "dependentSchemas",
    "unevaluatedProperties",
    "unevaluatedItems",
];

/// One compile of a schema. Its caches and indexes are garbage once the root check is built.
pub struct Compiler {
    /// The documents the schema and its references live in.
    pub documents: Arc<Documents>,
    /// The resolver of this compile.
    pub resolver: Resolver,
    /// The draft whose semantics apply.
    pub draft: Draft,
    /// The format checks to apply.
    pub formats: Option<indexmap::IndexMap<String, FormatCheck>>,
    /// Whether checks track the dynamic scope (the schema has a bookended `$dynamicRef`).
    pub dynamic: bool,
    /// The compiled checks.
    pub tree: CheckTree,
    /// The check of each schema node.
    cache: HashMap<NodeId, usize>,
    /// The annotating check of each schema node.
    annotating_cache: HashMap<NodeId, usize>,
    /// The annotating check of each schema node without its `unevaluated*` parts.
    stripped_cache: HashMap<NodeId, usize>,
    /// The `$dynamicRef` nodes whose targets are still to be compiled.
    pending_dynamic: Vec<(usize, String, usize)>,
    /// The `$dynamicRef` annotating nodes whose targets are still to be compiled.
    pending_annotating_dynamic: Vec<(usize, String, usize)>,
    /// Whether the schema needs the dynamic scope: compile again with it.
    restarted: bool,
}

impl Compiler {
    /// A compiler of a schema.
    pub fn new(documents: Arc<Documents>, options: &CompileOptions, dynamic: bool) -> Self {
        let resolver = Resolver::new(documents.clone(), options);
        Self {
            documents,
            draft: options.draft,
            formats: options.formats.clone(),
            dynamic,
            resolver,
            tree: CheckTree::default(),
            cache: HashMap::new(),
            annotating_cache: HashMap::new(),
            stripped_cache: HashMap::new(),
            pending_dynamic: Vec::new(),
            pending_annotating_dynamic: Vec::new(),
            restarted: false,
        }
    }

    /// Whether the schema needs the dynamic scope: compile again with it.
    pub fn restarted(&self) -> bool {
        self.restarted
    }

    /// The check of the root schema.
    pub fn root(&mut self) -> Result<usize, CompileError> {
        self.root_base().map_err(Into::into)
    }

    fn root_base(&mut self) -> Build<usize> {
        let root = self.check(&NodeId::root(), crate::resolver::DEFAULT_BASE)?;
        self.resolve_dynamic()?;
        Ok(root)
    }

    /// The check of a schema node.
    pub(crate) fn check(&mut self, id: &NodeId, base: &str) -> Build<usize> {
        if let Some(cached) = self.cache.get(id) {
            return Ok(*cached);
        }
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        match node {
            JsonSchema::Bool(true) => return Ok(self.push(CheckNode::Always)),
            JsonSchema::Bool(false) => return Ok(self.push(CheckNode::Never)),
            JsonSchema::Object(_) => {}
            other => {
                return Err(Fail::Invalid(CompileError::NotASchema(other.to_string())));
            }
        }
        if let Some(bare) = self.bare_type(node) {
            return Ok(self.push(bare));
        }
        // The node is reserved before it is built, so a recursive reference finds it.
        let slot = self.push(CheckNode::Always);
        self.cache.insert(id.clone(), slot);
        let own_base = self.resolver.base_for(id, node, base);
        let built = match self.build(id, &own_base) {
            Ok(built) => built,
            Err(error) => {
                // The node does not compile: it is not cached, so a later attempt compiles it again.
                self.cache.remove(id);
                return Err(error);
            }
        };
        self.tree.checks[slot] = CheckNode::Slot(built);
        Ok(slot)
    }

    /// The annotating check of a schema node.
    pub(crate) fn annotating(&mut self, id: &NodeId, base: &str) -> Build<usize> {
        if let Some(cached) = self.annotating_cache.get(id) {
            return Ok(*cached);
        }
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        match node {
            JsonSchema::Bool(true) => return Ok(self.push_annotating(AnnotatingNode::Always)),
            JsonSchema::Bool(false) => return Ok(self.push_annotating(AnnotatingNode::Never)),
            _ => {}
        }
        let slot = self.push_annotating(AnnotatingNode::Always);
        self.annotating_cache.insert(id.clone(), slot);
        let own_base = self.resolver.base_for(id, node, base);
        let built = self.build_annotating(id, &own_base, false)?;
        self.tree.annotating[slot] = AnnotatingNode::Slot(built);
        Ok(slot)
    }

    /// The annotating check of a schema node without its `unevaluated*` parts: what the `unevaluated*` keywords
    /// see.
    pub(crate) fn annotating_stripped(&mut self, id: &NodeId, base: &str) -> Build<usize> {
        if let Some(cached) = self.stripped_cache.get(id) {
            return Ok(*cached);
        }
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        let own_base = self.resolver.base_for(id, node, base);
        let built = self.build_annotating(id, &own_base, true)?;
        self.stripped_cache.insert(id.clone(), built);
        Ok(built)
    }

    fn push(&mut self, node: CheckNode) -> usize {
        self.tree.checks.push(node);
        self.tree.checks.len() - 1
    }

    fn push_annotating(&mut self, node: AnnotatingNode) -> usize {
        self.tree.annotating.push(node);
        self.tree.annotating.len() - 1
    }

    fn push_pattern(&mut self, pattern: regex::Pattern) -> usize {
        self.tree.patterns.push(pattern);
        self.tree.patterns.len() - 1
    }

    /// The module-level check of a schema that is only a `type` (with annotations), `Always` for one with no
    /// assertion at all; undefined for every other schema.
    fn bare_type(&self, node: &JsonSchema) -> Option<CheckNode> {
        let JsonSchema::Object(map) = node else {
            return None;
        };
        let mut type_value = None;
        for key in map.keys() {
            if key == "type" {
                type_value = map.get("type");
            } else if !NEUTRAL_KEYWORDS.contains(&key.as_str()) {
                return None;
            }
        }
        match type_value {
            None => Some(CheckNode::Always),
            Some(JsonSchema::String(name)) => TypeName::from_name(name).map(CheckNode::Type),
            Some(_) => None,
        }
    }

    fn build(&mut self, id: &NodeId, base: &str) -> Build<usize> {
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        if self.draft == Draft::V7
            && let Some(reference) = node.get("$ref").and_then(JsonSchema::as_str)
        {
            return self.reference(reference, base);
        }
        if self.draft != Draft::V7
            && (node.get("unevaluatedProperties").is_some()
                || node.get("unevaluatedItems").is_some())
        {
            let annotating = self.annotating(id, base)?;
            return Ok(self.push(CheckNode::Annotating(annotating)));
        }
        let check = self.plain(id, base, false)?;
        if self.dynamic && self.resolver.is_resource(id, base) {
            return Ok(self.push(CheckNode::Scoped {
                uri: base.to_string(),
                check,
            }));
        }
        Ok(check)
    }

    /// Every keyword of a node, not tracking annotations.
    fn plain(&mut self, id: &NodeId, base: &str, stripped: bool) -> Build<usize> {
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        let JsonSchema::Object(map) = node else {
            return Ok(self.push(CheckNode::Always));
        };
        let mut parts: Vec<usize> = Vec::new();
        let types: Option<Vec<&JsonSchema>> = match map.get("type") {
            None => None,
            Some(JsonSchema::Array(items)) => Some(items.iter().collect()),
            Some(value) => Some(vec![value]),
        };
        let single = types
            .as_ref()
            .filter(|items| items.len() == 1)
            .map(|items| items[0]);
        let single_name = single.and_then(JsonSchema::as_str);
        let type_in_group = if single_name == Some("bytes") {
            let min = number_of(map.get("minLength")).unwrap_or(0.0);
            let max = number_of(map.get("maxLength")).unwrap_or(f64::INFINITY);
            parts.push(self.push(CheckNode::Bytes { min, max }));
            true
        } else {
            let string = self.string_group(map, single_name == Some("string"))?;
            let has_string = string.is_some();
            if let Some(string) = string {
                parts.push(string);
            }
            let numeric = matches!(
                single_name,
                Some("number") | Some("integer") | Some("bigint")
            );
            let mode = match single_name {
                Some("number") => NumberMode::Number,
                Some("integer") => NumberMode::Integer,
                Some("bigint") => NumberMode::BigInt,
                _ => NumberMode::Loose,
            };
            let number = self.number_group(map, mode);
            let has_number = number.is_some();
            if let Some(number) = number {
                parts.push(number);
            }
            let object =
                self.object_group(id, map, base, single_name == Some("object"), stripped)?;
            let has_object = object.is_some();
            if let Some(object) = object {
                parts.push(object);
            }
            let array = self.array_group(id, map, base, single_name == Some("array"), stripped)?;
            let has_array = array.is_some();
            if let Some(array) = array {
                parts.push(array);
            }
            (single_name == Some("string") && has_string)
                || (numeric && has_number)
                || (single_name == Some("object") && has_object)
                || (single_name == Some("array") && has_array)
        };
        if let Some(types) = &types
            && !type_in_group
        {
            parts.insert(0, self.type_check(types));
        }
        self.in_place(id, map, base, &mut parts, stripped)?;
        Ok(self.and(parts))
    }

    /// The keywords that apply to the value itself, whatever its type.
    fn in_place(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        parts: &mut Vec<usize>,
        stripped: bool,
    ) -> Build<()> {
        if let Some(JsonSchema::Array(values)) = map.get("enum") {
            let values: Vec<Value> = values.iter().map(Value::from).collect();
            parts.push(self.push(CheckNode::Enum(values)));
        }
        if let Some(value) = map.get("const") {
            parts.push(self.push(CheckNode::Const(Value::from(value))));
        }
        if !stripped {
            if let Some(JsonSchema::Array(members)) = map.get("allOf") {
                let mut checks = Vec::new();
                for index in 0..members.len() {
                    checks.push(self.check(&id.element("allOf", index), base)?);
                }
                parts.push(self.and(checks));
            }
            if let Some(JsonSchema::Array(branches)) = map.get("anyOf") {
                parts.push(self.union(id, "anyOf", branches, base, false)?);
            }
            if let Some(JsonSchema::Array(branches)) = map.get("oneOf") {
                parts.push(self.union(id, "oneOf", branches, base, true)?);
            }
        }
        if map.get("not").is_some() {
            let check = self.check(&id.child("not"), base)?;
            parts.push(self.push(CheckNode::Not(check)));
        }
        if !stripped
            && map.get("if").is_some()
            && (map.get("then").is_some() || map.get("else").is_some())
        {
            let condition = self.check(&id.child("if"), base)?;
            let then_branch = if map.get("then").is_some() {
                self.check(&id.child("then"), base)?
            } else {
                self.push(CheckNode::Always)
            };
            let else_branch = if map.get("else").is_some() {
                self.check(&id.child("else"), base)?
            } else {
                self.push(CheckNode::Always)
            };
            parts.push(self.push(CheckNode::Conditional {
                condition,
                then_branch,
                else_branch,
            }));
        }
        if !stripped && let Some(reference) = map.get("$ref").and_then(JsonSchema::as_str) {
            parts.push(self.reference(reference, base)?);
        }
        if !stripped
            && self.draft != Draft::V7
            && let Some(reference) = map.get("$dynamicRef").and_then(JsonSchema::as_str)
        {
            parts.push(self.dynamic_reference(reference, base)?);
        }
        Ok(())
    }

    /// `anyOf` or `oneOf`.
    fn union(
        &mut self,
        id: &NodeId,
        keyword: &str,
        branches: &[JsonSchema],
        base: &str,
        exactly_one: bool,
    ) -> Build<usize> {
        if let Some(check) = self.const_union(branches, exactly_one)? {
            return Ok(check);
        }
        if let Some(check) = self.tagged(id, keyword, branches, base)? {
            return Ok(check);
        }
        let mut checks = Vec::new();
        for index in 0..branches.len() {
            checks.push(self.check(&id.element(keyword, index), base)?);
        }
        Ok(if exactly_one {
            self.push(CheckNode::One(checks))
        } else {
            self.any_of(checks)
        })
    }

    /// `anyOf` whose bare-type branches have the types of `mask`: those by one type test, the others by their
    /// checks.
    fn any_of(&mut self, checks: Vec<usize>) -> usize {
        let mut mask = 0;
        let mut rest = Vec::new();
        for check in &checks {
            match self.tree.checks[*check] {
                CheckNode::Type(name) => mask |= name.bit(),
                _ => rest.push(*check),
            }
        }
        if mask == 0 {
            return self.push(CheckNode::Any(rest));
        }
        if rest.is_empty() {
            return self.push(CheckNode::TypeMask(mask));
        }
        let mask_check = self.push(CheckNode::TypeMask(mask));
        let any = self.push(CheckNode::Any(rest));
        self.push(CheckNode::Any(vec![mask_check, any]))
    }

    /// `anyOf`/`oneOf` whose branches are all `{ const: primitive }`: an enum (distinct values make `oneOf` the
    /// same).
    fn const_union(&mut self, branches: &[JsonSchema], exactly_one: bool) -> Build<Option<usize>> {
        let mut values: Vec<Value> = Vec::new();
        for branch in branches {
            let JsonSchema::Object(map) = branch else {
                return Ok(None);
            };
            let Some(constant) = map.get("const") else {
                return Ok(None);
            };
            for key in map.keys() {
                if key != "const" && !NEUTRAL_KEYWORDS.contains(&key.as_str()) {
                    return Ok(None);
                }
            }
            if matches!(constant, JsonSchema::Object(_) | JsonSchema::Array(_)) {
                return Ok(None);
            }
            let value = Value::from(constant);
            if exactly_one && values.contains(&value) {
                return Ok(None);
            }
            values.push(value);
        }
        Ok(Some(self.push(CheckNode::Enum(values))))
    }

    /// `anyOf`/`oneOf` whose branches are all objects with a distinct required const on one key: the tag picks
    /// the branch.
    fn tagged(
        &mut self,
        id: &NodeId,
        keyword: &str,
        branches: &[JsonSchema],
        base: &str,
    ) -> Build<Option<usize>> {
        if branches.len() < 2 {
            return Ok(None);
        }
        let documents = self.documents.clone();
        let mut resolved: Vec<&JsonSchema> = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            let JsonSchema::Object(map) = branch else {
                return Ok(None);
            };
            let reference = match map.get("$ref").and_then(JsonSchema::as_str) {
                Some(reference) if map.len() == 1 => Some(reference),
                _ => None,
            };
            let node = match reference {
                None => branch,
                Some(reference) => {
                    let branch_base =
                        self.resolver
                            .base_for(&id.element(keyword, index), branch, base);
                    let target = self
                        .resolver
                        .resolve(reference, &branch_base)
                        .map_err(Fail::Invalid)?;
                    documents.node_value(&target.id)
                }
            };
            if !is_schema_object(node) {
                return Ok(None);
            }
            resolved.push(node);
        }
        let Some(JsonSchema::Object(first)) = resolved[0].get("properties") else {
            return Ok(None);
        };
        for key in first.keys() {
            let mut tags: Vec<Value> = Vec::new();
            let mut distinct = true;
            for node in &resolved {
                let Some(tag) = tag_of(node, key) else {
                    distinct = false;
                    break;
                };
                if tags.contains(&tag) {
                    distinct = false;
                    break;
                }
                tags.push(tag);
            }
            if !distinct {
                continue;
            }
            let mut checks = Vec::new();
            for index in 0..branches.len() {
                checks.push(self.check(&id.element(keyword, index), base)?);
            }
            return Ok(Some(self.push(CheckNode::Tagged {
                key: key.clone(),
                tags,
                checks,
            })));
        }
        Ok(None)
    }

    /// The string keywords: length bounds, pattern and format.
    fn string_group(
        &mut self,
        map: &serde_json::Map<String, JsonSchema>,
        strict: bool,
    ) -> Build<Option<usize>> {
        let min = number_of(map.get("minLength")).unwrap_or(0.0);
        let max = number_of(map.get("maxLength")).unwrap_or(f64::INFINITY);
        let pattern = match map.get("pattern").and_then(JsonSchema::as_str) {
            Some(source) => {
                let compiled = regex::compile(source)
                    .map_err(|_| Fail::Invalid(CompileError::InvalidPattern(source.to_string())))?;
                Some(self.push_pattern(compiled))
            }
            None => None,
        };
        let format = match (
            self.formats.as_ref(),
            map.get("format").and_then(JsonSchema::as_str),
        ) {
            (Some(formats), Some(name)) if formats.contains_key(name) => {
                Some(formats.get(name).expect("the format is in the map").clone())
            }
            _ => None,
        };
        if min == 0.0 && max == f64::INFINITY && pattern.is_none() && format.is_none() {
            return Ok(None);
        }
        Ok(Some(self.push(CheckNode::Str {
            strict,
            min,
            max,
            pattern,
            format,
        })))
    }

    /// The number keywords.
    fn number_group(
        &mut self,
        map: &serde_json::Map<String, JsonSchema>,
        mode: NumberMode,
    ) -> Option<usize> {
        let min = bound_of(map.get("minimum"));
        let max = bound_of(map.get("maximum"));
        let exclusive_min = bound_of(map.get("exclusiveMinimum"));
        let exclusive_max = bound_of(map.get("exclusiveMaximum"));
        let multiple_of = bound_of(map.get("multipleOf"));
        if min.is_none()
            && max.is_none()
            && exclusive_min.is_none()
            && exclusive_max.is_none()
            && multiple_of.is_none()
        {
            return None;
        }
        Some(self.push(CheckNode::Number {
            mode,
            min: min.unwrap_or_else(Bound::negative_infinity),
            max: max.unwrap_or_else(Bound::positive_infinity),
            exclusive_min: exclusive_min.unwrap_or_else(Bound::negative_infinity),
            exclusive_max: exclusive_max.unwrap_or_else(Bound::positive_infinity),
            multiple_of,
        }))
    }

    /// The object keywords.
    fn object_group(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        strict: bool,
        stripped: bool,
    ) -> Build<Option<usize>> {
        let only_core = OBJECT_EXTRAS
            .iter()
            .all(|keyword| map.get(*keyword).is_none());
        // Without annotations the properties, the patterns and the additional properties are not evaluated; the
        // required names still are.
        let core = self.object_core(
            id,
            map,
            base,
            if only_core {
                if strict { Guard::Strict } else { Guard::Loose }
            } else {
                Guard::None
            },
            stripped,
        )?;
        if core.is_some() && only_core {
            return Ok(core);
        }
        let mut parts: Vec<usize> = core.into_iter().collect();
        if map.get("propertyNames").is_some() {
            let check = self.check(&id.child("propertyNames"), base)?;
            parts.push(self.push(CheckNode::PropertyNames(check)));
        }
        let min = number_of(map.get("minProperties"));
        let max = number_of(map.get("maxProperties"));
        if min.is_some() || max.is_some() {
            parts.push(self.push(CheckNode::PropertyCount {
                min: min.unwrap_or(0.0),
                max: max.unwrap_or(f64::INFINITY),
            }));
        }
        let mut dependent_required: Vec<(String, Vec<String>)> = Vec::new();
        let mut dependent_schemas: Vec<(String, usize)> = Vec::new();
        if self.draft != Draft::V7 {
            if let Some(JsonSchema::Object(entries)) = map.get("dependentRequired") {
                for (key, list) in entries {
                    if let JsonSchema::Array(names) = list {
                        dependent_required.push((
                            key.clone(),
                            names
                                .iter()
                                .filter_map(JsonSchema::as_str)
                                .map(str::to_string)
                                .collect(),
                        ));
                    }
                }
            }
            if let Some(JsonSchema::Object(entries)) = map.get("dependentSchemas") {
                for key in entries.keys() {
                    let check = self.check(&id.member("dependentSchemas", key), base)?;
                    dependent_schemas.push((key.clone(), check));
                }
            }
        }
        if let Some(JsonSchema::Object(entries)) = map.get("dependencies") {
            for (key, dependency) in entries {
                match dependency {
                    JsonSchema::Array(names) => dependent_required.push((
                        key.clone(),
                        names
                            .iter()
                            .filter_map(JsonSchema::as_str)
                            .map(str::to_string)
                            .collect(),
                    )),
                    _ => {
                        let check = self.check(&id.member("dependencies", key), base)?;
                        dependent_schemas.push((key.clone(), check));
                    }
                }
            }
        }
        if !dependent_required.is_empty() {
            parts.push(self.push(CheckNode::DependentRequired(dependent_required)));
        }
        if !dependent_schemas.is_empty() {
            parts.push(self.push(CheckNode::DependentSchemas(dependent_schemas)));
        }
        if parts.is_empty() {
            return Ok(if strict {
                Some(self.push(CheckNode::Type(TypeName::Object)))
            } else {
                None
            });
        }
        let inner = self.and(parts);
        Ok(Some(self.push(CheckNode::GuardObject { inner, strict })))
    }

    /// The core object keywords: properties, required, patternProperties and additionalProperties.
    fn object_core(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        guard: Guard,
        stripped: bool,
    ) -> Build<Option<usize>> {
        let empty = serde_json::Map::new();
        let properties = if stripped {
            &empty
        } else {
            match map.get("properties") {
                Some(JsonSchema::Object(properties)) => properties,
                _ => &empty,
            }
        };
        let required: Vec<String> = match map.get("required") {
            Some(JsonSchema::Array(names)) => names
                .iter()
                .filter_map(JsonSchema::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let patterns = if stripped {
            &empty
        } else {
            match map.get("patternProperties") {
                Some(JsonSchema::Object(patterns)) => patterns,
                _ => &empty,
            }
        };
        let additional = if stripped {
            None
        } else {
            map.get("additionalProperties")
        };
        let keys: Vec<String> = properties.keys().cloned().collect();
        let pattern_keys: Vec<String> = patterns.keys().cloned().collect();
        if keys.is_empty() && required.is_empty() && pattern_keys.is_empty() && additional.is_none()
        {
            return Ok(None);
        }
        let mut entries: Vec<ObjectEntry> = Vec::new();
        for key in &keys {
            let check = self.check(&id.member("properties", key), base)?;
            entries.push(ObjectEntry {
                key: key.clone(),
                check,
                required: required.contains(key),
            });
        }
        let extra_required: Vec<String> = required
            .iter()
            .filter(|key| !properties.contains_key(*key))
            .cloned()
            .collect();
        let mut pattern_checks: Vec<(usize, usize)> = Vec::new();
        for key in &pattern_keys {
            let pattern = regex::compile(key)
                .map_err(|_| Fail::Invalid(CompileError::InvalidPattern(key.clone())))?;
            let index = self.push_pattern(pattern);
            let check = self.check(&id.member("patternProperties", key), base)?;
            pattern_checks.push((index, check));
        }
        let extra = match additional {
            None | Some(JsonSchema::Bool(true)) => None,
            Some(_) => Some(self.check(&id.child("additionalProperties"), base)?),
        };
        if pattern_checks.is_empty()
            && extra_required.is_empty()
            && (extra.is_none() || matches!(additional, Some(JsonSchema::Bool(false))))
        {
            let required_count = keys.iter().filter(|key| required.contains(key)).count();
            return Ok(Some(self.push(CheckNode::DeclaredObject {
                entries,
                required_count,
                closed: matches!(additional, Some(JsonSchema::Bool(false))),
                guard,
            })));
        }
        Ok(Some(self.push(CheckNode::OpenObject {
            entries,
            keys,
            extra_required,
            patterns: pattern_checks,
            additional: extra,
            guard,
        })))
    }

    /// The array keywords.
    fn array_group(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        strict: bool,
        stripped: bool,
    ) -> Build<Option<usize>> {
        let mut prefix: Vec<NodeId> = Vec::new();
        let mut rest: Option<NodeId> = None;
        let items_is_array = matches!(map.get("items"), Some(JsonSchema::Array(_)));
        if items_is_array {
            if let Some(JsonSchema::Array(items)) = map.get("items") {
                for index in 0..items.len() {
                    prefix.push(id.element("items", index));
                }
            }
            if map.get("additionalItems").is_some() {
                rest = Some(id.child("additionalItems"));
            }
        } else {
            if self.draft != Draft::V7
                && let Some(JsonSchema::Array(items)) = map.get("prefixItems")
            {
                for index in 0..items.len() {
                    prefix.push(id.element("prefixItems", index));
                }
            }
            if map.get("items").is_some() {
                rest = Some(id.child("items"));
            }
        }
        let mut prefix_checks = Vec::new();
        for child in &prefix {
            prefix_checks.push(self.check(child, base)?);
        }
        let rest_check = match &rest {
            None => None,
            Some(child) => {
                let documents = self.documents.clone();
                let value = documents.node_value(child);
                if matches!(value, JsonSchema::Bool(true)) {
                    None
                } else {
                    Some(self.check(child, base)?)
                }
            }
        };
        let min_items = number_of(map.get("minItems"));
        let max_items = number_of(map.get("maxItems"));
        let contains = map.get("contains");
        let unique = matches!(map.get("uniqueItems"), Some(JsonSchema::Bool(true)));
        if !stripped
            && contains.is_none()
            && !unique
            && (!prefix_checks.is_empty() || rest_check.is_some())
        {
            let guard = if strict { Guard::Strict } else { Guard::Loose };
            return Ok(Some(self.push(CheckNode::List {
                guard,
                min: min_items.unwrap_or(0.0),
                max: max_items.unwrap_or(f64::INFINITY),
                prefix: prefix_checks,
                rest: rest_check,
            })));
        }
        let mut parts: Vec<usize> = Vec::new();
        if !stripped && (!prefix_checks.is_empty() || rest_check.is_some()) {
            parts.push(self.push(CheckNode::List {
                guard: Guard::None,
                min: 0.0,
                max: f64::INFINITY,
                prefix: prefix_checks,
                rest: rest_check,
            }));
        }
        if min_items.is_some() || max_items.is_some() {
            parts.push(self.push(CheckNode::ArrayLength {
                min: min_items.unwrap_or(0.0),
                max: max_items.unwrap_or(f64::INFINITY),
            }));
        }
        if !stripped && contains.is_some() {
            let min = if self.draft == Draft::V7 {
                1.0
            } else {
                number_of(map.get("minContains")).unwrap_or(1.0)
            };
            let max = if self.draft == Draft::V7 {
                f64::INFINITY
            } else {
                number_of(map.get("maxContains")).unwrap_or(f64::INFINITY)
            };
            let check = self.check(&id.child("contains"), base)?;
            parts.push(self.push(CheckNode::Contains { check, min, max }));
        }
        if !stripped && unique {
            parts.push(self.push(CheckNode::UniqueItems));
        }
        if parts.is_empty() {
            return Ok(if strict {
                Some(self.push(CheckNode::Type(TypeName::Array)))
            } else {
                None
            });
        }
        let inner = self.and(parts);
        Ok(Some(self.push(CheckNode::GuardArray { inner, strict })))
    }

    fn and(&mut self, parts: Vec<usize>) -> usize {
        match parts.len() {
            0 => self.push(CheckNode::Always),
            1 => parts[0],
            _ => self.push(CheckNode::All(parts)),
        }
    }

    fn type_check(&mut self, types: &[&JsonSchema]) -> usize {
        if types.len() == 1 {
            return match types[0].as_str().and_then(TypeName::from_name) {
                Some(name) => self.push(CheckNode::Type(name)),
                None => self.push(CheckNode::Never),
            };
        }
        let mut mask = 0;
        for entry in types {
            if let Some(name) = entry.as_str().and_then(TypeName::from_name) {
                mask |= name.bit();
            }
        }
        self.push(CheckNode::TypeMask(mask))
    }

    /// A `$ref` to another schema.
    fn reference(&mut self, reference: &str, base: &str) -> Build<usize> {
        let target = self
            .resolver
            .resolve(reference, base)
            .map_err(Fail::Invalid)?;
        let check = self.check(&target.id, &target.base)?;
        if self.dynamic && target.base != base {
            return Ok(self.push(CheckNode::Scoped {
                uri: target.base,
                check,
            }));
        }
        Ok(check)
    }

    /// A `$dynamicRef`: the dynamic anchor in scope, or the reference's own target.
    fn dynamic_reference(&mut self, reference: &str, base: &str) -> Build<usize> {
        let Some((initial, name)) = self.bookended(reference, base)? else {
            return self.reference(reference, base);
        };
        let fallback = self.check(&initial.id, &initial.base)?;
        let slot = self.push(CheckNode::DynamicRef {
            name: name.clone(),
            fallback,
            targets: Vec::new(),
        });
        self.pending_dynamic.push((slot, name, fallback));
        Ok(slot)
    }

    /// The initial target and anchor name of a `$dynamicRef` that resolves dynamically, or none for a plain ref.
    fn bookended(
        &mut self,
        reference: &str,
        base: &str,
    ) -> Build<Option<(crate::resolver::Resolved, String)>> {
        let initial = self
            .resolver
            .resolve(reference, base)
            .map_err(Fail::Invalid)?;
        let uri = resolve_uri(reference, base);
        let document = crate::resolver::document_of(&uri);
        let name = uri[document.len() + 1..].to_string();
        let documents = self.documents.clone();
        let node = documents.node_value(&initial.id);
        let anchored = is_schema_object(node)
            && node.get("$dynamicAnchor").and_then(JsonSchema::as_str) == Some(name.as_str());
        if name.is_empty() || name.starts_with('/') || !anchored {
            return Ok(None);
        }
        if !self.dynamic {
            self.restarted = true;
            return Err(Fail::Restart);
        }
        Ok(Some((initial, name)))
    }

    fn build_annotating(&mut self, id: &NodeId, base: &str, stripped: bool) -> Build<usize> {
        let documents = self.documents.clone();
        let node = documents.node_value(id);
        let JsonSchema::Object(map) = node else {
            return Ok(self.push_annotating(AnnotatingNode::Always));
        };
        let annotates = map
            .keys()
            .any(|key| ANNOTATING_KEYWORDS.contains(&key.as_str()));
        let plain = self.plain(id, base, true)?;
        let parts = if annotates {
            self.annotating_parts(id, map, base, stripped)?
        } else {
            Vec::new()
        };
        let all = if parts.is_empty() {
            AnnotatingNode::Ignore(plain)
        } else {
            AnnotatingNode::All { plain, parts }
        };
        let own_node = if !stripped
            && (map.get("unevaluatedProperties").is_some() || map.get("unevaluatedItems").is_some())
        {
            AnnotatingNode::Isolated(self.push_annotating(all))
        } else {
            all
        };
        let own = self.push_annotating(own_node);
        if !stripped && self.dynamic && self.resolver.is_resource(id, base) {
            return Ok(self.push_annotating(AnnotatingNode::Scoped {
                uri: base.to_string(),
                check: own,
            }));
        }
        Ok(own)
    }

    fn annotating_parts(
        &mut self,
        id: &NodeId,
        map: &serde_json::Map<String, JsonSchema>,
        base: &str,
        stripped: bool,
    ) -> Build<Vec<usize>> {
        let mut parts: Vec<usize> = Vec::new();
        if map.get("properties").is_some()
            || map.get("patternProperties").is_some()
            || map.get("additionalProperties").is_some()
        {
            let mut props: Vec<(String, usize)> = Vec::new();
            if let Some(JsonSchema::Object(entries)) = map.get("properties") {
                for key in entries.keys() {
                    let check = self.check(&id.member("properties", key), base)?;
                    props.push((key.clone(), check));
                }
            }
            let mut patterns: Vec<(usize, usize)> = Vec::new();
            if let Some(JsonSchema::Object(entries)) = map.get("patternProperties") {
                for key in entries.keys() {
                    let pattern = regex::compile(key)
                        .map_err(|_| Fail::Invalid(CompileError::InvalidPattern(key.clone())))?;
                    let index = self.push_pattern(pattern);
                    let check = self.check(&id.member("patternProperties", key), base)?;
                    patterns.push((index, check));
                }
            }
            let additional = match map.get("additionalProperties") {
                None => None,
                Some(_) => Some(self.check(&id.child("additionalProperties"), base)?),
            };
            parts.push(self.push_annotating(AnnotatingNode::Object {
                props,
                patterns,
                additional,
            }));
        }
        if map.get("prefixItems").is_some()
            || map.get("items").is_some()
            || map.get("contains").is_some()
        {
            let items_is_array = matches!(map.get("items"), Some(JsonSchema::Array(_)));
            let mut prefix: Vec<NodeId> = Vec::new();
            if items_is_array {
                if let Some(JsonSchema::Array(items)) = map.get("items") {
                    for index in 0..items.len() {
                        prefix.push(id.element("items", index));
                    }
                }
            } else if let Some(JsonSchema::Array(items)) = map.get("prefixItems") {
                for index in 0..items.len() {
                    prefix.push(id.element("prefixItems", index));
                }
            }
            let rest = if items_is_array {
                if map.get("additionalItems").is_some() {
                    Some(id.child("additionalItems"))
                } else {
                    None
                }
            } else if map.get("items").is_some() {
                Some(id.child("items"))
            } else {
                None
            };
            let mut prefix_checks = Vec::new();
            for child in &prefix {
                prefix_checks.push(self.check(child, base)?);
            }
            let rest_check = match &rest {
                None => None,
                Some(child) => Some(self.check(child, base)?),
            };
            let contains = match map.get("contains") {
                None => None,
                Some(_) => Some(self.check(&id.child("contains"), base)?),
            };
            parts.push(self.push_annotating(AnnotatingNode::Array {
                prefix: prefix_checks,
                rest: rest_check,
                contains,
                min_contains: number_of(map.get("minContains")).unwrap_or(1.0),
                max_contains: number_of(map.get("maxContains")).unwrap_or(f64::INFINITY),
            }));
        }
        if let Some(JsonSchema::Array(members)) = map.get("allOf") {
            for index in 0..members.len() {
                let check = self.annotating(&id.element("allOf", index), base)?;
                parts.push(check);
            }
        }
        if let Some(JsonSchema::Array(branches)) = map.get("anyOf") {
            let mut checks = Vec::new();
            for index in 0..branches.len() {
                checks.push(self.annotating(&id.element("anyOf", index), base)?);
            }
            parts.push(self.push_annotating(AnnotatingNode::Any {
                branches: checks,
                exactly_one: false,
            }));
        }
        if let Some(JsonSchema::Array(branches)) = map.get("oneOf") {
            let mut checks = Vec::new();
            for index in 0..branches.len() {
                checks.push(self.annotating(&id.element("oneOf", index), base)?);
            }
            parts.push(self.push_annotating(AnnotatingNode::Any {
                branches: checks,
                exactly_one: true,
            }));
        }
        if map.get("if").is_some() {
            let condition = self.annotating(&id.child("if"), base)?;
            let then_branch = match map.get("then") {
                None => None,
                Some(_) => Some(self.annotating(&id.child("then"), base)?),
            };
            let else_branch = match map.get("else") {
                None => None,
                Some(_) => Some(self.annotating(&id.child("else"), base)?),
            };
            parts.push(self.push_annotating(AnnotatingNode::Conditional {
                condition,
                then_branch,
                else_branch,
            }));
        }
        if let Some(reference) = map.get("$ref").and_then(JsonSchema::as_str) {
            parts.push(self.annotating_reference(reference, base)?);
        }
        if let Some(reference) = map.get("$dynamicRef").and_then(JsonSchema::as_str) {
            parts.push(self.annotating_dynamic_reference(reference, base)?);
        }
        if let Some(JsonSchema::Object(entries)) = map.get("dependentSchemas") {
            for key in entries.keys() {
                let check = self.annotating(&id.member("dependentSchemas", key), base)?;
                parts.push(self.push_annotating(AnnotatingNode::Dependent {
                    key: key.clone(),
                    check,
                }));
            }
        }
        if !stripped {
            if map.get("unevaluatedProperties").is_some() {
                let check = self.check(&id.child("unevaluatedProperties"), base)?;
                parts.push(self.push_annotating(AnnotatingNode::UnevaluatedProperties(check)));
            }
            if map.get("unevaluatedItems").is_some() {
                let check = self.check(&id.child("unevaluatedItems"), base)?;
                parts.push(self.push_annotating(AnnotatingNode::UnevaluatedItems(check)));
            }
        }
        Ok(parts)
    }

    fn annotating_reference(&mut self, reference: &str, base: &str) -> Build<usize> {
        let target = self
            .resolver
            .resolve(reference, base)
            .map_err(Fail::Invalid)?;
        let check = self.annotating(&target.id, &target.base)?;
        if self.dynamic && target.base != base {
            return Ok(self.push_annotating(AnnotatingNode::Scoped {
                uri: target.base,
                check,
            }));
        }
        Ok(check)
    }

    fn annotating_dynamic_reference(&mut self, reference: &str, base: &str) -> Build<usize> {
        let Some((initial, name)) = self.bookended(reference, base)? else {
            return self.annotating_reference(reference, base);
        };
        let fallback = self.annotating(&initial.id, &initial.base)?;
        let slot = self.push_annotating(AnnotatingNode::DynamicRef {
            name: name.clone(),
            fallback,
            targets: Vec::new(),
        });
        self.pending_annotating_dynamic.push((slot, name, fallback));
        Ok(slot)
    }

    /// Compiles the targets of every `$dynamicRef`: the dynamic anchors of the schema, by the base URI they were
    /// indexed under.
    fn resolve_dynamic(&mut self) -> Build<()> {
        loop {
            let anchors_before = self.resolver.dynamic_anchors.len();
            let pending = self.pending_dynamic.clone();
            for (slot, name, fallback) in pending {
                let targets = self.dynamic_targets(&name)?;
                self.tree.checks[slot] = CheckNode::DynamicRef {
                    name: name.clone(),
                    fallback,
                    targets,
                };
            }
            let annotating = self.pending_annotating_dynamic.clone();
            for (slot, name, fallback) in annotating {
                let targets = self.dynamic_annotating_targets(&name)?;
                self.tree.annotating[slot] = AnnotatingNode::DynamicRef {
                    name: name.clone(),
                    fallback,
                    targets,
                };
            }
            if anchors_before == self.resolver.dynamic_anchors.len() {
                return Ok(());
            }
        }
    }

    fn dynamic_annotating_targets(&mut self, name: &str) -> Build<Vec<(String, usize)>> {
        let anchors: Vec<(String, NodeId)> = self
            .resolver
            .dynamic_anchors
            .iter()
            .filter_map(|(key, id)| {
                let (base, anchor) = key.split_once('#')?;
                if anchor == name {
                    Some((base.to_string(), id.clone()))
                } else {
                    None
                }
            })
            .collect();
        let mut targets = Vec::new();
        for (base, id) in anchors {
            let documents = self.documents.clone();
            let node = documents.node_value(&id);
            let own_base = self.resolver.base_for(&id, node, &base);
            // A target that does not compile is not a target: the reference falls back to its own.
            if let Ok(check) = self.annotating(&id, &own_base) {
                targets.push((base, check));
            }
        }
        Ok(targets)
    }

    fn dynamic_targets(&mut self, name: &str) -> Build<Vec<(String, usize)>> {
        let anchors: Vec<(String, NodeId)> = self
            .resolver
            .dynamic_anchors
            .iter()
            .filter_map(|(key, id)| {
                let (base, anchor) = key.split_once('#')?;
                if anchor == name {
                    Some((base.to_string(), id.clone()))
                } else {
                    None
                }
            })
            .collect();
        let mut targets = Vec::new();
        for (base, id) in anchors {
            let documents = self.documents.clone();
            let node = documents.node_value(&id);
            let own_base = self.resolver.base_for(&id, node, &base);
            // A target that does not compile is not a target: the reference falls back to its own.
            if let Ok(check) = self.check(&id, &own_base) {
                targets.push((base, check));
            }
        }
        Ok(targets)
    }
}

/// The tag of a tagged-union branch: a primitive const on a required property of an object schema.
fn tag_of(branch: &JsonSchema, key: &str) -> Option<Value> {
    if branch.get("type").and_then(JsonSchema::as_str) != Some("object") {
        return None;
    }
    let JsonSchema::Array(required) = branch.get("required")? else {
        return None;
    };
    if !required.iter().any(|name| name.as_str() == Some(key)) {
        return None;
    }
    let JsonSchema::Object(properties) = branch.get("properties")? else {
        return None;
    };
    let JsonSchema::Object(tag) = properties.get(key)? else {
        return None;
    };
    let constant = tag.get("const")?;
    if matches!(constant, JsonSchema::Object(_) | JsonSchema::Array(_)) {
        return None;
    }
    Some(Value::from(constant))
}

/// A number keyword's value.
fn number_of(value: Option<&JsonSchema>) -> Option<f64> {
    value.and_then(JsonSchema::as_f64)
}

/// A number keyword's value: a number, or a bigint (TypeBox's `Type.BigInt` bounds, written as a decimal
/// string).
fn bound_of(value: Option<&JsonSchema>) -> Option<Bound> {
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
