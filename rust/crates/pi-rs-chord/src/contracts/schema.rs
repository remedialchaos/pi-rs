//! Chord's short schema constructors. Schemas remain portable JSON data.
use crate::{
    Error, Result,
    json::{JsonString, JsonValue},
};
use indexmap::IndexMap;
fn node(entries: impl IntoIterator<Item = (&'static str, JsonValue)>) -> JsonValue {
    JsonValue::Object(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn typed(kind: &str) -> JsonValue {
    node([("type", JsonValue::String(kind.into()))])
}
pub fn str() -> JsonValue {
    typed("string")
}
pub fn num() -> JsonValue {
    typed("number")
}
pub fn int() -> JsonValue {
    typed("integer")
}
pub fn bool() -> JsonValue {
    typed("boolean")
}
pub fn nul() -> JsonValue {
    typed("null")
}
pub fn bytes() -> JsonValue {
    typed("bytes")
}
pub fn unknown() -> JsonValue {
    JsonValue::Object(IndexMap::new())
}
#[derive(Clone, Copy, Default)]
pub struct Bounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}
pub fn bounded(mut schema: JsonValue, bounds: Bounds) -> Result<JsonValue> {
    let (min, max) = match schema.get("type") {
        Some(JsonValue::String(kind)) => match kind.to_utf8()?.as_str() {
            "string" | "bytes" => ("minLength", "maxLength"),
            "array" => ("minItems", "maxItems"),
            "number" | "integer" => ("minimum", "maximum"),
            _ => return Err(Error::Invalid("schema has no bounds".into())),
        },
        _ => return Err(Error::Invalid("schema has no type".into())),
    };
    if let JsonValue::Object(object) = &mut schema {
        for (key, value) in [(min, bounds.min), (max, bounds.max)] {
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(Error::Invalid("bound must be finite".into()));
                }
                object.insert(key.into(), JsonValue::Number(value));
            }
        }
    }
    Ok(schema)
}
pub fn defaulted(mut schema: JsonValue, value: JsonValue) -> Result<JsonValue> {
    crate::json::copy_json(&value)?;
    let JsonValue::Object(object) = &mut schema else {
        return Err(Error::Invalid("expected a schema object".into()));
    };
    object.insert("default".into(), value);
    Ok(schema)
}
pub fn pattern(mut schema: JsonValue, pattern: &str) -> JsonValue {
    if let JsonValue::Object(object) = &mut schema {
        object.insert("pattern".into(), JsonValue::String(pattern.into()));
    }
    schema
}
pub fn lit(value: JsonValue) -> Result<JsonValue> {
    let kind = match &value {
        JsonValue::String(_) => "string",
        JsonValue::Number(n) if n.is_finite() => "number",
        JsonValue::Bool(_) => "boolean",
        _ => {
            return Err(Error::Invalid(
                "expected a string, number or boolean literal".into(),
            ));
        }
    };
    Ok(node([
        ("type", JsonValue::String(kind.into())),
        ("const", value),
    ]))
}
pub fn one_of(values: impl IntoIterator<Item = JsonValue>) -> Result<JsonValue> {
    Ok(union(
        values.into_iter().map(lit).collect::<Result<Vec<_>>>()?,
    ))
}
pub fn union(schemas: impl IntoIterator<Item = JsonValue>) -> JsonValue {
    node([("anyOf", JsonValue::Array(schemas.into_iter().collect()))])
}
pub struct Property {
    pub schema: JsonValue,
    pub optional: bool,
}
pub fn required(schema: JsonValue) -> Property {
    Property {
        schema,
        optional: false,
    }
}
pub fn opt(schema: JsonValue) -> Property {
    Property {
        schema,
        optional: true,
    }
}
pub fn obj<K: Into<JsonString>>(properties: impl IntoIterator<Item = (K, Property)>) -> JsonValue {
    let mut required = Vec::new();
    let mut fields = IndexMap::new();
    for (key, property) in properties {
        let key = key.into();
        if !property.optional {
            required.push(JsonValue::String(key.clone()));
        }
        fields.insert(key, property.schema);
    }
    node([
        ("type", JsonValue::String("object".into())),
        ("required", JsonValue::Array(required)),
        ("properties", JsonValue::Object(fields)),
        ("additionalProperties", JsonValue::Bool(false)),
    ])
}
pub fn arr(items: JsonValue) -> JsonValue {
    node([
        ("type", JsonValue::String("array".into())),
        ("items", items),
    ])
}
pub fn map(value: JsonValue) -> JsonValue {
    node([
        ("type", JsonValue::String("object".into())),
        ("patternProperties", node([("^.*$", value)])),
    ])
}
pub fn method(args: Option<JsonValue>, result: Option<JsonValue>) -> super::Member {
    super::Member::Method {
        args: args.unwrap_or_else(nul),
        result: result.unwrap_or_else(nul),
    }
}
pub fn keyed(schema: JsonValue, key: Option<super::HostKind>) -> super::Member {
    super::Member::Keyed { schema, key }
}
pub fn event(schema: JsonValue) -> super::Member {
    super::Member::Event(schema)
}
pub fn queries(item: JsonValue, args: IndexMap<String, JsonValue>) -> super::Member {
    super::Member::Queries { item, args }
}
