// The compiled checks: a schema becomes a tree of checks, built once. Keywords group by the type they
// constrain, so a value is classified once per schema, and the common shapes (closed objects, arrays of one item
// schema, tagged unions, leaf schemas) each get one node.

use crate::regex::Pattern;
use crate::types::FormatCheck;
use crate::value::{Value, cmp_bigint_number, decimal, equal};
use num_bigint::BigInt as NumBigInt;
use std::cmp::Ordering;
use std::collections::HashSet;

/// The named types a `type` keyword accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeName {
    /// A string.
    String,
    /// A finite number.
    Number,
    /// A number with no fractional part.
    Integer,
    /// A boolean.
    Boolean,
    /// `null`.
    Null,
    /// An array.
    Array,
    /// An object.
    Object,
    /// A byte string (TypeBox's `Type.Bytes`).
    Bytes,
    /// A bigint (TypeBox's `Type.BigInt`).
    BigInt,
    /// `undefined` (TypeBox's `Type.Undefined`).
    Undefined,
}

impl TypeName {
    /// The bit of the type in a type mask.
    pub fn bit(self) -> u32 {
        match self {
            TypeName::String => 1,
            TypeName::Number => 2,
            TypeName::Integer => 4,
            TypeName::Boolean => 8,
            TypeName::Null => 16,
            TypeName::Array => 32,
            TypeName::Object => 64,
            TypeName::Bytes => 128,
            TypeName::BigInt => 256,
            TypeName::Undefined => 512,
        }
    }

    /// The type of a name, if it is one.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "string" => Some(TypeName::String),
            "number" => Some(TypeName::Number),
            "integer" => Some(TypeName::Integer),
            "boolean" => Some(TypeName::Boolean),
            "null" => Some(TypeName::Null),
            "array" => Some(TypeName::Array),
            "object" => Some(TypeName::Object),
            "bytes" => Some(TypeName::Bytes),
            "bigint" => Some(TypeName::BigInt),
            "undefined" => Some(TypeName::Undefined),
            _ => None,
        }
    }

    /// The name of the type, as a schema writes it.
    pub fn name(self) -> &'static str {
        match self {
            TypeName::String => "string",
            TypeName::Number => "number",
            TypeName::Integer => "integer",
            TypeName::Boolean => "boolean",
            TypeName::Null => "null",
            TypeName::Array => "array",
            TypeName::Object => "object",
            TypeName::Bytes => "bytes",
            TypeName::BigInt => "bigint",
            TypeName::Undefined => "undefined",
        }
    }

    /// Whether a value has the type.
    pub fn matches(self, value: &Value) -> bool {
        match self {
            TypeName::String => matches!(value, Value::String(_)),
            TypeName::Number => matches!(value, Value::Number(n) if n.is_finite()),
            TypeName::Integer => {
                matches!(value, Value::Number(n) if n.fract() == 0.0 && n.is_finite())
            }
            TypeName::Boolean => matches!(value, Value::Bool(_)),
            TypeName::Null => matches!(value, Value::Null),
            TypeName::Array => matches!(value, Value::Array(_)),
            TypeName::Object => matches!(value, Value::Object(_)),
            TypeName::Bytes => matches!(value, Value::Bytes(_)),
            TypeName::BigInt => matches!(value, Value::BigInt(_)),
            TypeName::Undefined => matches!(value, Value::Undefined),
        }
    }
}

