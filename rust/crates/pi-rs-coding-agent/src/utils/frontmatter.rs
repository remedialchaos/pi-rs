//! Frontmatter parsing: the Rust port of `packages/coding-agent/src/utils/frontmatter.ts`.
//!
//! Uses `saphyr` (YAML 1.2) to match the `yaml` package's `yes`/`no`-as-string behavior.

use indexmap::IndexMap;
use serde_json::Value;

/// The result of parsing a document with YAML frontmatter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedFrontmatter {
    pub frontmatter: IndexMap<String, Value>,
    pub body: String,
}

fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

fn extract(content: &str) -> (Option<String>, String) {
    let normalized = strip_bom(content).replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|index| index + 3) else {
        return (None, normalized);
    };
    (
        Some(normalized.get(4..end).unwrap_or("").to_string()),
        normalized.get(end + 4..).unwrap_or("").trim().to_string(),
    )
}

/// Converts a `saphyr` YAML tree into a JSON value, preserving mapping order.
fn to_value(yaml: &saphyr::Yaml) -> Value {
    match yaml {
        saphyr::Yaml::Value(scalar) => scalar_to_value(scalar),
        saphyr::Yaml::Sequence(sequence) => Value::Array(sequence.iter().map(to_value).collect()),
        saphyr::Yaml::Mapping(mapping) => {
            let mut out = IndexMap::new();
            for (key, value) in mapping {
                let key = match key {
                    saphyr::Yaml::Value(scalar) => scalar_to_string(scalar),
                    other => format!("{other:?}"),
                };
                out.insert(key, to_value(value));
            }
            serde_json::Value::Object(out.into_iter().collect())
        }
        other => Value::String(format!("{other:?}")),
    }
}

fn scalar_to_string(scalar: &saphyr::Scalar) -> String {
    match scalar {
        saphyr::Scalar::String(text) => text.to_string(),
        saphyr::Scalar::Boolean(boolean) => boolean.to_string(),
        saphyr::Scalar::Integer(number) => number.to_string(),
        saphyr::Scalar::FloatingPoint(number) => number.to_string(),
        saphyr::Scalar::Null => "null".to_string(),
    }
}

fn scalar_to_value(scalar: &saphyr::Scalar) -> Value {
    match scalar {
        saphyr::Scalar::String(text) => Value::String(text.to_string()),
        saphyr::Scalar::Boolean(boolean) => Value::Bool(*boolean),
        saphyr::Scalar::Integer(number) => Value::from(*number),
        saphyr::Scalar::FloatingPoint(number) => Value::from(number.into_inner()),
        saphyr::Scalar::Null => Value::Null,
    }
}

/// Parses frontmatter into an ordered map plus the trimmed body.
pub fn parse_frontmatter(content: &str) -> Result<ParsedFrontmatter, String> {
    let (yaml, body) = extract(content);
    let Some(yaml) = yaml else {
        return Ok(ParsedFrontmatter {
            frontmatter: IndexMap::new(),
            body,
        });
    };
    let documents =
        <saphyr::Yaml as saphyr::LoadableYamlNode>::load_from_str(&yaml).map_err(|error| {
            format!(
                "{} at line {}, column {}",
                error.info(),
                error.marker().line(),
                error.marker().col() + 1
            )
        })?;
    let frontmatter = match documents.first() {
        None | Some(saphyr::Yaml::Value(saphyr::Scalar::Null)) => IndexMap::new(),
        Some(saphyr::Yaml::Mapping(mapping)) => {
            let Value::Object(object) = to_value(&saphyr::Yaml::Mapping(mapping.clone())) else {
                return Ok(ParsedFrontmatter {
                    frontmatter: IndexMap::new(),
                    body,
                });
            };
            object.into_iter().collect()
        }
        Some(value) => {
            let mut out = IndexMap::new();
            out.insert("".to_string(), to_value(value));
            out
        }
    };
    Ok(ParsedFrontmatter { frontmatter, body })
}

/// Returns the body with frontmatter removed, like upstream's `stripFrontmatter`.
pub fn strip_frontmatter(content: &str) -> Result<String, String> {
    Ok(parse_frontmatter(content)?.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys_and_body() {
        let input = "---\nname: \"skill-name\"\ndescription: 'A desc'\n---\n\nBody text";
        let parsed = parse_frontmatter(input).unwrap();
        assert_eq!(
            parsed.frontmatter["name"],
            Value::String("skill-name".to_string())
        );
        assert_eq!(parsed.body, "Body text");
    }

    #[test]
    fn normalizes_crlf() {
        assert_eq!(
            strip_frontmatter("---\r\nname: test\r\n---\r\nLine one\r\nLine two").unwrap(),
            "Line one\nLine two"
        );
    }

    #[test]
    fn empty_frontmatter_is_an_empty_map() {
        assert!(
            parse_frontmatter("---\n# comment\n---\nBody")
                .unwrap()
                .frontmatter
                .is_empty()
        );
    }
}
