// Schema builders: short functions that return plain JSON Schema objects. The objects are ordinary data: they
// serialize with `serde_json` and compile with `compile()` like any other schema.
//
// Rust has no type-level brand, so the static type a builder carries in TypeScript is not represented: every
// builder returns a `JsonSchema`.

use crate::types::JsonSchema;
use serde_json::{Map, Value, json};

/// A number as JSON writes it: an integral number has no fractional part.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        return json!(value as i64);
    }
    json!(value)
}

/// The annotations every builder accepts: they describe the schema and never change what it accepts.
#[derive(Clone, Debug, Default)]
pub struct Annotations {
    /// The title of the schema.
    pub title: Option<String>,
    /// The description of the schema.
    pub description: Option<String>,
    /// The default value, an annotation for tools and forms; validation never inserts it.
    pub default: Option<Value>,
    /// The examples of the schema.
    pub examples: Option<Vec<Value>>,
    /// Whether the schema is deprecated.
    pub deprecated: Option<bool>,
}

impl Annotations {
    /// The annotations of a schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The title of the schema.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The description of the schema.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The default value of the schema.
    pub fn default_value(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    /// Whether the schema is deprecated.
    pub fn deprecated(mut self, deprecated: bool) -> Self {
        self.deprecated = Some(deprecated);
        self
    }

    fn apply(&self, mut schema: Map<String, Value>) -> Map<String, Value> {
        if let Some(title) = &self.title {
            schema.insert("title".to_string(), Value::String(title.clone()));
        }
        if let Some(description) = &self.description {
            schema.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
        }
        if let Some(default) = &self.default {
            schema.insert("default".to_string(), default.clone());
        }
        if let Some(examples) = &self.examples {
            schema.insert("examples".to_string(), Value::Array(examples.clone()));
        }
        if let Some(deprecated) = self.deprecated {
            schema.insert("deprecated".to_string(), Value::Bool(deprecated));
        }
        schema
    }
}

/// The length bounds of a string, an array or bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct LengthBounds {
    /// The least accepted length.
    pub min_length: Option<u64>,
    /// The greatest accepted length.
    pub max_length: Option<u64>,
}

impl LengthBounds {
    /// The length bounds of a schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The least accepted length.
    pub fn min_length(mut self, length: u64) -> Self {
        self.min_length = Some(length);
        self
    }

    /// The greatest accepted length.
    pub fn max_length(mut self, length: u64) -> Self {
        self.max_length = Some(length);
        self
    }

    fn apply(&self, mut schema: Map<String, Value>) -> Map<String, Value> {
        if let Some(min) = self.min_length {
            schema.insert("minLength".to_string(), json!(min));
        }
        if let Some(max) = self.max_length {
            schema.insert("maxLength".to_string(), json!(max));
        }
        schema
    }
}

/// The bounds of a number.
#[derive(Clone, Copy, Debug, Default)]
pub struct NumberBounds {
    /// The least accepted value.
    pub minimum: Option<f64>,
    /// The greatest accepted value.
    pub maximum: Option<f64>,
    /// The exclusive least accepted value.
    pub exclusive_minimum: Option<f64>,
    /// The exclusive greatest accepted value.
    pub exclusive_maximum: Option<f64>,
    /// The divisor of the accepted values.
    pub multiple_of: Option<f64>,
}

impl NumberBounds {
    /// The bounds of a schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The least accepted value.
    pub fn minimum(mut self, minimum: f64) -> Self {
        self.minimum = Some(minimum);
        self
    }

    /// The greatest accepted value.
    pub fn maximum(mut self, maximum: f64) -> Self {
        self.maximum = Some(maximum);
        self
    }

    /// The exclusive least accepted value.
    pub fn exclusive_minimum(mut self, minimum: f64) -> Self {
        self.exclusive_minimum = Some(minimum);
        self
    }

    /// The exclusive greatest accepted value.
    pub fn exclusive_maximum(mut self, maximum: f64) -> Self {
        self.exclusive_maximum = Some(maximum);
        self
    }

