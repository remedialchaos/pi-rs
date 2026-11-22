//! Names of MCP tools and of their servers' namespaces, and the servers' one-line summaries, as
//! models and code mode scripts see them (`packages/mcp/src/naming.ts`). Applications that name
//! tools the same way rank and group them the same way in tool search and code mode.

use sha2::{Digest, Sha256};

/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME_LENGTH: usize = 64;

/// `mcp__<server>__<tool>`, sanitized and shortened with a hash suffix when too long. Like Codex,
/// everything but `[A-Za-z0-9_]` becomes `_`, so the name is also the identifier code mode scripts
/// call it by. `is_taken` reports names used by a different MCP tool: sanitizing can map two tools
/// to one name (`a-b` and `a_b`), which then get the hash suffix.
pub fn create_mcp_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name: String = format!("mcp__{server}__{tool}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let mut digest = Sha256::new();
    digest.update(server.as_bytes());
    digest.update([0]);
    digest.update(tool.as_bytes());
    let hash = format!("{:x}", digest.finalize());
    // The sanitized name is ASCII, so slicing by bytes is slicing by characters.
    let suffix = &hash[..8];
    let keep = MAX_TOOL_NAME_LENGTH - suffix.len() - 1;
    // Like a JavaScript slice, the prefix keeps what the (shorter) name has.
    let prefix_end = keep.min(name.len());
    format!("{}_{suffix}", &name[..prefix_end])
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by `_`, like the tool names.
/// Server names that differ only in `-` and `_` share a namespace, so a configuration must not
/// have both.
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// A server's one-line summary for the system prompt: the first line of its configured
/// `description`, else of the instructions it sent in `initialize`; empty when it has neither.
pub fn mcp_server_summary(description: Option<&str>, instructions: Option<&str>) -> String {
    let text = js_trim(description)
        .filter(|text| !text.is_empty())
        .or_else(|| js_trim(instructions).filter(|text| !text.is_empty()))
        .unwrap_or_default();
    js_trim(text.split('\n').next())
        .unwrap_or_default()
        .to_string()
}

/// `String.prototype.trim()` per ECMAScript: `WhiteSpace` (space, tab, VT, FF, NBSP, ZWNBSP and the
/// Unicode space separators) and `LineTerminator` (LF, CR, LS, PS) at either end.
fn js_trim(text: Option<&str>) -> Option<&str> {
    text.map(|text| {
        let is_space = |c: char| {
            matches!(
                c,
                '\u{0009}'
                    | '\u{000A}'
                    | '\u{000B}'
                    | '\u{000C}'
                    | '\u{000D}'
                    | '\u{0020}'
                    | '\u{00A0}'
                    | '\u{1680}'
                    | '\u{2000}'
                    ..='\u{200A}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{202F}'
                        | '\u{205F}'
                        | '\u{3000}'
                        | '\u{FEFF}'
            )
        };
        text.trim_matches(is_space)
    })
}
