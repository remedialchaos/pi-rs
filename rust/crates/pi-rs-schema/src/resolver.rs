// Reference resolution of one compile: schema resources, anchors and JSON Pointers over the root schema and the
// registered documents. A Resolver lives only while a schema compiles (or while its errors are explained).
//
// A schema node is named by the document it lives in and its JSON Pointer path in that document, so the resolver
// never needs the node's address: two paths to the same content are two nodes, as in JSON.

use crate::types::{CompileError, CompileOptions, Draft, JsonSchema};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The base URI of a root schema without `$id`; relative references resolve against it.
pub const DEFAULT_BASE: &str = "https://json-schema.invalid/root.json";

/// The document of the boolean schema `false`, which an unresolved reference compiles to.
const FALSE_DOCUMENT: &str = "\u{0}false";

/// The name of a schema node: the document it lives in and its JSON Pointer path in that document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeId {
    /// The document: empty for the root schema, the URI for a registered document.
    pub document: String,
    /// The JSON Pointer of the node in its document.
    pub path: String,
}

impl NodeId {
    /// The node of the root schema.
    pub fn root() -> Self {
        Self {
            document: String::new(),
            path: String::new(),
        }
    }

    /// The node of the boolean schema `false`.
    pub fn false_schema() -> Self {
        Self {
            document: FALSE_DOCUMENT.to_string(),
            path: String::new(),
        }
    }

    /// The node of a keyword of this node.
    pub fn child(&self, keyword: &str) -> Self {
        Self {
            document: self.document.clone(),
            path: format!("{}/{}", self.path, escape_pointer(keyword)),
        }
    }

    /// The node of a member of a map keyword of this node.
    pub fn member(&self, keyword: &str, key: &str) -> Self {
        Self {
            document: self.document.clone(),
            path: format!(
                "{}/{}/{}",
                self.path,
                escape_pointer(keyword),
                escape_pointer(key)
            ),
        }
    }

    /// The node of an element of an array keyword of this node.
    pub fn element(&self, keyword: &str, index: usize) -> Self {
        Self {
            document: self.document.clone(),
            path: format!("{}/{}/{}", self.path, escape_pointer(keyword), index),
        }
    }
}

/// The schema a reference names, with the base URI it compiles under.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The node the reference names.
    pub id: NodeId,
    /// The base URI the node compiles under.
    pub base: String,
}

