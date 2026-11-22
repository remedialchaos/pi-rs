//! UTF-8 BOM helpers (port of `packages/coding-agent/src/utils/text.ts`).

/// A leading byte order mark split away from the rest of the text.
///
/// Both fields borrow from the input, so no allocation is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BomSplit<'a> {
    pub bom: &'a str,
    pub text: &'a str,
}

/// Split a leading UTF-8 byte order mark from decoded text.
///
/// Upstream: `content.startsWith("\uFEFF") ? { bom: "\uFEFF", text: content.slice(1) } : { bom: "", text: content }`.
/// The BOM is three bytes in UTF-8, so slicing the first three bytes is safe once
/// `strip_prefix` succeeded.
pub fn split_bom(content: &str) -> BomSplit<'_> {
    match content.strip_prefix('\u{feff}') {
        Some(text) => BomSplit {
            bom: &content[..3],
            text,
        },
        None => BomSplit {
            bom: "",
            text: content,
        },
    }
}

/// Remove a leading UTF-8 byte order mark from decoded text.
pub fn strip_bom(content: &str) -> &str {
    split_bom(content).text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_leading_bom() {
        let split = split_bom("\u{feff}content");
        assert_eq!(split.bom, "\u{feff}");
        assert_eq!(split.text, "content");
    }

    #[test]
    fn keeps_text_without_bom() {
        let split = split_bom("content");
        assert_eq!(split.bom, "");
        assert_eq!(split.text, "content");
    }

    #[test]
    fn does_not_strip_interior_bom() {
        assert_eq!(strip_bom("a\u{feff}b"), "a\u{feff}b");
        assert_eq!(strip_bom("\u{feff}\u{feff}x"), "\u{feff}x");
        assert_eq!(strip_bom("plain"), "plain");
    }
}