impl PartialEq for CheckNode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CheckNode::Always, CheckNode::Always) => true,
            (CheckNode::Never, CheckNode::Never) => true,
            (CheckNode::Type(left), CheckNode::Type(right)) => left == right,
            (CheckNode::TypeMask(left), CheckNode::TypeMask(right)) => left == right,
            (CheckNode::All(left), CheckNode::All(right)) => left == right,
            (CheckNode::Any(left), CheckNode::Any(right)) => left == right,
            (CheckNode::One(left), CheckNode::One(right)) => left == right,
            (CheckNode::Not(left), CheckNode::Not(right)) => left == right,
            (
                CheckNode::Conditional {
                    condition: left_condition,
                    then_branch: left_then,
                    else_branch: left_else,
                },
                CheckNode::Conditional {
                    condition: right_condition,
                    then_branch: right_then,
                    else_branch: right_else,
                },
            ) => {
                left_condition == right_condition
                    && left_then == right_then
                    && left_else == right_else
            }
            (CheckNode::Const(left), CheckNode::Const(right)) => left == right,
            (CheckNode::Enum(left), CheckNode::Enum(right)) => left == right,
            (
                CheckNode::Number {
                    mode: left_mode,
                    min: left_min,
                    max: left_max,
                    exclusive_min: left_xmin,
                    exclusive_max: left_xmax,
                    multiple_of: left_multiple,
                },
                CheckNode::Number {
                    mode: right_mode,
                    min: right_min,
                    max: right_max,
                    exclusive_min: right_xmin,
                    exclusive_max: right_xmax,
                    multiple_of: right_multiple,
                },
            ) => {
                left_mode == right_mode
                    && left_min == right_min
                    && left_max == right_max
                    && left_xmin == right_xmin
                    && left_xmax == right_xmax
                    && left_multiple == right_multiple
            }
            (
                CheckNode::Str {
                    strict: left_strict,
                    min: left_min,
                    max: left_max,
                    pattern: left_pattern,
                    format: left_format,
                },
                CheckNode::Str {
                    strict: right_strict,
                    min: right_min,
                    max: right_max,
                    pattern: right_pattern,
                    format: right_format,
                },
            ) => {
                left_strict == right_strict
                    && left_min == right_min
                    && left_max == right_max
                    && left_pattern == right_pattern
                    && match (left_format, right_format) {
                        (None, None) => true,
                        (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
                        _ => false,
                    }
            }
            (
                CheckNode::Bytes {
                    min: left,
                    max: left_max,
                },
                CheckNode::Bytes {
                    min: right,
                    max: right_max,
                },
            ) => left == right && left_max == right_max,
            (
                CheckNode::DeclaredObject {
                    entries: left_entries,
                    required_count: left_count,
                    closed: left_closed,
                    guard: left_guard,
                },
                CheckNode::DeclaredObject {
                    entries: right_entries,
                    required_count: right_count,
                    closed: right_closed,
                    guard: right_guard,
                },
            ) => {
                left_entries == right_entries
                    && left_count == right_count
                    && left_closed == right_closed
                    && left_guard == right_guard
            }
            (
                CheckNode::OpenObject {
                    entries: left_entries,
                    keys: left_keys,
                    extra_required: left_extra,
                    patterns: left_patterns,
                    additional: left_additional,
                    guard: left_guard,
                },
                CheckNode::OpenObject {
                    entries: right_entries,
                    keys: right_keys,
                    extra_required: right_extra,
                    patterns: right_patterns,
                    additional: right_additional,
                    guard: right_guard,
                },
            ) => {
                left_entries == right_entries
                    && left_keys == right_keys
                    && left_extra == right_extra
                    && left_patterns == right_patterns
                    && left_additional == right_additional
                    && left_guard == right_guard
            }
            (CheckNode::PropertyNames(left), CheckNode::PropertyNames(right)) => left == right,
            (
                CheckNode::PropertyCount {
                    min: left,
                    max: left_max,
                },
                CheckNode::PropertyCount {
                    min: right,
                    max: right_max,
                },
            ) => left == right && left_max == right_max,
            (
                CheckNode::ArrayLength {
                    min: left,
                    max: left_max,
                },
                CheckNode::ArrayLength {
                    min: right,
                    max: right_max,
                },
            ) => left == right && left_max == right_max,
            (
                CheckNode::GuardObject {
                    inner: left,
                    strict: left_strict,
                },
                CheckNode::GuardObject {
                    inner: right,
                    strict: right_strict,
                },
            ) => left == right && left_strict == right_strict,
            (
                CheckNode::GuardArray {
                    inner: left,
                    strict: left_strict,
                },
                CheckNode::GuardArray {
                    inner: right,
                    strict: right_strict,
                },
            ) => left == right && left_strict == right_strict,
            (CheckNode::DependentRequired(left), CheckNode::DependentRequired(right)) => {
                left == right
            }
            (CheckNode::DependentSchemas(left), CheckNode::DependentSchemas(right)) => {
                left == right
            }
            (
                CheckNode::List {
                    guard: left_guard,
                    min: left_min,
                    max: left_max,
                    prefix: left_prefix,
                    rest: left_rest,
                },
                CheckNode::List {
                    guard: right_guard,
                    min: right_min,
                    max: right_max,
                    prefix: right_prefix,
                    rest: right_rest,
                },
            ) => {
                left_guard == right_guard
                    && left_min == right_min
                    && left_max == right_max
                    && left_prefix == right_prefix
                    && left_rest == right_rest
            }
            (
                CheckNode::Contains {
                    check: left_check,
                    min: left_min,
                    max: left_max,
                },
                CheckNode::Contains {
                    check: right_check,
                    min: right_min,
                    max: right_max,
                },
            ) => left_check == right_check && left_min == right_min && left_max == right_max,
            (CheckNode::UniqueItems, CheckNode::UniqueItems) => true,
            (
                CheckNode::Tagged {
                    key: left_key,
                    tags: left_tags,
                    checks: left_checks,
                },
                CheckNode::Tagged {
                    key: right_key,
                    tags: right_tags,
                    checks: right_checks,
                },
            ) => left_key == right_key && left_tags == right_tags && left_checks == right_checks,
            (
                CheckNode::Scoped {
                    uri: left,
                    check: left_check,
                },
                CheckNode::Scoped {
                    uri: right,
                    check: right_check,
                },
            ) => left == right && left_check == right_check,
            (
                CheckNode::DynamicRef {
                    name: left_name,
                    fallback: left_fallback,
                    targets: left_targets,
                },
                CheckNode::DynamicRef {
                    name: right_name,
                    fallback: right_fallback,
                    targets: right_targets,
                },
            ) => {
                left_name == right_name
                    && left_fallback == right_fallback
                    && left_targets == right_targets
            }
            (CheckNode::Annotating(left), CheckNode::Annotating(right)) => left == right,
            (CheckNode::Slot(left), CheckNode::Slot(right)) => left == right,
            _ => false,
        }
    }
}