/// Escapes a name as a JSON Pointer segment.
pub fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Unescapes a JSON Pointer segment.
pub fn unescape_pointer(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

/// `uri` without its fragment.
pub fn document_of(uri: &str) -> String {
    match uri.find('#') {
        Some(hash) => uri[..hash].to_string(),
        None => uri.to_string(),
    }
}

/// `reference` resolved against `base`. URNs and other bases a URL parser rejects take fragment-only references.
pub fn resolve_uri(reference: &str, base: &str) -> String {
    let parsed = url::Url::parse(base).ok().and_then(|base| {
        url::Url::options()
            .base_url(Some(&base))
            .parse(reference)
            .ok()
    });
    match parsed {
        Some(url) => url.to_string(),
        None => {
            if reference.starts_with('#') {
                format!("{}{}", document_of(base), reference)
            } else {
                reference.to_string()
            }
        }
    }
}

/// The documents a compile reads: the root schema and the registered documents.
pub struct Documents {
    /// The root schema.
    pub root: Arc<JsonSchema>,
    /// The registered documents, by URI.
    registry: HashMap<String, Arc<JsonSchema>>,
    /// The boolean schema `false`, which an unresolved reference compiles to.
    false_schema: JsonSchema,
}

impl Documents {
    /// The documents of a compile.
    pub fn new(
        root: Arc<JsonSchema>,
        schemas: Option<&indexmap::IndexMap<String, JsonSchema>>,
    ) -> Self {
        let mut registry = HashMap::new();
        if let Some(schemas) = schemas {
            for (uri, schema) in schemas {
                registry.insert(
                    document_of(&resolve_uri(uri, DEFAULT_BASE)),
                    Arc::new(schema.clone()),
                );
            }
        }
        Self {
            root,
            registry,
            false_schema: JsonSchema::Bool(false),
        }
    }

    /// The URIs of the registered documents.
    pub fn registry_keys(&self) -> Vec<String> {
        self.registry.keys().cloned().collect()
    }

    /// The schema of a node.
    pub fn node_value(&self, id: &NodeId) -> &JsonSchema {
        if id.document == FALSE_DOCUMENT {
            return &self.false_schema;
        }
        let document: &JsonSchema = if id.document.is_empty() {
            &self.root
        } else {
            match self.registry.get(&id.document) {
                Some(schema) => schema,
                None => return &self.false_schema,
            }
        };
        let mut current = document;
        for segment in id.path.split('/').skip(1) {
            let key = unescape_pointer(segment);
            current = match current {
                JsonSchema::Object(map) => match map.get(&key) {
                    Some(value) => value,
                    None => return &self.false_schema,
                },
                JsonSchema::Array(items) => {
                    match key.parse::<usize>().ok().and_then(|index| items.get(index)) {
                        Some(value) => value,
                        None => return &self.false_schema,
                    }
                }
                _ => return &self.false_schema,
            };
        }
        current
    }
}

/// Resolves references in the root schema and the registered documents.
pub struct Resolver {
    /// The draft whose semantics apply.
    pub draft: Draft,
    /// The documents the schema and its references live in.
    pub documents: Arc<Documents>,
    /// The base URI each node was indexed under.
    base_of: HashMap<NodeId, String>,
    /// The root node of each schema resource, by the base URI it was indexed under.
    resources: HashMap<String, NodeId>,
    /// Every `$anchor` and `$dynamicAnchor`, by "base#name".
    anchors: HashMap<String, NodeId>,
    /// Every `$dynamicAnchor`, by "base#name".
    pub dynamic_anchors: HashMap<String, NodeId>,
    /// The references that named nothing and resolved to `false`.
    unresolved: HashSet<String>,
    /// Whether an unresolved reference compiles as `false`.
    reject_unresolved: bool,
}

impl Resolver {
    /// A resolver over a root schema and its options.
    pub fn new(documents: Arc<Documents>, options: &CompileOptions) -> Self {
        let mut resolver = Self {
            draft: options.draft,
            documents,
            base_of: HashMap::new(),
            resources: HashMap::new(),
            anchors: HashMap::new(),
            dynamic_anchors: HashMap::new(),
            unresolved: HashSet::new(),
            reject_unresolved: matches!(
                options.unresolved_refs,
                crate::types::UnresolvedRefs::Reject
            ),
        };
        resolver.index_all();
        resolver
    }

    /// The base URI of a node, whose parent has `base`: its `$id` changes it, unless draft 7 makes the `$id`
    /// inert.
    pub fn base_for(&self, id: &NodeId, node: &JsonSchema, base: &str) -> String {
        if let Some(indexed) = self.base_of.get(id) {
            return indexed.clone();
        }
        let Some(identifier) = node.get("$id").and_then(JsonSchema::as_str) else {
            return base.to_string();
        };
        if self.draft == Draft::V7
            && (node.get("$ref").and_then(JsonSchema::as_str).is_some()
                || identifier.starts_with('#'))
        {
            return base.to_string();
        }
        document_of(&resolve_uri(identifier, base))
    }

    /// The schema a reference names, with the base URI it compiles under.
    pub fn resolve(&mut self, reference: &str, base: &str) -> Result<Resolved, CompileError> {
        if !self.reject_unresolved {
            return self.find(reference, base);
        }
        match self.find(reference, base) {
            Ok(resolved) => Ok(resolved),
            Err(_) => {
                self.unresolved.insert(resolve_uri(reference, base));
                Ok(Resolved {
                    id: NodeId::false_schema(),
                    base: base.to_string(),
                })
            }
        }
    }

    /// Whether `reference`, from `base`, named nothing and resolved to `false`.
    pub fn is_unresolved(&self, reference: &str, base: &str) -> bool {
        self.unresolved.contains(&resolve_uri(reference, base))
    }

    /// Whether a node is the root of the schema resource at `base`.
    pub fn is_resource(&self, id: &NodeId, base: &str) -> bool {
        self.resources.get(base) == Some(id)
    }

    fn find(&self, reference: &str, base: &str) -> Result<Resolved, CompileError> {
        let uri = resolve_uri(reference, base);
        let document = document_of(&uri);
        let fragment = if uri.len() > document.len() {
            decode_uri_component(&uri[document.len() + 1..])
        } else {
            String::new()
        };
        let Some(root) = self.resources.get(&document) else {
            return Err(CompileError::UnresolvedRef(reference.to_string()));
        };
        if fragment.is_empty() {
            return Ok(Resolved {
                id: root.clone(),
                base: document,
            });
        }
        if fragment.starts_with('/') {
            return self.pointer(root, &document, &fragment);
        }
        let target = self
            .anchors
            .get(&format!("{document}#{fragment}"))
            .ok_or_else(|| CompileError::UnresolvedRef(reference.to_string()))?;
        let base = self
            .base_of
            .get(target)
            .cloned()
            .unwrap_or_else(|| document.clone());
        Ok(Resolved {
            id: target.clone(),
            base,
        })
    }

    fn pointer(&self, root: &NodeId, base: &str, fragment: &str) -> Result<Resolved, CompileError> {
        let mut id = root.clone();
        let mut current_base = base.to_string();
        for raw in fragment.split('/').skip(1) {
            let key = unescape_pointer(raw);
            let node = self.documents.node_value(&id);
            let next = match node {
                JsonSchema::Object(map) => map.get(&key),
                JsonSchema::Array(items) => {
                    key.parse::<usize>().ok().and_then(|index| items.get(index))
                }
                _ => None,
            };
            let Some(next) = next else {
                return Err(CompileError::UnresolvedRef(format!("#{fragment}")));
            };
            id = id.child(&key);
            if let JsonSchema::Object(_) | JsonSchema::Array(_) = next
                && let Some(indexed) = self.base_of.get(&id)
            {
                current_base = indexed.clone();
            }
        }
        Ok(Resolved {
            id,
            base: current_base,
        })
    }

    /// Indexes the root schema and the registered documents.
    fn index_all(&mut self) {
        let root_id = NodeId::root();
        self.resources
            .insert(DEFAULT_BASE.to_string(), root_id.clone());
        self.index(&root_id, DEFAULT_BASE);
        for uri in self.documents.registry_keys() {
            if self.resources.contains_key(&uri) {
                continue;
            }
            let id = NodeId {
                document: uri.clone(),
                path: String::new(),
            };
            self.resources.insert(uri.clone(), id.clone());
            self.index(&id, &uri);
        }
    }

    /// Walks only keywords that hold subschemas: an `$id` inside `enum`, `const` or an unknown keyword names
    /// nothing.
    fn index(&mut self, id: &NodeId, base: &str) {
        if self.base_of.contains_key(id) {
            return;
        }
        // The node is read before the maps change: the borrow of the schema must not outlive it.
        let (identifier, anchor, dynamic_anchor, ref_only) = {
            let node = self.documents.node_value(id);
            if !is_schema_object(node) {
                return;
            }
            (
                node.get("$id")
                    .and_then(JsonSchema::as_str)
                    .map(str::to_string),
                node.get("$anchor")
                    .and_then(JsonSchema::as_str)
                    .map(str::to_string),
                node.get("$dynamicAnchor")
                    .and_then(JsonSchema::as_str)
                    .map(str::to_string),
                self.draft == Draft::V7 && node.get("$ref").and_then(JsonSchema::as_str).is_some(),
            )
        };
        let mut current_base = base.to_string();
        // In draft 7 a `$ref` makes every sibling keyword, `$id` included, inert.
        if !ref_only && let Some(identifier) = identifier.as_deref() {
            if self.draft == Draft::V7 && identifier.starts_with('#') {
                self.anchors.insert(
                    format!("{}{}", document_of(&current_base), identifier),
                    id.clone(),
                );
            } else {
                let uri = resolve_uri(identifier, &current_base);
                current_base = document_of(&uri);
                self.resources.insert(current_base.clone(), id.clone());
                if uri.len() > current_base.len() + 1 {
                    self.anchors.insert(uri, id.clone());
                }
            }
        }
        self.base_of.insert(id.clone(), current_base.clone());
        if ref_only {
            return;
        }
        if self.draft != Draft::V7 {
            if let Some(anchor) = anchor {
                self.anchors
                    .insert(format!("{current_base}#{anchor}"), id.clone());
            }
            if let Some(anchor) = dynamic_anchor {
                self.anchors
                    .insert(format!("{current_base}#{anchor}"), id.clone());
                self.dynamic_anchors
                    .insert(format!("{current_base}#{anchor}"), id.clone());
            }
        }
        let children = {
            let node = self.documents.node_value(id);
            child_ids(id, node)
        };
        for child in children {
            self.index(&child, &current_base);
        }
    }
}

/// The nodes of the subschemas of a node.
fn child_ids(id: &NodeId, node: &JsonSchema) -> Vec<NodeId> {
    let mut children = Vec::new();
    for keyword in SCHEMA_KEYWORDS {
        if node.get(keyword).is_some() {
            children.push(id.child(keyword));
        }
    }
    for keyword in MAP_KEYWORDS {
        if let Some(JsonSchema::Object(map)) = node.get(keyword) {
            for key in map.keys() {
                children.push(id.member(keyword, key));
            }
        }
    }
    for keyword in ARRAY_KEYWORDS {
        if let Some(JsonSchema::Array(items)) = node.get(keyword) {
            for index in 0..items.len() {
                children.push(id.element(keyword, index));
            }
        }
    }
    children
}

/// The keywords that hold one subschema.
const SCHEMA_KEYWORDS: &[&str] = &[
    "additionalProperties",
    "propertyNames",
    "contains",
    "not",
    "if",
    "then",
    "else",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "items",
];

/// The keywords that hold a map of subschemas.
const MAP_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

/// The keywords that hold an array of subschemas.
const ARRAY_KEYWORDS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems", "items"];

/// Whether a value is a schema object (not `true`/`false`, an array or a primitive).
pub fn is_schema_object(value: &JsonSchema) -> bool {
    matches!(value, JsonSchema::Object(_))
}

/// The percent-decoded form of a fragment, as `decodeURIComponent` writes it.
fn decode_uri_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
