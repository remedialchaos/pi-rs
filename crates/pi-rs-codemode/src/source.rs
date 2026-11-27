//! First-line options parsing with ECMAScript whitespace and source line preservation.
#![forbid(unsafe_code)]
use serde_json::{Value, json};
/// The first-line prefix that carries the options of a codemode source.
pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";
/// Lark grammar for providers with grammar-constrained tool input. It only fixes the shape of the
/// options line; the options JSON and the code are checked by [`parse_codemode_source`].
pub const CODEMODE_SOURCE_GRAMMAR: &str = r"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";
const FIELDS: &str = "`max_output_tokens` and `timeout_ms`";
/// A source that could not be parsed: empty input, invalid options, or an options line that is
/// not the first line. Like upstream's `CodemodeSourceError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CodemodeSourceError {
    /// Why the source could not be parsed.
    pub message: String,
}
impl CodemodeSourceError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    /// Parses a codemode source, like upstream's `parseCodemodeSource`.
    pub fn parse(input: String) -> Result<Value, Self> {
        parse(&input).map_err(Self::new)
    }
}
pub fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
/// Parses a codemode source, like upstream's `parseCodemodeSource`. The failure is the message of
/// the [`CodemodeSourceError`] the parse failed with.
pub fn parse_codemode_source(input: String) -> Result<Value, String> {
    parse(&input)
}
fn parse(input: &str) -> Result<Value, String> {
    if input.trim_matches(js_whitespace).is_empty() {
        return Err("Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.".into());
    }
    for (index, line) in input.split('\n').enumerate().skip(1) {
        if line
            .trim_start_matches(js_whitespace)
            .starts_with(CODEMODE_OPTIONS_PREFIX)
        {
            return Err(format!(
                "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
                index + 1
            ));
        }
    }
    let end = input.find('\n').unwrap_or(input.len());
    let first = input[..end]
        .trim_end_matches('\r')
        .trim_start_matches(js_whitespace);
    let Some(directive) = first.strip_prefix(CODEMODE_OPTIONS_PREFIX) else {
        return Ok(json!({"code": input, "options": {}}));
    };
    let code = &input[end..];
    if code.trim_matches(js_whitespace).is_empty() {
        return Err(
            "The @options line must be followed by JavaScript source on subsequent lines".into(),
        );
    }
    let directive = directive.trim_matches(js_whitespace);
    let object_error = || format!("@options must be a JSON object with supported fields {FIELDS}");
    if directive.is_empty() {
        return Err(object_error());
    }
    let parsed: Value = serde_json::from_str(directive).map_err(|error| {
        let detail = json_parse_error(directive, &error);
        format!("@options must be valid JSON with supported fields {FIELDS}: {detail}")
    })?;
    let object = parsed.as_object().ok_or_else(object_error)?;
    let mut options = serde_json::Map::new();
    for key in object.keys() {
        if key != "max_output_tokens" && key != "timeout_ms" {
            return Err(format!("@options only supports {FIELDS}; got `{key}`"));
        }
    }
    for (key, name, max, zero) in [
        (
            "max_output_tokens",
            "maxOutputTokens",
            9_007_199_254_740_991.0,
            true,
        ),
        ("timeout_ms", "timeoutMs", 2_147_483_647.0, false),
    ] {
        if let Some(value) = object.get(key) {
            let valid = value.as_f64().is_some_and(|n| {
                n.is_finite() && n.fract() == 0.0 && n >= if zero { 0.0 } else { 1.0 } && n <= max
            });
            if !valid {
                return Err(if zero {
                    "@options field `max_output_tokens` must be a non-negative safe integer".into()
                } else {
                    "@options field `timeout_ms` must be a positive integer up to 2147483647".into()
                });
            }
            options.insert(name.into(), value.clone());
        }
    }
    Ok(json!({"code": code, "options": options}))
}
fn json_parse_error(text: &str, error: &serde_json::Error) -> String {
    let line = error.line();
    let column = error.column();
    let byte = text
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>()
        + column.saturating_sub(1);
    let position = text[..byte.min(text.len())].encode_utf16().count();
    let message = error.to_string();
    if message.starts_with("key must be a string") {
        return format!(
            "Expected property name or '}}' in JSON at position {position} (line {line} column {column})"
        );
    }
    if error.is_eof() {
        return "Unexpected end of JSON input".into();
    }
    message
}
