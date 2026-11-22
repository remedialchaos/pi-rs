//! Schema normalization, fingerprints, parameter declarations and host identities.
use crate::{
    Error, Result,
    data::{Validator, join_key},
    json::{JsonString, JsonValue, number_to_string},
};
use indexmap::IndexMap;

mod runtime;
pub mod schema;
pub use runtime::*;

pub const NORMAL_FORM_VERSION: u32 = 2;
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub schema: JsonValue,
    pub normal: JsonValue,
    pub bytes: bool,
}
fn refused(path: &str, problem: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{path}: {problem}"))
}
fn object(entries: impl IntoIterator<Item = (JsonString, JsonValue)>) -> JsonValue {
    JsonValue::Object(entries.into_iter().collect())
}
fn sorted_data(value: &JsonValue) -> Result<JsonValue> {
    Ok(match value {
        JsonValue::Object(o) => {
            let mut pairs: Vec<_> = o.iter().collect();
            pairs.sort_by_key(|(k, _)| *k);
            object(
                pairs
                    .into_iter()
                    .map(|(k, v)| Ok((k.clone(), sorted_data(v)?)))
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        JsonValue::Array(a) => {
            JsonValue::Array(a.iter().map(sorted_data).collect::<Result<Vec<_>>>()?)
        }
        other => crate::json::copy_json(other)?,
    })
}
pub fn snapshot_schema(source: &JsonValue, path: &str) -> Result<Snapshot> {
    walk(source, path, false)
}
pub fn snapshot_arguments(source: &JsonValue, path: &str) -> Result<Snapshot> {
    walk(source, path, true)
}
fn walk(source: &JsonValue, path: &str, closed: bool) -> Result<Snapshot> {
    if matches!(source, JsonValue::Bool(_)) {
        return Ok(Snapshot {
            schema: source.clone(),
            normal: source.clone(),
            bytes: false,
        });
    }
    let JsonValue::Object(input) = source else {
        return Err(refused(path, "expected a schema"));
    };
    let is_bytes = source.get("type") == Some(&JsonValue::String("bytes".into()));
    let mut entries: Vec<_> = input.iter().collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut schema = IndexMap::new();
    let mut normal = IndexMap::new();
    let mut bytes = is_bytes;
    for (key, value) in entries {
        let key_text = key.to_utf8()?;
        let name = key_text.as_str();
        let at = format!("{path}.{key_text}");
        let annotation = matches!(
            name,
            "title" | "description" | "examples" | "$comment" | "deprecated"
        );
        if is_bytes && !annotation && !matches!(name, "type" | "minLength" | "maxLength") {
            return Err(refused(
                path,
                format!("unsupported keyword {}", key.to_json()),
            ));
        }
        let copy = match name {
            "additionalProperties" | "additionalItems" => walk(value, &at, closed)?,
            "anyOf" | "allOf" | "prefixItems" => walk_list(value, &at, closed)?,
            "items" => {
                if matches!(value, JsonValue::Array(_)) {
                    walk_list(value, &at, closed)?
                } else {
                    walk(value, &at, closed)?
                }
            }
            "properties" | "patternProperties" | "$defs" => {
                let JsonValue::Object(map) = value else {
                    return Err(refused(&at, "expected an object of schemas"));
                };
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by_key(|(k, _)| *k);
                let mut s = IndexMap::new();
                let mut n = IndexMap::new();
                let mut b = false;
                for (k, v) in entries {
                    let item = walk(v, &format!("{at}.{}", k.to_utf8()?), closed)?;
                    b |= item.bytes;
                    s.insert(k.clone(), item.schema);
                    n.insert(k.clone(), item.normal);
                }
                Snapshot {
                    schema: JsonValue::Object(s),
                    normal: JsonValue::Object(n),
                    bytes: b,
                }
            }
            "type" => {
                let valid = if let JsonValue::String(s) = value {
                    s.to_utf8().ok().is_some_and(|s| {
                        matches!(
                            s.as_str(),
                            "null"
                                | "boolean"
                                | "number"
                                | "integer"
                                | "string"
                                | "array"
                                | "object"
                                | "bytes"
                        )
                    })
                } else {
                    false
                };
                if !valid {
                    return Err(refused(
                        &at,
                        format!("unsupported type {}", value.to_json()?),
                    ));
                }
                Snapshot {
                    schema: value.clone(),
                    normal: value.clone(),
                    bytes: false,
                }
            }
            "required" => {
                let JsonValue::Array(a) = value else {
                    return Err(refused(&at, "expected an array of strings"));
                };
                let mut strings = Vec::new();
                for v in a {
                    let JsonValue::String(s) = v else {
                        return Err(refused(&at, "expected an array of strings"));
                    };
                    strings.push(s.clone());
                }
                strings.sort();
                let sorted = JsonValue::Array(strings.into_iter().map(JsonValue::String).collect());
                Snapshot {
                    schema: sorted.clone(),
                    normal: sorted,
                    bytes: false,
                }
            }
            "enum" => {
                let JsonValue::Array(a) = sorted_data(value)? else {
                    return Err(refused(&at, "expected an array"));
                };
                let mut keyed = a
                    .into_iter()
                    .map(|v| Ok((JsonString::from(v.to_json()?), v)))
                    .collect::<Result<Vec<_>>>()?;
                keyed.sort_by(|a, b| a.0.cmp(&b.0));
                let v = JsonValue::Array(keyed.into_iter().map(|(_, v)| v).collect());
                Snapshot {
                    schema: v.clone(),
                    normal: v,
                    bytes: false,
                }
            }
            "const" | "default" | "$ref" | "$id" | "pattern" | "format" | "minLength"
            | "maxLength" | "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
            | "multipleOf" | "minItems" | "maxItems" | "uniqueItems" | "minProperties"
            | "maxProperties" | "readOnly" | "writeOnly" | "title" | "description" | "examples"
            | "$comment" | "deprecated" => {
                let v = sorted_data(value).map_err(|_| {
                    refused(
                        &at,
                        "expected JSON data (finite numbers, plain objects and arrays)",
                    )
                })?;
                Snapshot {
                    schema: v.clone(),
                    normal: v,
                    bytes: false,
                }
            }
            _ => {
                return Err(refused(
                    path,
                    format!("unsupported keyword {}", key.to_json()),
                ));
            }
        };
        bytes |= copy.bytes;
        schema.insert(key.clone(), copy.schema);
        if !annotation {
            normal.insert(key.clone(), copy.normal);
        }
    }
    let node = JsonValue::Object(schema);
    let is_record = node.get("patternProperties").is_some() && node.get("properties").is_none();
    let is_object = node.get("type") == Some(&JsonValue::String("object".into()))
        || node.get("properties").is_some();
    if closed
        && is_object
        && !is_record
        && node.get("additionalProperties") != Some(&JsonValue::Bool(false))
    {
        return Err(refused(
            path,
            "argument objects must be closed (additionalProperties: false)",
        ));
    }
    if is_bytes {
        for key in ["minLength", "maxLength"] {
            if let Some(value) = node.get(key)
                && !matches!(value,JsonValue::Number(n) if n.is_finite() && *n >= 0.0 && n.fract()==0.0 && *n <= 9_007_199_254_740_991.0)
            {
                return Err(refused(path, "bytes bounds must be non-negative integers"));
            }
        }
    }
    Ok(Snapshot {
        schema: node,
        normal: JsonValue::Object(normal),
        bytes,
    })
}
fn walk_list(source: &JsonValue, path: &str, closed: bool) -> Result<Snapshot> {
    let JsonValue::Array(a) = source else {
        return Err(refused(path, "expected an array of schemas"));
    };
    let mut schema = Vec::new();
    let mut normal = Vec::new();
    let mut bytes = false;
    for (i, v) in a.iter().enumerate() {
        let copy = walk(v, &format!("{path}[{i}]"), closed)?;
        bytes |= copy.bytes;
        schema.push(copy.schema);
        normal.push(copy.normal);
    }
    Ok(Snapshot {
        schema: JsonValue::Array(schema),
        normal: JsonValue::Array(normal),
        bytes,
    })
}
pub fn hash64(text: &str) -> String {
    let mut low = 0x811c9dc5u32;
    let mut high = 0xcbf29ce4u32;
    for b in text.bytes() {
        low = (low ^ u32::from(b)).wrapping_mul(0x01000193);
        high = (high ^ u32::from(b)).wrapping_mul(0x01000197);
    }
    format!("{high:08x}{low:08x}")
}
pub fn fingerprint_of(normal: &JsonValue) -> Result<String> {
    Ok(hash64(&format!(
        "{{\"normalFormVersion\":{NORMAL_FORM_VERSION},\"value\":{}}}",
        normal.to_json()?
    )))
}
pub fn check_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@/._~-".contains(&b))
    {
        return Err(Error::Invalid(format!(
            "{what} {}: expected lowercase [a-z0-9@/._~-]",
            JsonString::from(name).to_json()
        )));
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq)]
pub struct ParamsData {
    pub fields: Vec<String>,
    pub schema: JsonValue,
    pub normal: JsonValue,
}
pub fn read_params(declared: &[(String, JsonValue)], path: &str) -> Result<ParamsData> {
    let mut properties = IndexMap::new();
    let mut required = Vec::new();
    let mut normal = Vec::new();
    for (field, value) in declared {
        let at = format!("{path}.{field}");
        let snapshot = snapshot_schema(value, &at)?;
        if !is_param(&snapshot.normal) {
            return Err(refused(
                &at,
                "a param is a required str(), int(), bool() or oneOf(...) of strings",
            ));
        }
        properties.insert(JsonString::from(field.as_str()), snapshot.schema);
        required.push(JsonValue::String(field.as_str().into()));
        normal.push(JsonValue::Array(vec![
            JsonValue::String(field.as_str().into()),
            snapshot.normal,
        ]));
    }
    Ok(ParamsData {
        fields: declared.iter().map(|(f, _)| f.clone()).collect(),
        schema: object([
            ("type".into(), JsonValue::String("object".into())),
            ("properties".into(), JsonValue::Object(properties)),
            ("required".into(), JsonValue::Array(required)),
            ("additionalProperties".into(), JsonValue::Bool(false)),
        ]),
        normal: JsonValue::Array(normal),
    })
}
fn is_param(node: &JsonValue) -> bool {
    let JsonValue::Object(o) = node else {
        return false;
    };
    if let Some(any) = node.get("anyOf") {
        return o.len() == 1
            && matches!(any,JsonValue::Array(branches) if branches.iter().all(|b|matches!(b,JsonValue::Object(m) if m.len()==2 && b.get("type")==Some(&JsonValue::String("string".into())) && matches!(b.get("const"),Some(JsonValue::String(_))))));
    }
    let allowed: &[&str] = match node.get("type") {
        Some(JsonValue::String(t)) => match t.to_utf8().as_deref() {
            Ok("string") => &["type", "minLength", "maxLength", "pattern", "format"],
            Ok("integer") => &[
                "type",
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "multipleOf",
            ],
            Ok("boolean") => &["type"],
            _ => return false,
        },
        _ => return false,
    };
    o.keys().all(|k| {
        k.to_utf8()
            .ok()
            .is_some_and(|k| allowed.contains(&k.as_str()))
    })
}
pub fn check_params(params: &ParamsData, raw: &JsonValue, reason: &str) -> Result<String> {
    Validator::compile(&params.schema.to_serde()?)?.check(&raw.to_serde()?, reason)?;
    params_key(params, raw)
}
fn js_string(value: Option<&JsonValue>) -> Result<String> {
    Ok(match value {
        None => "undefined".into(),
        Some(JsonValue::String(s)) => s.to_utf8()?,
        Some(JsonValue::Number(n)) => number_to_string(*n),
        Some(JsonValue::Bool(b)) => b.to_string(),
        Some(JsonValue::Null) => "null".into(),
        _ => "[object Object]".into(),
    })
}
fn params_key(params: &ParamsData, raw: &JsonValue) -> Result<String> {
    Ok(join_key(
        &params
            .fields
            .iter()
            .map(|f| js_string(raw.get(f)))
            .collect::<Result<Vec<_>>>()?,
    ))
}
#[derive(Clone, Debug, PartialEq)]
pub struct Address {
    pub kind: String,
    pub params: JsonValue,
    pub key: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct HostKind {
    pub kind: String,
    pub params: ParamsData,
}
impl HostKind {
    pub fn new(name: &str, params: ParamsData) -> Result<Self> {
        check_name(name, "A host kind's name")?;
        if name == "client" {
            return Err(Error::Invalid(
                "A host kind's name \"client\" is reserved for hosts without an address".into(),
            ));
        }
        Ok(Self {
            kind: name.into(),
            params,
        })
    }
    pub fn address(&self, params: JsonValue) -> Result<Address> {
        let key = join_key(
            &std::iter::once(Ok(self.kind.clone()))
                .chain(self.params.fields.iter().map(|f| js_string(params.get(f))))
                .collect::<Result<Vec<_>>>()?,
        );
        Ok(Address {
            kind: self.kind.clone(),
            params,
            key,
        })
    }
    pub fn key_of_address(&self, value: &JsonValue) -> Result<Option<String>> {
        if value.get("kind") != Some(&JsonValue::String(self.kind.as_str().into())) {
            return Ok(None);
        }
        let Some(JsonValue::Object(values)) = value.get("params") else {
            return Ok(None);
        };
        if values.len() != self.params.fields.len() {
            return Ok(None);
        }
        let JsonValue::Array(normal) = &self.params.normal else {
            return Err(Error::Invalid("invalid params normal form".into()));
        };
        for pair in normal {
            let JsonValue::Array(pair) = pair else {
                return Err(Error::Invalid("invalid params pair".into()));
            };
            let [JsonValue::String(field), schema] = pair.as_slice() else {
                return Err(Error::Invalid("invalid params field".into()));
            };
            let each = values.get(field);
            let fits = if let Some(JsonValue::Array(branches)) = schema.get("anyOf") {
                matches!(each, Some(JsonValue::String(_)))
                    && branches.iter().any(|b| b.get("const") == each)
            } else {
                match schema.get("type") {
                    Some(JsonValue::String(t)) => match t.to_utf8().as_deref() {
                        Ok("integer") => {
                            matches!(each,Some(JsonValue::Number(n)) if n.is_finite()&&n.fract()==0.0&&n.abs()<=9_007_199_254_740_991.0)
                        }
                        Ok("boolean") => matches!(each, Some(JsonValue::Bool(_))),
                        Ok("string") => matches!(each, Some(JsonValue::String(_))),
                        _ => false,
                    },
                    _ => false,
                }
            };
            if !fits {
                return Ok(None);
            }
        }
        Ok(Some(self.address(JsonValue::Object(values.clone()))?.key))
    }
    pub fn address_of_key(&self, key: &str) -> Result<Address> {
        let mut parts = vec![String::new()];
        let mut chars = key.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(c) = chars.next()
                        && let Some(p) = parts.last_mut()
                    {
                        p.push(c);
                    }
                }
                '|' => parts.push(String::new()),
                _ => {
                    if let Some(p) = parts.last_mut() {
                        p.push(c);
                    }
                }
            }
        }
        if parts.first() != Some(&self.kind) || parts.len() != self.params.fields.len() + 1 {
            return Err(Error::Invalid(format!(
                "{}: not an address key of {}",
                JsonString::from(key).to_json(),
                self.kind
            )));
        }
        let JsonValue::Array(normal) = &self.params.normal else {
            return Err(Error::Invalid("invalid params normal form".into()));
        };
        let mut params = IndexMap::new();
        for (entry, part) in normal.iter().zip(parts.into_iter().skip(1)) {
            let JsonValue::Array(pair) = entry else {
                return Err(Error::Invalid("invalid params pair".into()));
            };
            let [JsonValue::String(field), schema] = pair.as_slice() else {
                return Err(Error::Invalid("invalid params field".into()));
            };
            let value = match schema.get("type") {
                Some(JsonValue::String(t)) if t == &JsonString::from("integer") => {
                    JsonValue::Number(if part.trim().is_empty() {
                        0.0
                    } else {
                        part.parse()
                            .map_err(|_| Error::Invalid("invalid stored integer address".into()))?
                    })
                }
                Some(JsonValue::String(t)) if t == &JsonString::from("boolean") => {
                    JsonValue::Bool(part == "true")
                }
                _ => JsonValue::String(part.into()),
            };
            params.insert(field.clone(), value);
        }
        Ok(Address {
            kind: self.kind.clone(),
            params: JsonValue::Object(params),
            key: key.into(),
        })
    }
}
pub fn kind_of_key(key: &str) -> &str {
    key.split_once('|').map_or(key, |(k, _)| k)
}
pub fn identity_key(name: &str, key: &str, principal: Option<&str>) -> String {
    let mut parts = vec![name.into(), key.into()];
    if let Some(p) = principal {
        parts.push(p.into());
    }
    join_key(&parts)
}
