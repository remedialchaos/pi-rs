// The public types: the schema compile() accepts, its options, and the errors a failed value produces.

use crate::value::Value;
use indexmap::IndexMap;
use std::sync::Arc;

/// A JSON Schema: `true`, `false` or a schema object.
///
/// Schemas come from tool definitions, MCP servers and other untrusted sources, so compile() reads every keyword
/// defensively instead of trusting a static shape.
pub type JsonSchema = serde_json::Value;

/// A format check: whether `value` (a string) has the format.
pub type FormatCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The draft whose semantics apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Draft {
    /// Draft 2020-12 (the default).
    #[default]
    V2020_12,
    /// Draft 7.
    V7,
}

/// What a `$ref` that names nothing does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UnresolvedRefs {
    /// compile() fails with an error (the default).
    #[default]
    Throw,
    /// The reference compiles as `false`, so only values at that place fail, with the error "unresolved reference".
    Reject,
}

/// The options of compile().
#[derive(Clone, Default)]
pub struct CompileOptions {
    /// The draft whose semantics apply.
    pub draft: Draft,
    /// Format name to check. Without it `format` is an annotation, as draft 2020-12 specifies. The standard
    /// formats are in `crate::formats`.
    pub formats: Option<IndexMap<String, FormatCheck>>,
    /// Schema documents by URI, for `$ref`s to other documents.
    pub schemas: Option<IndexMap<String, JsonSchema>>,
    /// Add `coerce()` and `parse()` to the validator, for loosely typed input.
    pub coerce: bool,
    /// What a `$ref` that names nothing does.
    pub unresolved_refs: UnresolvedRefs,
}

impl CompileOptions {
    /// The options of a draft 2020-12 compile.
    pub fn new() -> Self {
        Self::default()
    }

    /// The draft whose semantics apply.
    pub fn with_draft(mut self, draft: Draft) -> Self {
        self.draft = draft;
        self
    }

    /// The format checks to apply.
    pub fn with_formats(mut self, formats: IndexMap<String, FormatCheck>) -> Self {
        self.formats = Some(formats);
        self
    }

    /// The documents other schemas may reference.
    pub fn with_schemas(mut self, schemas: IndexMap<String, JsonSchema>) -> Self {
        self.schemas = Some(schemas);
        self
    }

    /// Whether the validator coerces loosely typed input.
    pub fn with_coerce(mut self, coerce: bool) -> Self {
        self.coerce = coerce;
        self
    }

    /// What a `$ref` that names nothing does.
    pub fn with_unresolved_refs(mut self, unresolved_refs: UnresolvedRefs) -> Self {
        self.unresolved_refs = unresolved_refs;
        self
    }
}

/// One failure of a value: where it is (a JSON Pointer, "" for the value itself), the failing keyword and a
/// message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// The JSON Pointer of the failing value ("" for the value itself).
    pub path: String,
    /// The keyword that failed.
    pub keyword: String,
    /// What the failure is.
    pub message: String,
}

impl ValidationError {
    /// A failure at `path`.
    pub fn new(
        path: impl Into<String>,
        keyword: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            keyword: keyword.into(),
            message: message.into(),
        }
    }
}

/// The result of `parse()`: the value when it is valid, else the errors of the value.
pub type ParseResult<T> = Result<T, Vec<ValidationError>>;

/// Why a schema does not compile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// A schema that is neither an object nor a boolean.
    NotASchema(String),
    /// A `$ref` that names nothing.
    UnresolvedRef(String),
    /// A regular expression that is not a valid ECMAScript pattern.
    InvalidPattern(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::NotASchema(value) => {
                write!(f, "A schema must be an object or a boolean, got {value}")
            }
            CompileError::UnresolvedRef(reference) => write!(f, "Unresolved $ref {reference}"),
            CompileError::InvalidPattern(source) => {
                write!(f, "Invalid regular expression {source}")
            }
        }
    }
}

impl std::error::Error for CompileError {}

/// The value a compiled schema accepts: the schema itself.
pub type Schema = JsonSchema;

/// The static type of the values a schema accepts; Rust has no type-level brand, so it is `Value`.
pub type Static = Value;