impl std::fmt::Debug for CheckNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckNode::Always => f.write_str("Always"),
            CheckNode::Never => f.write_str("Never"),
            CheckNode::Type(name) => write!(f, "Type({name:?})"),
            CheckNode::TypeMask(mask) => write!(f, "TypeMask({mask})"),
            CheckNode::All(parts) => write!(f, "All({parts:?})"),
            CheckNode::Any(parts) => write!(f, "Any({parts:?})"),
            CheckNode::One(parts) => write!(f, "One({parts:?})"),
            CheckNode::Not(child) => write!(f, "Not({child})"),
            CheckNode::Conditional {
                condition,
                then_branch,
                else_branch,
            } => {
                write!(f, "Conditional({condition}, {then_branch}, {else_branch})")
            }
            CheckNode::Const(value) => write!(f, "Const({value:?})"),
            CheckNode::Enum(values) => write!(f, "Enum({values:?})"),
            CheckNode::Number {
                mode,
                min,
                max,
                exclusive_min,
                exclusive_max,
                multiple_of,
            } => write!(
                f,
                "Number({mode:?}, {min:?}, {max:?}, {exclusive_min:?}, {exclusive_max:?}, {multiple_of:?})"
            ),
            CheckNode::Str {
                strict,
                min,
                max,
                pattern,
                format,
            } => write!(
                f,
                "Str({strict}, {min}, {max}, {pattern:?}, {})",
                format.is_some()
            ),
            CheckNode::Bytes { min, max } => write!(f, "Bytes({min}, {max})"),
            CheckNode::DeclaredObject {
                entries,
                required_count,
                closed,
                guard,
            } => {
                write!(
                    f,
                    "DeclaredObject({entries:?}, {required_count}, {closed}, {guard:?})"
                )
            }
            CheckNode::OpenObject {
                entries,
                keys,
                extra_required,
                patterns,
                additional,
                guard,
            } => write!(
                f,
                "OpenObject({entries:?}, {keys:?}, {extra_required:?}, {patterns:?}, {additional:?}, {guard:?})"
            ),
            CheckNode::PropertyNames(child) => write!(f, "PropertyNames({child})"),
            CheckNode::PropertyCount { min, max } => write!(f, "PropertyCount({min}, {max})"),
            CheckNode::ArrayLength { min, max } => write!(f, "ArrayLength({min}, {max})"),
            CheckNode::GuardObject { inner, strict } => write!(f, "GuardObject({inner}, {strict})"),
            CheckNode::GuardArray { inner, strict } => write!(f, "GuardArray({inner}, {strict})"),
            CheckNode::DependentRequired(entries) => write!(f, "DependentRequired({entries:?})"),
            CheckNode::DependentSchemas(entries) => write!(f, "DependentSchemas({entries:?})"),
            CheckNode::List {
                guard,
                min,
                max,
                prefix,
                rest,
            } => {
                write!(f, "List({guard:?}, {min}, {max}, {prefix:?}, {rest:?})")
            }
            CheckNode::Contains { check, min, max } => write!(f, "Contains({check}, {min}, {max})"),
            CheckNode::UniqueItems => f.write_str("UniqueItems"),
            CheckNode::Tagged { key, tags, checks } => {
                write!(f, "Tagged({key:?}, {tags:?}, {checks:?})")
            }
            CheckNode::Scoped { uri, check } => write!(f, "Scoped({uri:?}, {check})"),
            CheckNode::DynamicRef {
                name,
                fallback,
                targets,
            } => {
                write!(f, "DynamicRef({name:?}, {fallback}, {targets:?})")
            }
            CheckNode::Annotating(node) => write!(f, "Annotating({node})"),
            CheckNode::Slot(index) => write!(f, "Slot({index})"),
        }
    }
}

impl PartialEq for AnnotatingNode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AnnotatingNode::Always, AnnotatingNode::Always) => true,
            (AnnotatingNode::Never, AnnotatingNode::Never) => true,
            (AnnotatingNode::Ignore(left), AnnotatingNode::Ignore(right)) => left == right,
            (
                AnnotatingNode::All {
                    plain: left_plain,
                    parts: left_parts,
                },
                AnnotatingNode::All {
                    plain: right_plain,
                    parts: right_parts,
                },
            ) => left_plain == right_plain && left_parts == right_parts,
            (
                AnnotatingNode::Object {
                    props: left_props,
                    patterns: left_patterns,
                    additional: left_additional,
                },
                AnnotatingNode::Object {
                    props: right_props,
                    patterns: right_patterns,
                    additional: right_additional,
                },
            ) => {
                left_props == right_props
                    && left_patterns == right_patterns
                    && left_additional == right_additional
            }
            (
                AnnotatingNode::Array {
                    prefix: left_prefix,
                    rest: left_rest,
                    contains: left_contains,
                    min_contains: left_min,
                    max_contains: left_max,
                },
                AnnotatingNode::Array {
                    prefix: right_prefix,
                    rest: right_rest,
                    contains: right_contains,
                    min_contains: right_min,
                    max_contains: right_max,
                },
            ) => {
                left_prefix == right_prefix
                    && left_rest == right_rest
                    && left_contains == right_contains
                    && left_min == right_min
                    && left_max == right_max
            }
            (
                AnnotatingNode::Any {
                    branches: left,
                    exactly_one: left_exactly,
                },
                AnnotatingNode::Any {
                    branches: right,
                    exactly_one: right_exactly,
                },
            ) => left == right && left_exactly == right_exactly,
            (
                AnnotatingNode::Conditional {
                    condition: left_condition,
                    then_branch: left_then,
                    else_branch: left_else,
                },
                AnnotatingNode::Conditional {
                    condition: right_condition,
                    then_branch: right_then,
                    else_branch: right_else,
                },
            ) => {
                left_condition == right_condition
                    && left_then == right_then
                    && left_else == right_else
            }
            (
                AnnotatingNode::Dependent {
                    key: left,
                    check: left_check,
                },
                AnnotatingNode::Dependent {
                    key: right,
                    check: right_check,
                },
            ) => left == right && left_check == right_check,
            (
                AnnotatingNode::UnevaluatedProperties(left),
                AnnotatingNode::UnevaluatedProperties(right),
            ) => left == right,
            (AnnotatingNode::UnevaluatedItems(left), AnnotatingNode::UnevaluatedItems(right)) => {
                left == right
            }
            (AnnotatingNode::Isolated(left), AnnotatingNode::Isolated(right)) => left == right,
            (
                AnnotatingNode::Scoped {
                    uri: left,
                    check: left_check,
                },
                AnnotatingNode::Scoped {
                    uri: right,
                    check: right_check,
                },
            ) => left == right && left_check == right_check,
            (
                AnnotatingNode::DynamicRef {
                    name: left_name,
                    fallback: left_fallback,
                    targets: left_targets,
                },
                AnnotatingNode::DynamicRef {
                    name: right_name,
                    fallback: right_fallback,
                    targets: right_targets,
                },
            ) => {
                left_name == right_name
                    && left_fallback == right_fallback
                    && left_targets == right_targets
            }
            (AnnotatingNode::Slot(left), AnnotatingNode::Slot(right)) => left == right,
            _ => false,
        }
    }
}

impl std::fmt::Debug for AnnotatingNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AnnotatingNode")
    }
}

impl std::fmt::Debug for CheckTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CheckTree({} checks, {} annotating)",
            self.checks.len(),
            self.annotating.len()
        )
    }
}

/// The type bits of a value: an integer has the bits of `number` and `integer`; NaN and infinity have neither.
pub fn type_bits(value: &Value) -> u32 {
    match value {
        Value::String(_) => TypeName::String.bit(),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.is_finite() {
                TypeName::Number.bit() | TypeName::Integer.bit()
            } else if n.is_finite() {
                TypeName::Number.bit()
            } else {
                0
            }
        }
        Value::Bool(_) => TypeName::Boolean.bit(),
        Value::Null => TypeName::Null.bit(),
        Value::Array(_) => TypeName::Array.bit(),
        Value::Object(_) => TypeName::Object.bit(),
        Value::Bytes(_) => TypeName::Bytes.bit(),
        Value::BigInt(_) => TypeName::BigInt.bit(),
        Value::Undefined => TypeName::Undefined.bit(),
    }
}