    /// The divisor of the accepted values.
    pub fn multiple_of(mut self, multiple: f64) -> Self {
        self.multiple_of = Some(multiple);
        self
    }

    fn apply(&self, mut schema: Map<String, Value>) -> Map<String, Value> {
        if let Some(minimum) = self.minimum {
            schema.insert("minimum".to_string(), number(minimum));
        }
        if let Some(maximum) = self.maximum {
            schema.insert("maximum".to_string(), number(maximum));
        }
        if let Some(minimum) = self.exclusive_minimum {
            schema.insert("exclusiveMinimum".to_string(), number(minimum));
        }
        if let Some(maximum) = self.exclusive_maximum {
            schema.insert("exclusiveMaximum".to_string(), number(maximum));
        }
        if let Some(multiple) = self.multiple_of {
            schema.insert("multipleOf".to_string(), number(multiple));
        }
        schema
    }
}

/// The bounds of a bigint: TypeBox's `Type.BigInt` options. A bigint bound is a decimal string, as JSON has no
/// bigint.
#[derive(Clone, Debug, Default)]
pub struct BigintBounds {
    /// The least accepted value.
    pub minimum: Option<String>,
    /// The greatest accepted value.
    pub maximum: Option<String>,
    /// The exclusive least accepted value.
    pub exclusive_minimum: Option<String>,
    /// The exclusive greatest accepted value.
    pub exclusive_maximum: Option<String>,
    /// The divisor of the accepted values.
    pub multiple_of: Option<String>,
}

impl BigintBounds {
    /// The bounds of a schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The least accepted value.
    pub fn minimum(mut self, minimum: impl Into<String>) -> Self {
        self.minimum = Some(minimum.into());
        self
    }

    /// The greatest accepted value.
    pub fn maximum(mut self, maximum: impl Into<String>) -> Self {
        self.maximum = Some(maximum.into());
        self
    }

    /// The exclusive least accepted value.
    pub fn exclusive_minimum(mut self, minimum: impl Into<String>) -> Self {
        self.exclusive_minimum = Some(minimum.into());
        self
    }

    /// The exclusive greatest accepted value.
    pub fn exclusive_maximum(mut self, maximum: impl Into<String>) -> Self {
        self.exclusive_maximum = Some(maximum.into());
        self
    }

    /// The divisor of the accepted values.
    pub fn multiple_of(mut self, multiple: impl Into<String>) -> Self {
        self.multiple_of = Some(multiple.into());
        self
    }

    fn apply(&self, mut schema: Map<String, Value>) -> Map<String, Value> {
        if let Some(minimum) = &self.minimum {
            schema.insert("minimum".to_string(), Value::String(minimum.clone()));
        }
        if let Some(maximum) = &self.maximum {
            schema.insert("maximum".to_string(), Value::String(maximum.clone()));
        }
        if let Some(minimum) = &self.exclusive_minimum {
            schema.insert(
                "exclusiveMinimum".to_string(),
                Value::String(minimum.clone()),
            );
        }
        if let Some(maximum) = &self.exclusive_maximum {
            schema.insert(
                "exclusiveMaximum".to_string(),
                Value::String(maximum.clone()),
            );
        }
        if let Some(multiple) = &self.multiple_of {
            schema.insert("multipleOf".to_string(), Value::String(multiple.clone()));
        }
        schema
    }
}

/// The options of a string schema.
#[derive(Clone, Debug, Default)]
pub struct StrOptions {
    /// The annotations of the schema.
    pub annotations: Annotations,
    /// The length bounds of the schema.
    pub bounds: LengthBounds,
    /// The pattern the value must match.
    pub pattern: Option<String>,
    /// The format the value must have.
    pub format: Option<String>,
}

impl StrOptions {
    /// The options of a string schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The annotations of the schema.
    pub fn annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = annotations;
        self
    }

    /// The length bounds of the schema.
    pub fn bounds(mut self, bounds: LengthBounds) -> Self {
        self.bounds = bounds;
        self
    }

    /// The pattern the value must match.
    pub fn pattern(mut self, pattern: impl Into<String>) -> Self {
        self.pattern = Some(pattern.into());
        self
    }

    /// The format the value must have.
    pub fn format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }
}

