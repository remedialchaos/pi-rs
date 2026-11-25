//! ASCII tool identifiers, preserving JavaScript's code-point iteration.
#![forbid(unsafe_code)]
pub fn to_codemode_identifier(name: String) -> Result<String, String> {
    let identifier: String = name
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if c.is_ascii_alphabetic() || c == '_' || c == '$' || (i > 0 && c.is_ascii_digit()) {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(if identifier.is_empty() {
        "_".into()
    } else {
        identifier
    })
}