/// Whether a value is a JSON object: not null, an array or bytes.
pub fn is_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// Whether a combined object or array check also checks the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    /// Not at all.
    None,
    /// Strictly: another type fails.
    Strict,
    /// Loosely: another type passes.
    Loose,
}

/// The mode of the number keywords.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberMode {
    /// Bounds only: other types pass.
    Loose,
    /// A number.
    Number,
    /// An integer.
    Integer,
    /// A bigint.
    BigInt,
}

/// A bound of a number or a bigint.
#[derive(Clone, Debug)]
pub enum Bound {
    /// A number.
    Number(f64),
    /// A bigint.
    BigInt(NumBigInt),
}

impl Bound {
    /// The bound that admits everything.
    pub fn negative_infinity() -> Self {
        Bound::Number(f64::NEG_INFINITY)
    }

    /// The bound that admits everything.
    pub fn positive_infinity() -> Self {
        Bound::Number(f64::INFINITY)
    }
}

impl PartialEq for Bound {
    fn eq(&self, other: &Self) -> bool {
        compare_bound(self, other) == Some(Ordering::Equal)
    }
}

/// Compares two bounds as JavaScript compares them: the mathematical value of both.
pub fn compare_bound(a: &Bound, b: &Bound) -> Option<Ordering> {
    match (a, b) {
        (Bound::Number(x), Bound::Number(y)) => x.partial_cmp(y),
        (Bound::BigInt(x), Bound::BigInt(y)) => Some(x.cmp(y)),
        (Bound::BigInt(x), Bound::Number(y)) => cmp_bigint_number(x, *y),
        (Bound::Number(x), Bound::BigInt(y)) => cmp_bigint_number(y, *x).map(Ordering::reverse),
    }
}

/// Whether `value` is a multiple of `divisor`, exactly for decimals: 0.0075 / 0.0001 is 74.99999999999999 in
/// floating point.
pub fn is_multiple(value: &Bound, divisor: &Bound) -> bool {
    if matches!(value, Bound::BigInt(_)) || matches!(divisor, Bound::BigInt(_)) {
        let (Some(value), Some(divisor)) = (integral_of(value), integral_of(divisor)) else {
            return false;
        };
        return divisor != NumBigInt::from(0) && (&value % &divisor) == NumBigInt::from(0);
    }
    let (Bound::Number(x), Bound::Number(d)) = (value, divisor) else {
        return false;
    };
    let q = x / d;
    if q.fract() == 0.0 && q.is_finite() {
        return true;
    }
    if !q.is_finite() {
        return x % d == 0.0;
    }
    let (vc, ve) = decimal(*x);
    let (dc, de) = decimal(*d);
    if dc == NumBigInt::from(0) {
        return false;
    }
    if ve >= de {
        let scale = NumBigInt::from(10).pow((ve - de) as u32);
        (&vc * scale) % &dc == NumBigInt::from(0)
    } else {
        let scale = NumBigInt::from(10).pow((de - ve) as u32);
        &vc % (&dc * scale) == NumBigInt::from(0)
    }
}

/// The exact integer value of a bound, when it has one.
fn integral_of(bound: &Bound) -> Option<NumBigInt> {
    match bound {
        Bound::BigInt(b) => Some(b.clone()),
        Bound::Number(n) => {
            if !n.is_finite() || n.fract() != 0.0 {
                return None;
            }
            crate::value::bigint_of_f64(*n)
        }
    }
}

/// One declared property of an object check.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectEntry {
    /// The name of the property.
    pub key: String,
    /// The check of the property's value.
    pub check: usize,
    /// Whether the property is required.
    pub required: bool,
}

