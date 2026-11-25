//! Strip `//` line comments and trailing commas from JSON, leaving string literals
//! untouched. Port of `packages/coding-agent/src/utils/json.ts`.
//!
//! Upstream does this in two regex passes:
//! 1. `/"(?:\\.|[^"\\])*"|\/\/[^\n]*/g` — remove `//` to end of line (including `\r`),
//!    but keep string literals.
//! 2. `/"(?:\\.|[^"\\])*"|,(\s*[}\]])/g` — remove a comma whose next non-whitespace
//!    character is `}` or `]`, but keep string literals.
//!
//! There is no block-comment handling. Both passes are implemented as a small state
//! machine that tracks whether it is inside a string and whether the previous byte
//! was an escape. Iteration is over `char`s so non-ASCII text is preserved exactly.

/// Return true for characters matched by JavaScript's `\s` (plus a few Unicode
/// whitespace characters that matter for JSON pretty printing).
fn is_js_whitespace(c: char) -> bool {
    c.is_ascii_whitespace() || c.is_whitespace() || c == '\u{feff}'
}

/// Pass one: remove `//` line comments outside string literals.
fn strip_line_comments(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;

    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push('"');
            i += 1;
        } else if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            // Skip everything up to (but not including) the newline. JS `[^\n]*`
            // consumes the `\r` of a CRLF pair as well.
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Pass two: remove trailing commas outside string literals.
fn strip_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;

    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }

        if c == ',' {
            // Look ahead over whitespace; if a closing bracket follows, this is a
            // trailing comma. The comma is dropped and the whitespace/bracket chars
            // are copied by the same or subsequent iterations.
            let mut j = i + 1;
            let mut tail = String::new();
            while j < chars.len() && is_js_whitespace(chars[j]) {
                tail.push(chars[j]);
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                out.push_str(&tail);
                out.push(chars[j]);
                i = j + 1;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Strip `//` line comments and trailing commas from JSON, preserving strings.
pub fn strip_json_comments(input: &str) -> String {
    strip_trailing_commas(&strip_line_comments(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_line_comments() {
        let input = "{\n  // comment\n  \"a\": 1, // trailing\n  \"b\": 2\n}";
        assert_eq!(
            strip_json_comments(input),
            "{\n  \n  \"a\": 1, \n  \"b\": 2\n}"
        );
    }

    #[test]
    fn removes_trailing_commas_with_whitespace() {
        assert_eq!(
            strip_json_comments("{\"a\": [1, 2, ],\"b\": {\"c\": 3,} }"),
            "{\"a\": [1, 2 ],\"b\": {\"c\": 3} }"
        );
        assert_eq!(strip_json_comments("[1,\n2,\n]"), "[1,\n2\n]");
    }

    #[test]
    fn preserves_comment_markers_inside_strings() {
        let input = "{\"url\": \"https://example.com\", \"text\": \"a, \\\"b\\\", }\"}";
        assert_eq!(strip_json_comments(input), input);
    }

    #[test]
    fn preserves_non_ascii_text() {
        let input = "{\"name\": \"caf\u{e9} \u{4e2d}\u{6587}\" // note\n}";
        assert_eq!(
            strip_json_comments(input),
            "{\"name\": \"caf\u{e9} \u{4e2d}\u{6587}\" \n}"
        );
    }

    #[test]
    fn does_not_handle_block_comments() {
        // Upstream has no block-comment handling; verify parity.
        assert_eq!(strip_json_comments("{\"a\" /*x*/: 1}"), "{\"a\" /*x*/: 1}");
    }
}
