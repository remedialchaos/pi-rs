use super::*;
use crate::{
    contracts::Snapshot,
    link::{LinkValue, parse_payload, payload_part},
};
use pi_rs_schema::Value;

fn borrowed(value: &Value) -> Result<LinkValue<'_>> {
    Ok(match value {
        Value::Bytes(bytes) => LinkValue::Bytes(bytes),
        Value::Array(items) => LinkValue::Array(items.iter().map(borrowed).collect::<Result<_>>()?),
        Value::Object(map) => LinkValue::Object(
            map.iter()
                .map(|(k, v)| Ok((k.as_str().into(), borrowed(v)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Null => LinkValue::Json(JsonValue::Null),
        Value::Bool(b) => LinkValue::Json(JsonValue::Bool(*b)),
        Value::String(s) => LinkValue::Json(JsonValue::String(s.as_str().into())),
        Value::Number(n) => LinkValue::Json(JsonValue::Number(*n)),
        _ => return Err(Error::Invalid("unsupported link value".into())),
    })
}

pub(super) fn encode(
    value: JsonValue,
    schema: &Snapshot,
    compression: bool,
) -> Result<Option<Payload>> {
    if value == JsonValue::Null {
        return Ok(None);
    }
    let (text, segments) = if schema.bytes {
        let value =
            crate::data::schema_value_with_bytes(&value.to_serde()?, &schema.schema.to_serde()?);
        let (text, segments) = payload_part(&borrowed(&value)?, true)?;
        (
            text,
            segments.map(|segments| segments.into_iter().map(<[u8]>::to_vec).collect()),
        )
    } else {
        (value.to_json()?, None)
    };
    Ok(Some(Payload {
        part: Part::origin(text, compression)?,
        segments,
    }))
}

fn wire_value(value: &LinkValue<'_>) -> Result<Value> {
    Ok(match value {
        LinkValue::Json(json) => Value::from(json.to_serde()?),
        LinkValue::Bytes(bytes) => Value::Bytes(bytes.to_vec()),
        LinkValue::Array(items) => {
            Value::Array(items.iter().map(wire_value).collect::<Result<_>>()?)
        }
        LinkValue::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.to_utf8()?, wire_value(v)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

fn native(value: LinkValue<'_>) -> JsonValue {
    match value {
        LinkValue::Json(json) => json,
        LinkValue::Bytes(bytes) => JsonValue::Array(
            bytes
                .iter()
                .map(|byte| JsonValue::Number(f64::from(*byte)))
                .collect(),
        ),
        LinkValue::Array(items) => JsonValue::Array(items.into_iter().map(native).collect()),
        LinkValue::Object(map) => {
            JsonValue::Object(map.into_iter().map(|(k, v)| (k, native(v))).collect())
        }
    }
}

pub(super) fn decode(
    payload: Option<Payload>,
    schema: &Snapshot,
    trusted: bool,
) -> Result<JsonValue> {
    let Some(payload) = payload else {
        return Ok(JsonValue::Null);
    };
    let json = payload.part.parse(Limits::for_link(trusted))?;
    if schema.bytes != payload.segments.is_some() {
        return Err(ChordError::invalid(
            "payload",
            "byte segments differ from method schema",
            None,
        )
        .into());
    }
    let segments = payload
        .segments
        .as_ref()
        .map(|segments| segments.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let value = parse_payload(&json, segments.as_deref())?;
    if schema.bytes {
        let validator = pi_rs_schema::compile(&schema.schema.to_serde()?)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if !validator.check(&wire_value(&value)?) {
            return Err(
                ChordError::invalid("payload", "value differs from method schema", None).into(),
            );
        }
    }
    Ok(native(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_methods_send_nested_octets_as_binary_segments() -> Result<()> {
        let schema = crate::contracts::schema::obj([(
            "data",
            crate::contracts::schema::required(crate::contracts::schema::bytes()),
        )]);
        let schema = Snapshot {
            schema: schema.clone(),
            normal: schema,
            bytes: true,
        };
        let input = JsonValue::parse(r#"{"data":[0,255]}"#)?;
        let payload = encode(input.clone(), &schema, false)?.ok_or(Error::Closed)?;
        assert_eq!(payload.segments, Some(vec![vec![0, 255]]));
        assert_eq!(payload.part, Part::Text(r#"{"data":{"$bytes":0}}"#.into()));
        assert_eq!(decode(Some(payload), &schema, false)?, input);
        let inline = Payload {
            part: Part::Text(input.to_json()?),
            segments: Some(Vec::new()),
        };
        assert!(decode(Some(inline), &schema, false).is_err());
        Ok(())
    }
}