/// One compiled check.
#[derive(Clone)]
pub enum CheckNode {
    /// Every value passes.
    Always,
    /// No value passes.
    Never,
    /// A value of one named type.
    Type(TypeName),
    /// A value of any of the named types.
    TypeMask(u32),
    /// Every check passes.
    All(Vec<usize>),
    /// Any check passes.
    Any(Vec<usize>),
    /// Exactly one check passes.
    One(Vec<usize>),
    /// The check must not pass.
    Not(usize),
    /// `if`, `then` and `else`.
    Conditional {
        /// The condition.
        condition: usize,
        /// The branch taken when the condition passes.
        then_branch: usize,
        /// The branch taken when the condition fails.
        else_branch: usize,
    },
    /// `const`.
    Const(Value),
    /// `enum`.
    Enum(Vec<Value>),
    /// The number keywords.
    Number {
        /// The kind of number the schema requires.
        mode: NumberMode,
        /// The lowest accepted value.
        min: Bound,
        /// The highest accepted value.
        max: Bound,
        /// The exclusive lowest accepted value.
        exclusive_min: Bound,
        /// The exclusive highest accepted value.
        exclusive_max: Bound,
        /// The divisor of `multipleOf`.
        multiple_of: Option<Bound>,
    },
    /// The string keywords.
    Str {
        /// Whether the schema asserts the type (a `type: "string"` schema does).
        strict: bool,
        /// The least accepted length, in code points.
        min: f64,
        /// The greatest accepted length, in code points.
        max: f64,
        /// The `pattern` to match.
        pattern: Option<usize>,
        /// The `format` to hold.
        format: Option<FormatCheck>,
    },
    /// The bytes keywords.
    Bytes {
        /// The least accepted length.
        min: f64,
        /// The greatest accepted length.
        max: f64,
    },
    /// An object with declared properties only.
    DeclaredObject {
        /// The declared properties.
        entries: Vec<ObjectEntry>,
        /// How many of them are required.
        required_count: usize,
        /// Whether an undeclared property fails.
        closed: bool,
        /// Whether the check also checks the type.
        guard: Guard,
    },
    /// An object with pattern and additional properties.
    OpenObject {
        /// The declared properties.
        entries: Vec<ObjectEntry>,
        /// The names of the declared properties.
        keys: Vec<String>,
        /// The required names that are not declared.
        extra_required: Vec<String>,
        /// The `patternProperties` and their checks.
        patterns: Vec<(usize, usize)>,
        /// The check of the properties no pattern names.
        additional: Option<usize>,
        /// Whether the check also checks the type.
        guard: Guard,
    },
    /// Every property name passes the check.
    PropertyNames(usize),
    /// `minProperties` and `maxProperties`.
    PropertyCount {
        /// The least accepted number of properties.
        min: f64,
        /// The greatest accepted number of properties.
        max: f64,
    },
    /// `minItems` and `maxItems`.
    ArrayLength {
        /// The least accepted number of items.
        min: f64,
        /// The greatest accepted number of items.
        max: f64,
    },
    /// An object check: other values pass unless `strict`.
    GuardObject {
        /// The check of an object.
        inner: usize,
        /// Whether another type fails.
        strict: bool,
    },
    /// An array check: other values pass unless `strict`.
    GuardArray {
        /// The check of an array.
        inner: usize,
        /// Whether another type fails.
        strict: bool,
    },
    /// `dependentRequired`.
    DependentRequired(Vec<(String, Vec<String>)>),
    /// `dependentSchemas`.
    DependentSchemas(Vec<(String, usize)>),
    /// An array: its prefix items and the items after them.
    List {
        /// Whether the check also checks the type.
        guard: Guard,
        /// The least accepted number of items.
        min: f64,
        /// The greatest accepted number of items.
        max: f64,
        /// The checks of the prefix items.
        prefix: Vec<usize>,
        /// The check of the items after the prefix.
        rest: Option<usize>,
    },
    /// `contains` with its bounds.
    Contains {
        /// The check a matching item must pass.
        check: usize,
        /// The least accepted number of matching items.
        min: f64,
        /// The greatest accepted number of matching items.
        max: f64,
    },
    /// `uniqueItems`.
    UniqueItems,
    /// A union of objects tagged by a const on one key.
    Tagged {
        /// The key that holds the tag.
        key: String,
        /// The tag of each branch.
        tags: Vec<Value>,
        /// The check of each branch.
        checks: Vec<usize>,
    },
    /// A check that pushes a URI onto the dynamic scope while it runs.
    Scoped {
        /// The URI of the resource.
        uri: String,
        /// The check to run.
        check: usize,
    },
    /// A `$dynamicRef`.
    DynamicRef {
        /// The name of the dynamic anchor.
        name: String,
        /// The check of the initial target.
        fallback: usize,
        /// The check of each dynamic anchor, by the base URI it was indexed under.
        targets: Vec<(String, usize)>,
    },
    /// An annotating check run as a plain check, with fresh annotations.
    Annotating(usize),
    /// The check of another node: the indirection a recursive reference goes through.
    Slot(usize),
}

/// What a schema evaluated: property names, the evaluated item prefix, the items matched by `contains`.
#[derive(Clone, Debug, Default)]
pub struct Annotations {
    props: Option<HashSet<String>>,
    all_props: bool,
    items: usize,
    all_items: bool,
    indexes: Option<HashSet<usize>>,
}

impl Annotations {
    /// Empty annotations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a property was evaluated.
    pub fn has_prop(&self, key: &str) -> bool {
        self.props.as_ref().is_some_and(|props| props.contains(key))
    }

    /// Whether every property was evaluated.
    pub fn all_props(&self) -> bool {
        self.all_props
    }

    /// The number of evaluated leading items.
    pub fn items(&self) -> usize {
        self.items
    }

    /// Whether every item was evaluated.
    pub fn all_items(&self) -> bool {
        self.all_items
    }

    /// Whether an item was evaluated.
    pub fn has_index(&self, index: usize) -> bool {
        self.indexes
            .as_ref()
            .is_some_and(|indexes| indexes.contains(&index))
    }

    fn add_prop(&mut self, key: &str) {
        self.props
            .get_or_insert_with(HashSet::new)
            .insert(key.to_string());
    }

    fn add_index(&mut self, index: usize) {
        self.indexes.get_or_insert_with(HashSet::new).insert(index);
    }

    fn merge(&mut self, other: &Annotations) {
        if let Some(props) = &other.props {
            for key in props {
                self.add_prop(key);
            }
        }
        self.all_props |= other.all_props;
        if other.items > self.items {
            self.items = other.items;
        }
        self.all_items |= other.all_items;
        if let Some(indexes) = &other.indexes {
            for index in indexes {
                self.add_index(*index);
            }
        }
    }
}

/// One compiled annotating check.
#[derive(Clone)]
pub enum AnnotatingNode {
    /// Every value passes.
    Always,
    /// No value passes.
    Never,
    /// A plain check: it evaluates nothing that `unevaluated*` sees.
    Ignore(usize),
    /// The non-annotating keywords, then each annotating part in order.
    All {
        /// The plain check.
        plain: usize,
        /// The annotating parts.
        parts: Vec<usize>,
    },
    /// `properties`, `patternProperties` and `additionalProperties`.
    Object {
        /// The declared properties and their checks.
        props: Vec<(String, usize)>,
        /// The `patternProperties` and their checks.
        patterns: Vec<(usize, usize)>,
        /// The check of the properties no pattern names.
        additional: Option<usize>,
    },
    /// `prefixItems`/`items`/`additionalItems` and `contains` with its bounds.
    Array {
        /// The checks of the prefix items.
        prefix: Vec<usize>,
        /// The check of the items after the prefix.
        rest: Option<usize>,
        /// The check a matching item must pass.
        contains: Option<usize>,
        /// The least accepted number of matching items.
        min_contains: f64,
        /// The greatest accepted number of matching items.
        max_contains: f64,
    },
    /// `anyOf` (or `oneOf`): every passing branch contributes its annotations.
    Any {
        /// The branches.
        branches: Vec<usize>,
        /// Whether exactly one branch must pass.
        exactly_one: bool,
    },
    /// `if`, `then` and `else`.
    Conditional {
        /// The condition.
        condition: usize,
        /// The branch taken when the condition passes.
        then_branch: Option<usize>,
        /// The branch taken when the condition fails.
        else_branch: Option<usize>,
    },
    /// One `dependentSchemas` entry.
    Dependent {
        /// The key that triggers the schema.
        key: String,
        /// The check of the schema.
        check: usize,
    },
    /// `unevaluatedProperties`: every property no other keyword evaluated passes the check.
    UnevaluatedProperties(usize),
    /// `unevaluatedItems`: every item no other keyword evaluated passes the check.
    UnevaluatedItems(usize),
    /// The annotations of its own schema, never those of its siblings in the parent.
    Isolated(usize),
    /// A check that pushes a URI onto the dynamic scope while it runs.
    Scoped {
        /// The URI of the resource.
        uri: String,
        /// The check to run.
        check: usize,
    },
    /// A `$dynamicRef`.
    DynamicRef {
        /// The name of the dynamic anchor.
        name: String,
        /// The check of the initial target.
        fallback: usize,
        /// The check of each dynamic anchor, by the base URI it was indexed under.
        targets: Vec<(String, usize)>,
    },
    /// The annotating check of another node: the indirection a recursive reference goes through.
    Slot(usize),
}

