//! Stored-span rendering using the independently recorded and gated implementation.
use crate::{AttributeValue, SpanAttributes, StoredSpan};
use serde_json::{Value, json};

// A verbatim copy of the independently gated renderer, with no N-API dependency.
mod verified;

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or_else(|| json!({"$num": crate::js::number_to_string(value)}))
}

fn attributes(attributes: &SpanAttributes) -> Result<Value, serde_json::Error> {
    let mut entries: Vec<_> = attributes.iter().collect();
    // Object.entries enumerates canonical array-index keys first, then insertion order.
    entries.sort_by_key(|(key, _)| {
        key.parse::<u32>()
            .ok()
            .filter(|index| *index != u32::MAX && index.to_string() == **key)
            .unwrap_or(u32::MAX)
    });
    entries
        .into_iter()
        .map(|(key, value)| {
            let value = if matches!(value, AttributeValue::Undefined) {
                json!({"$undefined": true})
            } else {
                serde_json::to_value(value)?
            };
            Ok((key.clone(), value))
        })
        .collect::<Result<serde_json::Map<_, _>, _>>()
        .map(Value::Object)
}

fn wire_span(span: &StoredSpan) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(span)?;
    value["startedMs"] = number(span.started_ms);
    value["endedMs"] = number(span.ended_ms);
    value["attributes"] = attributes(&span.attributes)?;
    for (event, wire) in span
        .events
        .iter()
        .zip(value["events"].as_array_mut().into_iter().flatten())
    {
        wire["timeMs"] = number(event.time_ms);
        wire["attributes"] = attributes(&event.attributes)?;
    }
    for (link, wire) in span
        .links
        .iter()
        .zip(value["links"].as_array_mut().into_iter().flatten())
    {
        wire["attributes"] = attributes(&link.attributes)?;
    }
    Ok(value)
}

/// Render a stored trace in chronological forest order, escaping terminal control characters.
pub fn render_trace(spans: &[StoredSpan]) -> Result<String, crate::ErrorDetails> {
    let values = spans
        .iter()
        .map(wire_span)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| crate::ErrorDetails {
            name: "SerializationError".into(),
            message: error.to_string(),
        })?;
    verified::render_trace(values).map_err(|message| crate::ErrorDetails {
        name: "RenderError".into(),
        message,
    })
}
