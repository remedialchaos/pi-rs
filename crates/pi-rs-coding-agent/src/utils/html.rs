//! HTML entity decoding, port of `packages/coding-agent/src/utils/html.ts`.
//!
//! Note on indices: upstream uses JavaScript UTF-16 indices; this port uses Rust
//! byte indices. For ASCII entity syntax (`&...;`) the two agree.

/// The decoded text and the number of characters (including `&` and `;`) consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHtmlEntity {
    pub text: String,
    pub length: usize,
}

/// Decode a Unicode code point.
///
/// Upstream `String.fromCodePoint` accepts surrogate code points and returns lone
/// surrogates. Rust `char` cannot represent those, so surrogates are rejected here
/// (returned as `None`) along with out-of-range values.
pub fn decode_code_point(code_point: u32) -> Option<char> {
    char::from_u32(code_point)
}

/// JavaScript `parseInt` semantics for a leading run of digits: skip leading
/// whitespace, accept an optional sign, consume digits until a non-digit, and
/// return `None` when no digit was found (upstream would produce `NaN`).
fn parse_int_prefix(input: &str, radix: u32) -> Option<u32> {
    let input = input.trim_start();
    let (negative, digits) = match input.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => match input.strip_prefix('+') {
            Some(rest) => (false, rest),
            None => (false, input),
        },
    };
    let mut value: u64 = 0;
    let mut found_digit = false;
    for c in digits.chars() {
        match c.to_digit(radix) {
            Some(digit) => {
                found_digit = true;
                value = value
                    .saturating_mul(u64::from(radix))
                    .saturating_add(u64::from(digit));
            }
            None => break,
        }
    }
    if !found_digit || negative {
        return None;
    }
    u32::try_from(value).ok()
}

/// Decode an HTML entity body (without `&` or `;`).
///
/// Named entities are case-sensitive; numeric forms accept trailing junk just like
/// `Number.parseInt` (`#x41junk` decodes as `A`).
pub fn decode_html_entity(entity: &str) -> Option<String> {
    match entity {
        "amp" => return Some("&".to_string()),
        "lt" => return Some("<".to_string()),
        "gt" => return Some(">".to_string()),
        "quot" => return Some("\"".to_string()),
        "apos" => return Some("'".to_string()),
        _ => {}
    }

    if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        return parse_int_prefix(hex, 16)
            .and_then(decode_code_point)
            .map(|c| c.to_string());
    }
    if let Some(decimal) = entity.strip_prefix('#') {
        return parse_int_prefix(decimal, 10)
            .and_then(decode_code_point)
            .map(|c| c.to_string());
    }
    None
}

/// Decode the entity starting at `index` (which must point at `&`).
///
/// Upstream limits the body to 15 characters (`semicolon_index - index > 16`
/// rejects longer matches), so `&abcdefghijklmno;` is the longest accepted form.
/// Returns `None` when `index` is not a char boundary, no `;` follows, the entity is
/// too long, or the entity body is not recognized.
pub fn decode_html_entity_at(html: &str, index: usize) -> Option<DecodedHtmlEntity> {
    let rest = html.get(index + 1..)?;
    let semicolon_offset = rest.find(';')?;
    let semicolon_index = index + 1 + semicolon_offset;
    if semicolon_index - index > 16 {
        return None;
    }
    let entity = html.get(index + 1..semicolon_index)?;
    let decoded = decode_html_entity(entity)?;
    Some(DecodedHtmlEntity {
        text: decoded,
        length: semicolon_index - index + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_named_entities() {
        for (entity, expected) in [
            ("amp", "&"),
            ("lt", "<"),
            ("gt", ">"),
            ("quot", "\""),
            ("apos", "'"),
        ] {
            assert_eq!(decode_html_entity(entity), Some(expected.to_string()));
        }
        assert_eq!(decode_html_entity("Amp"), None);
        assert_eq!(decode_html_entity("nbsp"), None);
    }

    #[test]
    fn decodes_numeric_entities_with_trailing_junk() {
        assert_eq!(decode_html_entity("#x41"), Some("A".to_string()));
        assert_eq!(decode_html_entity("#X41junk"), Some("A".to_string()));
        assert_eq!(decode_html_entity("#65"), Some("A".to_string()));
        assert_eq!(decode_html_entity("#65xyz"), Some("A".to_string()));
        assert_eq!(decode_html_entity("#x1F600"), Some("\u{1f600}".to_string()));
        assert_eq!(decode_html_entity("#x"), None);
        assert_eq!(decode_html_entity("#"), None);
    }

    #[test]
    fn code_point_bounds_and_surrogates() {
        assert_eq!(decode_code_point(0), Some('\0'));
        assert_eq!(decode_code_point(0x10ffff), Some('\u{10ffff}'));
        assert_eq!(decode_code_point(0x110000), None);
        assert_eq!(decode_code_point(0xd800), None);
    }

    #[test]
    fn decodes_entity_at_and_reports_length() {
        let decoded = decode_html_entity_at("x&amp;y", 1).expect("expected an entity at index 1");
        assert_eq!(decoded.text, "&");
        assert_eq!(decoded.length, 5);
        assert_eq!(
            decode_html_entity_at("&#x41;", 0)
                .expect("expected a numeric entity")
                .text,
            "A"
        );
    }

    #[test]
    fn decodes_entity_at_enforces_sixteen_character_limit() {
        // `&` + 15 body chars + `;` = 17 total, so `length` would be 17 > 16.
        assert_eq!(decode_html_entity_at("&abcdefghijklmno;", 0), None);
        // `&` + 14 body chars + `;` = 16 total: within the limit (but still unknown).
        assert_eq!(decode_html_entity_at("&abcdefghijklmn;", 0), None);
        assert_eq!(decode_html_entity_at("&amp no terminator", 0), None);
        assert_eq!(decode_html_entity_at("&amp;", 9), None);
    }
}