/// The compiled checks of a schema.
#[derive(Clone, Default)]
pub struct CheckTree {
    /// The plain checks.
    pub checks: Vec<CheckNode>,
    /// The annotating checks.
    pub annotating: Vec<AnnotatingNode>,
    /// The regular expressions the checks use.
    pub patterns: Vec<Pattern>,
    /// The check of the root schema.
    pub root: usize,
}

impl CheckTree {
    /// Whether a value passes the check.
    pub fn check(&self, value: &Value) -> bool {
        let mut scope = Vec::new();
        self.eval(self.root, value, &mut scope)
    }

    /// Whether a value passes a check.
    pub fn eval(&self, node: usize, value: &Value, scope: &mut Vec<String>) -> bool {
        match &self.checks[node] {
            CheckNode::Always => true,
            CheckNode::Never => false,
            CheckNode::Type(name) => name.matches(value),
            CheckNode::TypeMask(mask) => type_bits(value) & mask != 0,
            CheckNode::All(parts) => parts.iter().all(|part| self.eval(*part, value, scope)),
            CheckNode::Any(parts) => parts.iter().any(|part| self.eval(*part, value, scope)),
            CheckNode::One(parts) => {
                let mut matched = 0;
                for part in parts {
                    if self.eval(*part, value, scope) {
                        matched += 1;
                        if matched > 1 {
                            return false;
                        }
                    }
                }
                matched == 1
            }
            CheckNode::Not(child) => !self.eval(*child, value, scope),
            CheckNode::Conditional {
                condition,
                then_branch,
                else_branch,
            } => {
                if self.eval(*condition, value, scope) {
                    self.eval(*then_branch, value, scope)
                } else {
                    self.eval(*else_branch, value, scope)
                }
            }
            CheckNode::Const(expected) => const_matches(expected, value),
            CheckNode::Enum(values) => values.iter().any(|expected| const_matches(expected, value)),
            CheckNode::Number {
                mode,
                min,
                max,
                exclusive_min,
                exclusive_max,
                multiple_of,
            } => number_ok(
                value,
                *mode,
                min,
                max,
                exclusive_min,
                exclusive_max,
                multiple_of.as_ref(),
            ),
            CheckNode::Str {
                strict,
                min,
                max,
                pattern,
                format,
            } => {
                let Value::String(text) = value else {
                    return !strict;
                };
                let points = crate::value::code_points(text) as f64;
                if points < *min || points > *max {
                    return false;
                }
                if let Some(pattern) = pattern
                    && !self.patterns[*pattern].is_match(text)
                {
                    return false;
                }
                match format {
                    Some(format) => format(text),
                    None => true,
                }
            }
            CheckNode::Bytes { min, max } => match value {
                Value::Bytes(bytes) => {
                    let length = bytes.len() as f64;
                    length >= *min && length <= *max
                }
                _ => false,
            },
            CheckNode::DeclaredObject {
                entries,
                required_count,
                closed,
                guard,
            } => self.eval_declared_object(entries, *required_count, *closed, *guard, value, scope),
            CheckNode::OpenObject {
                entries,
                keys,
                extra_required,
                patterns,
                additional,
                guard,
            } => self.eval_open_object(
                entries,
                keys,
                extra_required,
                patterns,
                *additional,
                *guard,
                value,
                scope,
            ),
            CheckNode::PropertyNames(child) => match value {
                Value::Object(map) => map
                    .keys()
                    .all(|key| self.eval(*child, &Value::String(key.clone()), scope)),
                _ => true,
            },
            CheckNode::PropertyCount { min, max } => match value {
                Value::Object(map) => {
                    let count = map.len() as f64;
                    count >= *min && count <= *max
                }
                _ => true,
            },
            CheckNode::ArrayLength { min, max } => match value {
                Value::Array(items) => {
                    let count = items.len() as f64;
                    count >= *min && count <= *max
                }
                _ => true,
            },
            CheckNode::GuardObject { inner, strict } => {
                if is_object(value) {
                    self.eval(*inner, value, scope)
                } else {
                    !strict
                }
            }
            CheckNode::GuardArray { inner, strict } => match value {
                Value::Array(_) => self.eval(*inner, value, scope),
                _ => !strict,
            },
            CheckNode::DependentRequired(entries) => match value {
                Value::Object(map) => entries.iter().all(|(key, required)| {
                    !map.contains_key(key) || required.iter().all(|name| map.contains_key(name))
                }),
                _ => true,
            },
            CheckNode::DependentSchemas(entries) => match value {
                Value::Object(map) => entries
                    .iter()
                    .all(|(key, check)| !map.contains_key(key) || self.eval(*check, value, scope)),
                _ => true,
            },
            CheckNode::List {
                guard,
                min,
                max,
                prefix,
                rest,
            } => {
                let Value::Array(items) = value else {
                    return *guard == Guard::Loose;
                };
                let count = items.len() as f64;
                if count < *min || count > *max {
                    return false;
                }
                let matched = items.len().min(prefix.len());
                for (index, check) in prefix.iter().enumerate().take(matched) {
                    if !self.eval(*check, &items[index], scope) {
                        return false;
                    }
                }
                match rest {
                    Some(rest) => {
                        for item in items.iter().skip(prefix.len()) {
                            if !self.eval(*rest, item, scope) {
                                return false;
                            }
                        }
                        true
                    }
                    None => true,
                }
            }
            CheckNode::Contains { check, min, max } => {
                let Value::Array(items) = value else {
                    return true;
                };
                let mut matched = 0.0;
                for item in items {
                    if self.eval(*check, item, scope) {
                        matched += 1.0;
                        if matched > *max {
                            return false;
                        }
                    }
                }
                matched >= *min
            }
            CheckNode::UniqueItems => {
                let Value::Array(items) = value else {
                    return true;
                };
                for index in 1..items.len() {
                    for other in 0..index {
                        if equal(&items[index], &items[other]) {
                            return false;
                        }
                    }
                }
                true
            }
            CheckNode::Tagged { key, tags, checks } => {
                let Value::Object(map) = value else {
                    return false;
                };
                let Some(tag) = map.get(key) else {
                    return false;
                };
                for (index, candidate) in tags.iter().enumerate() {
                    if const_matches(candidate, tag) {
                        return self.eval(checks[index], value, scope);
                    }
                }
                false
            }
            CheckNode::Scoped { uri, check } => {
                scope.push(uri.clone());
                let result = self.eval(*check, value, scope);
                scope.pop();
                result
            }
            CheckNode::DynamicRef {
                fallback, targets, ..
            } => {
                for uri in scope.iter() {
                    if let Some((_, target)) =
                        targets.iter().find(|(candidate, _)| candidate == uri)
                    {
                        return self.eval(*target, value, scope);
                    }
                }
                self.eval(*fallback, value, scope)
            }
            CheckNode::Slot(index) => self.eval(*index, value, scope),
            CheckNode::Annotating(node) => {
                let mut annotations = Annotations::new();
                self.eval_annotating(*node, value, &mut annotations, scope)
            }
        }
    }

