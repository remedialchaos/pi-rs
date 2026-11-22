//! Iterative port of the upstream stored-trace renderer.
#![forbid(unsafe_code)]
use indexmap::IndexMap;
use serde_json::Value;

fn escaped(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => format!("\\u{:04x}", c as u32),
            _ => c.to_string(),
        })
        .collect()
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("TypeError: Cannot read properties of undefined (reading '{key}')"))
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| match value["$num"].as_str() {
            Some("Infinity") => f64::INFINITY,
            Some("-Infinity") => f64::NEG_INFINITY,
            Some("-0") => -0.0,
            _ => f64::NAN,
        })
}

fn milliseconds(value: f64) -> String {
    let scaled = value * 1000.0;
    let rounded = if scaled.fract() == -0.5 {
        scaled.ceil()
    } else {
        scaled.round()
    };
    crate::js::number_to_string(rounded / 1000.0)
}

fn json(value: &Value) -> Result<String, String> {
    if value.get("$undefined") == Some(&Value::Bool(true)) {
        return Ok("undefined".into());
    }
    if value.is_number() || value.get("$num").is_some() {
        let n = number(value);
        return Ok(if n.is_finite() {
            crate::js::number_to_string(n)
        } else {
            "null".into()
        });
    }
    match value {
        Value::Array(items) => {
            let items = items
                .iter()
                .map(|item| json(item).map(|s| if s == "undefined" { "null".into() } else { s }))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", items.join(",")))
        }
        _ => serde_json::to_string(value).map_err(|error| error.to_string()),
    }
}

fn attributes(value: Option<&Value>) -> Result<String, String> {
    let Some(value) = value.filter(|v| v.get("$undefined") != Some(&Value::Bool(true))) else {
        return Ok(String::new());
    };
    let map = value
        .as_object()
        .ok_or("TypeError: Cannot convert undefined or null to object")?;
    let mut out = String::new();
    for (key, value) in map {
        out.push_str(&format!(" {}={}", escaped(key), json(value)?));
    }
    Ok(out)
}

fn list<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match value.get(key) {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => Err(format!("TypeError: {key} is not iterable")),
    }
}

/// Render a chronological forest. Duplicate IDs keep their last value and original key order.
pub fn render_trace(spans: Vec<Value>) -> Result<String, String> {
    let mut by_id = IndexMap::new();
    for span in &spans {
        by_id.insert(text(span, "id")?, span);
    }
    let mut ordered: Vec<_> = by_id.values().copied().collect();
    crate::js::sort(&mut ordered, |a, b| {
        number(&a["startedMs"]) - number(&b["startedMs"])
    });
    let mut roots = Vec::new();
    let mut children: IndexMap<&str, Vec<&Value>> = IndexMap::new();
    for span in ordered {
        match span.get("parentId").and_then(Value::as_str) {
            Some(parent) if by_id.contains_key(parent) => {
                children.entry(parent).or_default().push(span)
            }
            _ => roots.push(span),
        }
    }
    let mut pending: Vec<_> = roots.into_iter().rev().map(|span| (span, 0)).collect();
    let mut lines = Vec::new();
    while let Some((span, depth)) = pending.pop() {
        let indent = "  ".repeat(depth);
        let mut line = format!(
            "{indent}{}{} ({}ms)",
            escaped(text(span, "name")?),
            attributes(span.get("attributes"))?,
            milliseconds(number(&span["endedMs"]) - number(&span["startedMs"]))
        );
        if span["status"]["status"] == "error" {
            line.push_str(" error");
            if let Some(error) = span["status"].get("error") {
                line.push_str(&format!(
                    " {}: {}",
                    escaped(text(error, "name")?),
                    escaped(text(error, "message")?)
                ));
            }
        }
        lines.push(line);
        for event in list(span, "events")? {
            lines.push(format!(
                "{indent}  · {}{} (+{}ms)",
                escaped(text(event, "name")?),
                attributes(event.get("attributes"))?,
                milliseconds(number(&event["timeMs"]))
            ));
        }
        for link in list(span, "links")? {
            lines.push(format!(
                "{indent}  → link trace {} span {}{}",
                escaped(text(link, "traceId")?),
                escaped(text(link, "spanId")?),
                attributes(link.get("attributes"))?
            ));
        }
        if let Some(descendants) = children.get(text(span, "id")?) {
            pending.extend(descendants.iter().rev().map(|span| (*span, depth + 1)));
        }
    }
    Ok(lines.join("\n"))
}