/// The options of a number or integer schema.
#[derive(Clone, Debug, Default)]
pub struct NumOptions {
    /// The annotations of the schema.
    pub annotations: Annotations,
    /// The bounds of the schema.
    pub bounds: NumberBounds,
}

impl NumOptions {
    /// The options of a number schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The annotations of the schema.
    pub fn annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = annotations;
        self
    }

    /// The bounds of the schema.
    pub fn bounds(mut self, bounds: NumberBounds) -> Self {
        self.bounds = bounds;
        self
    }
}

/// The options of a bigint schema.
#[derive(Clone, Debug, Default)]
pub struct BigintOptions {
    /// The annotations of the schema.
    pub annotations: Annotations,
    /// The bounds of the schema.
    pub bounds: BigintBounds,
}

impl BigintOptions {
    /// The options of a bigint schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The annotations of the schema.
    pub fn annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = annotations;
        self
    }

    /// The bounds of the schema.
    pub fn bounds(mut self, bounds: BigintBounds) -> Self {
        self.bounds = bounds;
        self
    }
}

/// The options of an array schema.
#[derive(Clone, Debug, Default)]
pub struct ArrOptions {
    /// The annotations of the schema.
    pub annotations: Annotations,
    /// The length bounds of the schema.
    pub bounds: LengthBounds,
    /// Whether the items must be unique.
    pub unique_items: bool,
}

impl ArrOptions {
    /// The options of an array schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The annotations of the schema.
    pub fn annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = annotations;
        self
    }

    /// The length bounds of the schema.
    pub fn bounds(mut self, bounds: LengthBounds) -> Self {
        self.bounds = bounds;
        self
    }

    /// Whether the items must be unique.
    pub fn unique_items(mut self, unique: bool) -> Self {
        self.unique_items = unique;
        self
    }
}

/// A string.
pub fn str() -> JsonSchema {
    str_with(&StrOptions::default())
}

/// A string, with the given options.
pub fn str_with(options: &StrOptions) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("string".to_string()));
    schema = options.bounds.apply(schema);
    if let Some(pattern) = &options.pattern {
        schema.insert("pattern".to_string(), Value::String(pattern.clone()));
    }
    if let Some(format) = &options.format {
        schema.insert("format".to_string(), Value::String(format.clone()));
    }
    JsonSchema::Object(options.annotations.apply(schema))
}

/// A number within optional bounds.
pub fn num(options: &NumOptions) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("number".to_string()));
    schema = options.bounds.apply(schema);
    JsonSchema::Object(options.annotations.apply(schema))
}

/// An integer within optional bounds.
pub fn int(options: &NumOptions) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("integer".to_string()));
    schema = options.bounds.apply(schema);
    JsonSchema::Object(options.annotations.apply(schema))
}

/// A bigint within optional bounds.
pub fn bigint(options: &BigintOptions) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("bigint".to_string()));
    schema = options.bounds.apply(schema);
    JsonSchema::Object(options.annotations.apply(schema))
}

/// A boolean.
pub fn bool(options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("boolean".to_string()));
    JsonSchema::Object(options.apply(schema))
}

/// `null`.
pub fn nul(options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("null".to_string()));
    JsonSchema::Object(options.apply(schema))
}

/// A byte string with optional length bounds.
pub fn bytes(options: &Annotations, bounds: &LengthBounds) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("bytes".to_string()));
    schema = bounds.apply(schema);
    JsonSchema::Object(options.apply(schema))
}

/// Any value.
pub fn unknown(options: &Annotations) -> JsonSchema {
    JsonSchema::Object(options.apply(Map::new()))
}

/// Exactly `value`.
pub fn lit(value: Value, options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("const".to_string(), value);
    JsonSchema::Object(options.apply(schema))
}