    fn eval_declared_object(
        &self,
        entries: &[ObjectEntry],
        required_count: usize,
        closed: bool,
        guard: Guard,
        value: &Value,
        scope: &mut Vec<String>,
    ) -> bool {
        if guard != Guard::None && !is_object(value) {
            return guard == Guard::Loose;
        }
        let Value::Object(map) = value else {
            return true;
        };
        let mut required = 0;
        for (key, item) in map {
            let Some(entry) = entries.iter().find(|entry| entry.key == *key) else {
                if closed {
                    return false;
                }
                continue;
            };
            if matches!(item, Value::Undefined) && !entry.required {
                continue;
            }
            if !self.eval(entry.check, item, scope) {
                return false;
            }
            if entry.required {
                required += 1;
            }
        }
        required == required_count
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the keywords of an object check, as the upstream names them"
    )]
    fn eval_open_object(
        &self,
        entries: &[ObjectEntry],
        keys: &[String],
        extra_required: &[String],
        patterns: &[(usize, usize)],
        additional: Option<usize>,
        guard: Guard,
        value: &Value,
        scope: &mut Vec<String>,
    ) -> bool {
        if guard != Guard::None && !is_object(value) {
            return guard == Guard::Loose;
        }
        let Value::Object(map) = value else {
            return true;
        };
        for entry in entries {
            let present = map.get(&entry.key);
            let absent = match present {
                None => true,
                Some(item) => {
                    matches!(item, Value::Undefined)
                        && !(entry.required && map.contains_key(&entry.key))
                }
            };
            if absent {
                if entry.required {
                    return false;
                }
                continue;
            }
            let item = present.expect("a present property is not absent");
            if !self.eval(entry.check, item, scope) {
                return false;
            }
        }
        for name in extra_required {
            if !map.contains_key(name) {
                return false;
            }
        }
        if patterns.is_empty() && additional.is_none() {
            return true;
        }
        for (key, item) in map {
            let mut matched = keys.iter().any(|known| known == key);
            for (pattern, check) in patterns {
                if self.patterns[*pattern].is_match(key) {
                    matched = true;
                    if !self.eval(*check, item, scope) {
                        return false;
                    }
                }
            }
            if !matched
                && let Some(additional) = additional
                && !self.eval(additional, item, scope)
            {
                return false;
            }
        }
        true
    }

    /// Whether a value passes an annotating check, recording what it evaluated.
    pub fn eval_annotating(
        &self,
        node: usize,
        value: &Value,
        annotations: &mut Annotations,
        scope: &mut Vec<String>,
    ) -> bool {
        match &self.annotating[node] {
            AnnotatingNode::Always => true,
            AnnotatingNode::Never => false,
            AnnotatingNode::Ignore(child) => self.eval(*child, value, scope),
            AnnotatingNode::All { plain, parts } => {
                if !self.eval(*plain, value, scope) {
                    return false;
                }
                for part in parts {
                    if !self.eval_annotating(*part, value, annotations, scope) {
                        return false;
                    }
                }
                true
            }
            AnnotatingNode::Object {
                props,
                patterns,
                additional,
            } => {
                let Value::Object(map) = value else {
                    return true;
                };
                let mut seen: Vec<&String> = Vec::new();
                for (key, check) in props {
                    if let Some(item) = map.get(key) {
                        if !self.eval(*check, item, scope) {
                            return false;
                        }
                        seen.push(key);
                    }
                }
                for (key, item) in map {
                    let mut matched = props.iter().any(|(declared, _)| declared == key);
                    for (pattern, check) in patterns {
                        if self.patterns[*pattern].is_match(key) {
                            if !self.eval(*check, item, scope) {
                                return false;
                            }
                            matched = true;
                            seen.push(key);
                        }
                    }
                    if !matched && let Some(additional) = additional {
                        if !self.eval(*additional, item, scope) {
                            return false;
                        }
                        seen.push(key);
                    }
                }
                for key in seen {
                    annotations.add_prop(key);
                }
                true
            }
            AnnotatingNode::Array {
                prefix,
                rest,
                contains,
                min_contains,
                max_contains,
            } => {
                let Value::Array(items) = value else {
                    return true;
                };
                let matched = items.len().min(prefix.len());
                for (index, check) in prefix.iter().enumerate().take(matched) {
                    if !self.eval(*check, &items[index], scope) {
                        return false;
                    }
                }
                if let Some(rest) = rest {
                    for item in items.iter().skip(prefix.len()) {
                        if !self.eval(*rest, item, scope) {
                            return false;
                        }
                    }
                    annotations.all_items = true;
                }
                if matched > annotations.items {
                    annotations.items = matched;
                }
                if let Some(contains) = contains {
                    let mut count = 0.0;
                    for (index, item) in items.iter().enumerate() {
                        if self.eval(*contains, item, scope) {
                            count += 1.0;
                            annotations.add_index(index);
                        }
                    }
                    if count < *min_contains || count > *max_contains {
                        return false;
                    }
                }
                true
            }
            AnnotatingNode::Any {
                branches,
                exactly_one,
            } => {
                let mut matched = 0;
                for branch in branches {
                    let mut own = Annotations::new();
                    if self.eval_annotating(*branch, value, &mut own, scope) {
                        matched += 1;
                        annotations.merge(&own);
                    }
                }
                if *exactly_one {
                    matched == 1
                } else {
                    matched > 0
                }
            }
            AnnotatingNode::Conditional {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut own = Annotations::new();
                if self.eval_annotating(*condition, value, &mut own, scope) {
                    annotations.merge(&own);
                    match then_branch {
                        Some(branch) => self.eval_annotating(*branch, value, annotations, scope),
                        None => true,
                    }
                } else {
                    match else_branch {
                        Some(branch) => self.eval_annotating(*branch, value, annotations, scope),
                        None => true,
                    }
                }
            }
            AnnotatingNode::Dependent { key, check } => {
                let Value::Object(map) = value else {
                    return true;
                };
                !map.contains_key(key) || self.eval_annotating(*check, value, annotations, scope)
            }
            AnnotatingNode::UnevaluatedProperties(check) => {
                let Value::Object(map) = value else {
                    return true;
                };
                if annotations.all_props {
                    return true;
                }
                for (key, item) in map {
                    if !annotations.has_prop(key) && !self.eval(*check, item, scope) {
                        return false;
                    }
                }
                annotations.all_props = true;
                true
            }
            AnnotatingNode::UnevaluatedItems(check) => {
                let Value::Array(items) = value else {
                    return true;
                };
                if annotations.all_items {
                    return true;
                }
                for (index, item) in items.iter().enumerate().skip(annotations.items) {
                    if !annotations.has_index(index) && !self.eval(*check, item, scope) {
                        return false;
                    }
                }
                annotations.all_items = true;
                true
            }
            AnnotatingNode::Isolated(child) => {
                let mut own = Annotations::new();
                if !self.eval_annotating(*child, value, &mut own, scope) {
                    return false;
                }
                annotations.merge(&own);
                true
            }
            AnnotatingNode::Scoped { uri, check } => {
                scope.push(uri.clone());
                let result = self.eval_annotating(*check, value, annotations, scope);
                scope.pop();
                result
            }
            AnnotatingNode::Slot(index) => self.eval_annotating(*index, value, annotations, scope),
            AnnotatingNode::DynamicRef {
                fallback, targets, ..
            } => {
                for uri in scope.iter() {
                    if let Some((_, target)) =
                        targets.iter().find(|(candidate, _)| candidate == uri)
                    {
                        return self.eval_annotating(*target, value, annotations, scope);
                    }
                }
                self.eval_annotating(*fallback, value, annotations, scope)
            }
        }
    }
}

