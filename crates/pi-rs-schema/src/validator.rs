// compile(): a schema to a Validator, whose `check` is the fast boolean path and whose `errors` explains failures.

use crate::checks::CheckTree;
use crate::coerce::Coercer;
use crate::compiler::Compiler;
use crate::explain::Explainer;
use crate::resolver::Documents;
use crate::types::{CompileError, CompileOptions, JsonSchema, ParseResult, ValidationError};
use crate::value::Value;
use std::sync::{Arc, Mutex};

/// Compiles `schema` into a Validator. Fails if the schema is malformed or a `$ref` names nothing (register other
/// documents with `options.schemas`). The schema is read, never modified.
pub fn compile(schema: &JsonSchema) -> Result<Validator, CompileError> {
    compile_with(schema, &CompileOptions::default())
}

/// Compiles `schema` into a Validator, with the given options.
pub fn compile_with(
    schema: &JsonSchema,
    options: &CompileOptions,
) -> Result<Validator, CompileError> {
    let (documents, tree) = build(schema, options)?;
    let validator = Validator {
        schema: Arc::new(schema.clone()),
        options: options.clone(),
        documents,
        tree,
        explainer: Mutex::new(None),
        coercer: Mutex::new(None),
    };
    if options.coerce {
        let coercer = Coercer::new(validator.documents.clone(), options)?;
        *validator
            .coercer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(coercer);
    }
    Ok(validator)
}

/// Compiles `schema` into a validator that also coerces loosely typed input.
pub fn compile_coercing(
    schema: &JsonSchema,
    options: &CompileOptions,
) -> Result<CoercingValidator, CompileError> {
    let mut options = options.clone();
    options.coerce = true;
    let validator = compile_with(schema, &options)?;
    Ok(CoercingValidator { validator })
}

/// Builds the checks of a schema: a compile without the dynamic scope, and one with it when the schema needs it.
fn build(
    schema: &JsonSchema,
    options: &CompileOptions,
) -> Result<(Arc<Documents>, CheckTree), CompileError> {
    let documents = Arc::new(Documents::new(
        Arc::new(schema.clone()),
        options.schemas.as_ref(),
    ));
    let mut compiler = Compiler::new(documents.clone(), options, false);
    match compiler.root() {
        Ok(root) => {
            compiler.tree.root = root;
            Ok((documents, compiler.tree))
        }
        Err(error) => {
            if !compiler.restarted() {
                return Err(error);
            }
            let mut compiler = Compiler::new(documents.clone(), options, true);
            let root = compiler.root()?;
            compiler.tree.root = root;
            Ok((documents, compiler.tree))
        }
    }
}

/// A compiled schema whose valid values have the type of the schema (`Value` unless the schema was built with
/// the builders).
pub struct Validator {
    schema: Arc<JsonSchema>,
    options: CompileOptions,
    documents: Arc<Documents>,
    tree: CheckTree,
    explainer: Mutex<Option<Explainer>>,
    coercer: Mutex<Option<Coercer>>,
}

impl Validator {
    /// The schema the validator was compiled from.
    pub fn schema(&self) -> &JsonSchema {
        &self.schema
    }

    /// The check of the root schema.
    pub fn root_check(&self) -> &crate::checks::CheckNode {
        &self.tree.checks[self.tree.root]
    }

    /// The options the validator was compiled with.
    pub fn options(&self) -> &CompileOptions {
        &self.options
    }

    /// Whether `value` is valid.
    pub fn check(&self, value: &Value) -> bool {
        self.tree.check(value)
    }

    /// Why `value` is invalid: one error per failing keyword, `[]` when it is valid. Built on the first failure
    /// and kept, so a validator whose values always pass never pays for it.
    pub fn errors(&self, value: &Value) -> Vec<ValidationError> {
        if self.check(value) {
            return Vec::new();
        }
        let mut guard = self
            .explainer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Explainer::new(self.schema.clone(), self.options.clone()).ok();
        }
        match guard.as_mut() {
            Some(explainer) => explainer.errors(value),
            None => vec![ValidationError::new("", "schema", "invalid")],
        }
    }

    /// `value` itself when it is valid, else its errors: check() and errors() in one result, without coercion.
    pub fn validate(&self, value: &Value) -> ParseResult<Value> {
        if self.check(value) {
            Ok(value.clone())
        } else {
            Err(self.errors(value))
        }
    }

    /// `value` coerced towards the schema: numeric strings to numbers, `"true"` to `true`, a single value to a
    /// one-item array, an optional `null` dropped, the first union branch the value converts to. Only the changed
    /// paths are copied; `value` comes back itself when nothing changes, and is never modified. Values that
    /// cannot be coerced stay as they are. `None` when the validator was not compiled with coercion.
    pub fn coerce(&self, value: &Value) -> Option<Value> {
        let mut guard = self
            .coercer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Coercer::new(self.documents.clone(), &self.options).ok();
        }
        let coercer = guard.as_mut()?;
        Some(coercer.coerce(value))
    }

    /// The conversion pass alone, as TypeBox's `Value.Convert`: the value converted towards the schema, without
    /// the optional nulls and the primitive coercion. `None` when the validator was not compiled with coercion.
    pub fn convert(&self, value: &Value) -> Option<Value> {
        let mut guard = self
            .coercer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Coercer::new(self.documents.clone(), &self.options).ok();
        }
        let coercer = guard.as_mut()?;
        Some(coercer.convert_only(value))
    }

    /// Coerces `value`, then checks it: the coerced value when it is valid, else its errors.
    pub fn parse(&self, value: &Value) -> ParseResult<Value> {
        let coerced = self.coerce(value).unwrap_or_else(|| value.clone());
        self.validate(&coerced)
    }
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator")
            .field("root", &self.tree.root)
            .finish()
    }
}

/// A validator compiled with coercion: it also coerces loosely typed input towards the schema.
pub struct CoercingValidator {
    validator: Validator,
}

impl std::fmt::Debug for CoercingValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoercingValidator")
            .field("validator", &self.validator)
            .finish()
    }
}

impl CoercingValidator {
    /// The schema the validator was compiled from.
    pub fn schema(&self) -> &JsonSchema {
        self.validator.schema()
    }

    /// Whether `value` is valid.
    pub fn check(&self, value: &Value) -> bool {
        self.validator.check(value)
    }

    /// Why `value` is invalid.
    pub fn errors(&self, value: &Value) -> Vec<ValidationError> {
        self.validator.errors(value)
    }

    /// `value` itself when it is valid, else its errors.
    pub fn validate(&self, value: &Value) -> ParseResult<Value> {
        self.validator.validate(value)
    }

    /// `value` coerced towards the schema.
    pub fn coerce(&self, value: &Value) -> Value {
        self.validator
            .coerce(value)
            .unwrap_or_else(|| value.clone())
    }

    /// The conversion pass alone, as TypeBox's `Value.Convert`.
    pub fn convert(&self, value: &Value) -> Value {
        self.validator
            .convert(value)
            .unwrap_or_else(|| value.clone())
    }

    /// Coerces `value`, then checks it.
    pub fn parse(&self, value: &Value) -> ParseResult<Value> {
        self.validator.parse(value)
    }
}