/// One of the given values.
pub fn one_of(values: &[Value], options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("enum".to_string(), Value::Array(values.to_vec()));
    JsonSchema::Object(options.apply(schema))
}

/// Any of the given schemas.
pub fn union(schemas: &[JsonSchema], options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("anyOf".to_string(), Value::Array(schemas.to_vec()));
    JsonSchema::Object(options.apply(schema))
}

/// `schema` or `null`.
pub fn nullable(schema: &JsonSchema, options: &Annotations) -> JsonSchema {
    let mut map = Map::new();
    map.insert(
        "anyOf".to_string(),
        Value::Array(vec![schema.clone(), json!({ "type": "null" })]),
    );
    JsonSchema::Object(options.apply(map))
}

/// An array of `items`.
pub fn arr(items: &JsonSchema, options: &ArrOptions) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("array".to_string()));
    schema.insert("items".to_string(), items.clone());
    schema = options.bounds.apply(schema);
    if options.unique_items {
        schema.insert("uniqueItems".to_string(), Value::Bool(true));
    }
    JsonSchema::Object(options.annotations.apply(schema))
}

/// A fixed-length array whose items match `items` in order.
pub fn tuple(items: &[JsonSchema], options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("array".to_string()));
    schema.insert("prefixItems".to_string(), Value::Array(items.to_vec()));
    schema.insert("items".to_string(), Value::Bool(false));
    schema.insert("minItems".to_string(), json!(items.len()));
    JsonSchema::Object(options.apply(schema))
}

/// An object with string keys and values of `value`.
pub fn map(value: &JsonSchema, options: &Annotations) -> JsonSchema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("additionalProperties".to_string(), value.clone());
    JsonSchema::Object(options.apply(schema))
}

/// `schema` as it is: nothing checks that its values are of the type the caller expects.
pub fn unsafe_schema(schema: &JsonSchema) -> JsonSchema {
    schema.clone()
}

/// A schema that contains itself: `build` receives a reference to the schema it returns.
pub fn recursive(id: &str, build: impl FnOnce(&JsonSchema) -> JsonSchema) -> JsonSchema {
    let reference = json!({ "$ref": id });
    let mut schema = build(&reference);
    if let JsonSchema::Object(map) = &mut schema {
        map.insert("$id".to_string(), Value::String(id.to_string()));
    }
    schema
}

/// A property of an object schema that may be omitted.
#[derive(Clone, Debug)]
pub struct Optional {
    /// The schema of the property.
    pub schema: JsonSchema,
}

/// Marks an object property as optional.
pub fn opt(schema: &JsonSchema) -> Optional {
    Optional {
        schema: schema.clone(),
    }
}

/// The properties of an object schema: a schema or an optional schema per name.
#[derive(Clone, Debug, Default)]
pub struct Properties {
    entries: Vec<(String, Property)>,
}

/// One property of an object schema.
#[derive(Clone, Debug)]
pub enum Property {
    /// A required property.
    Required(JsonSchema),
    /// An optional property.
    Optional(Optional),
}

impl Properties {
    /// The properties of an object schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a required property.
    pub fn with(mut self, name: &str, schema: &JsonSchema) -> Self {
        self.entries
            .push((name.to_string(), Property::Required(schema.clone())));
        self
    }

    /// Adds an optional property.
    pub fn with_optional(mut self, name: &str, schema: &Optional) -> Self {
        self.entries
            .push((name.to_string(), Property::Optional(schema.clone())));
        self
    }
}

/// A closed object: the given properties (required unless optional) and no others.
pub fn obj(properties: &Properties, options: &Annotations, open: bool) -> JsonSchema {
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for (name, property) in &properties.entries {
        match property {
            Property::Required(schema) => {
                props.insert(name.clone(), schema.clone());
                required.push(Value::String(name.clone()));
            }
            Property::Optional(optional) => {
                props.insert(name.clone(), optional.schema.clone());
            }
        }
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(props));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    if !open {
        schema.insert("additionalProperties".to_string(), Value::Bool(false));
    }
    JsonSchema::Object(options.apply(schema))
}
