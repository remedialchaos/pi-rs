//! Byte leaves stay borrowed when payloads cross the JSON/segment boundary.
use crate::{
    Error, Result,
    data::ChordError,
    json::{JsonString, JsonValue},
};
use indexmap::IndexMap;

#[derive(Clone, Debug, PartialEq)]
pub enum LinkValue<'a> {
    Json(JsonValue),
    Bytes(&'a [u8]),
    Array(Vec<LinkValue<'a>>),
    Object(IndexMap<JsonString, LinkValue<'a>>),
}
pub type OriginPayload<'a> = (String, Option<Vec<&'a [u8]>>);
pub fn payload_part<'a>(value: &LinkValue<'a>, bytes: bool) -> Result<OriginPayload<'a>> {
    let mut segments = Vec::new();
    let json = encode_value(value, bytes, &mut segments)?.to_json()?;
    Ok((json, bytes.then_some(segments)))
}
fn ordered<T>(map: &IndexMap<JsonString, T>) -> Vec<(&JsonString, &T)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by_key(|(k, _)| {
        k.to_utf8()
            .ok()
            .and_then(|k| {
                k.parse::<u32>()
                    .ok()
                    .filter(|n| *n != u32::MAX && n.to_string() == k)
            })
            .map_or((1, 0), |n| (0, n))
    });
    entries
}
fn reserved() -> Error {
    ChordError::invalid(
        "args",
        "\"$bytes\" is reserved in values crossing a link",
        None,
    )
    .into()
}
fn encode_json(value: &JsonValue, bytes: bool) -> Result<JsonValue> {
    Ok(match value {
        JsonValue::Object(map) => {
            if bytes && map.contains_key(&JsonString::from("$bytes")) {
                return Err(reserved());
            }
            JsonValue::Object(
                ordered(map)
                    .into_iter()
                    .map(|(k, v)| Ok((k.clone(), encode_json(v, bytes)?)))
                    .collect::<Result<_>>()?,
            )
        }
        JsonValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .map(|v| encode_json(v, bytes))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}
fn encode_value<'a>(
    value: &LinkValue<'a>,
    bytes: bool,
    segments: &mut Vec<&'a [u8]>,
) -> Result<JsonValue> {
    Ok(match value {
        LinkValue::Json(v) => encode_json(v, bytes)?,
        LinkValue::Bytes(data) => {
            if bytes {
                let index = segments.len();
                segments.push(data);
                JsonValue::Object(
                    [("$bytes".into(), JsonValue::Number(index as f64))]
                        .into_iter()
                        .collect(),
                )
            } else {
                JsonValue::Object(
                    data.iter()
                        .enumerate()
                        .map(|(i, b)| (i.to_string().into(), JsonValue::Number(f64::from(*b))))
                        .collect(),
                )
            }
        }
        LinkValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .map(|v| encode_value(v, bytes, segments))
                .collect::<Result<_>>()?,
        ),
        LinkValue::Object(map) => {
            if bytes && map.contains_key(&JsonString::from("$bytes")) {
                return Err(reserved());
            }
            JsonValue::Object(
                ordered(map)
                    .into_iter()
                    .map(|(k, v)| Ok((k.clone(), encode_value(v, bytes, segments)?)))
                    .collect::<Result<_>>()?,
            )
        }
    })
}
/// `None` means a JSON-only schema, where `$bytes` remains ordinary data.
pub fn parse_payload<'a>(
    value: &JsonValue,
    segments: Option<&[&'a [u8]]>,
) -> Result<LinkValue<'a>> {
    let Some(segments) = segments else {
        return Ok(LinkValue::Json(value.clone()));
    };
    resolve_segments(value, segments)
}
pub fn resolve_segments<'a>(value: &JsonValue, segments: &[&'a [u8]]) -> Result<LinkValue<'a>> {
    Ok(match value {
        JsonValue::Object(map) => {
            if let Some(index) = map.get(&JsonString::from("$bytes")) {
                let JsonValue::Number(n) = index else {
                    return Err(Error::Invalid(
                        "payload: a malformed byte segment reference".into(),
                    ));
                };
                if map.len() != 1
                    || !n.is_finite()
                    || *n < 0.0
                    || n.fract() != 0.0
                    || *n >= segments.len() as f64
                {
                    return Err(Error::Invalid(
                        "payload: a malformed byte segment reference".into(),
                    ));
                }
                LinkValue::Bytes(segments.get(*n as usize).copied().ok_or_else(|| {
                    Error::Invalid("payload: a malformed byte segment reference".into())
                })?)
            } else {
                LinkValue::Object(
                    map.iter()
                        .map(|(k, v)| Ok((k.clone(), resolve_segments(v, segments)?)))
                        .collect::<Result<_>>()?,
                )
            }
        }
        JsonValue::Array(items) => LinkValue::Array(
            items
                .iter()
                .map(|v| resolve_segments(v, segments))
                .collect::<Result<_>>()?,
        ),
        primitive => LinkValue::Json(primitive.clone()),
    })
}