/// Whether a value is the `const` value: the same primitives, or deep equality for objects and arrays.
fn const_matches(expected: &Value, value: &Value) -> bool {
    match expected {
        Value::Object(_) | Value::Array(_) => equal(expected, value),
        Value::Number(x) => match value {
            Value::Number(y) => x == y,
            _ => false,
        },
        Value::BigInt(x) => match value {
            Value::BigInt(y) => x == y,
            _ => false,
        },
        Value::Bytes(x) => match value {
            Value::Bytes(y) => x == y,
            _ => false,
        },
        _ => expected == value,
    }
}

/// The number keywords: bounds of either kind compare with values of either kind.
fn number_ok(
    value: &Value,
    mode: NumberMode,
    min: &Bound,
    max: &Bound,
    exclusive_min: &Bound,
    exclusive_max: &Bound,
    multiple_of: Option<&Bound>,
) -> bool {
    let bound = match value {
        Value::Number(n) => Bound::Number(*n),
        Value::BigInt(b) => Bound::BigInt(b.clone()),
        _ => return mode == NumberMode::Loose,
    };
    match mode {
        NumberMode::BigInt => {
            if !matches!(value, Value::BigInt(_)) {
                return false;
            }
        }
        NumberMode::Number | NumberMode::Integer => match value {
            Value::Number(n) if !n.is_finite() => return false,
            Value::BigInt(_) => return false,
            Value::Number(n) if mode == NumberMode::Integer && n.fract() != 0.0 => return false,
            _ => {}
        },
        NumberMode::Loose => {}
    }
    let at_least = compare_bound(&bound, min).is_some_and(|ordering| ordering != Ordering::Less);
    let at_most = compare_bound(&bound, max).is_some_and(|ordering| ordering != Ordering::Greater);
    let above =
        compare_bound(&bound, exclusive_min).is_some_and(|ordering| ordering == Ordering::Greater);
    let below =
        compare_bound(&bound, exclusive_max).is_some_and(|ordering| ordering == Ordering::Less);
    at_least
        && at_most
        && above
        && below
        && multiple_of.is_none_or(|divisor| is_multiple(&bound, divisor))
}

/// The bigint type of a value, for callers that need one.
pub fn bigint_of(value: &Value) -> Option<&NumBigInt> {
    value.as_bigint()
}
