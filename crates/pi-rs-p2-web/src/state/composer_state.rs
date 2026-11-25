//! App-scoped composer state and draft logic (upstream `composer-state.tsx`, `composer-focus.ts`).

/// Maximum size of a picked file (25 MB), matching upstream `addFiles`.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// A file picked for the draft. File contents live in the host; the key identifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickedFile {
    pub key: String,
    pub name: String,
    pub preview: bool,
    pub uploaded: Option<String>,
    pub session: Option<String>,
}

impl PickedFile {
    /// A picked file's reference in the draft: `![name](attachment:key)` for an image,
    /// `[name](attachment:key)` otherwise (upstream `pickedReference`).
    pub fn reference(&self) -> String {
        let bang = if self.preview { "!" } else { "" };
        format!("{bang}[{}](attachment:{})", self.name, self.key)
    }
}

/// The keys of attachment references found in `draft`, in draft order (duplicates once).
///
/// Upstream uses `fileReferences(draft)` from `runtime/references.ts`: a scan of
/// `[...](attachment:<key>)` links. Kept here as portable logic.
pub fn attachment_keys(draft: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = draft.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find the start of an attachment destination.
        if bytes[i..].starts_with(b"](attachment:") {
            let start = i + b"](attachment:".len();
            let mut end = start;
            while end < bytes.len()
                && bytes[end] != b')'
                && bytes[end] != b' '
                && bytes[end] != b'\n'
            {
                end += 1;
            }
            if end > start && end < bytes.len() && bytes[end] == b')' {
                let key = &draft[start..end];
                if !keys.iter().any(|existing| existing == key) {
                    keys.push(key.to_owned());
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    keys
}

/// The picked files the draft references, in the draft's order (upstream `referencedFiles`).
pub fn referenced_files(draft: &str, files: &[PickedFile]) -> Vec<PickedFile> {
    let by_key: std::collections::HashMap<&str, &PickedFile> =
        files.iter().map(|file| (file.key.as_str(), file)).collect();
    let mut picked: Vec<&PickedFile> = Vec::new();
    for key in attachment_keys(draft) {
        if let Some(file) = by_key.get(key.as_str())
            && !picked.iter().any(|existing| existing.key == file.key)
        {
            picked.push(file);
        }
    }
    picked.into_iter().cloned().collect()
}

/// The result of validating a batch of chosen files (upstream `addFiles`).
#[derive(Debug, PartialEq, Eq)]
pub struct PickResult {
    pub picked: Vec<PickedFile>,
    /// The name of the first oversized file, if any.
    pub too_large: Option<String>,
}

/// Validates chosen files by size and builds picked files with generated keys.
///
/// `key_for` supplies a unique key per accepted file (upstream uses `crypto.randomUUID()`);
/// `is_image` decides whether a preview reference is used.
pub fn pick_files<F, G>(chosen: &[(String, u64, bool)], key_for: F, is_image: G) -> PickResult
where
    F: Fn(usize) -> String,
    G: Fn(&str) -> bool,
{
    let too_large = chosen
        .iter()
        .find(|(_, size, _)| *size > MAX_FILE_BYTES)
        .map(|(name, _, _)| name.clone());
    let picked = chosen
        .iter()
        .enumerate()
        .filter(|(_, (_, size, _))| *size <= MAX_FILE_BYTES)
        .map(|(index, (name, _, _))| PickedFile {
            key: key_for(index),
            name: name.clone(),
            preview: is_image(name),
            uploaded: None,
            session: None,
        })
        .collect();
    PickResult { picked, too_large }
}

/// The JSON stored under `p2-draft`: `{"key": <conversation>, "text": <draft>}`.
pub fn encode_draft(key: &str, text: &str) -> String {
    let obj = serde_json::json!({ "key": key, "text": text });
    obj.to_string()
}

/// Reads a stored draft, returning `(key, text)` only for a shaped object with string fields.
pub fn decode_draft(stored: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(stored).ok()?;
    let key = value.get("key")?.as_str()?;
    let text = value.get("text")?.as_str()?;
    Some((key.to_owned(), text.to_owned()))
}

/// A restored draft's file references lose their keys: attachments went with the old page,
/// so only their names remain (upstream `renameReferences(text, () => undefined)`).
pub fn rename_references(draft: &str) -> String {
    let mut out = String::with_capacity(draft.len());
    let bytes = draft.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"](attachment:") {
            // Emit the link text (the name) and drop `](attachment:<key>)`.
            if let Some(open) = out.rfind('[') {
                let name = out[open + 1..].to_owned();
                out.truncate(open);
                out.push_str(&name);
            }
            i += b"](attachment:".len();
            while i < bytes.len() && bytes[i] != b')' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b')' {
                i += 1;
            }
        } else {
            let ch = draft[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(key: &str, name: &str, preview: bool) -> PickedFile {
        PickedFile {
            key: key.into(),
            name: name.into(),
            preview,
            uploaded: None,
            session: None,
        }
    }

    #[test]
    fn image_and_file_references() {
        assert_eq!(
            file("k", "pic.png", true).reference(),
            "![pic.png](attachment:k)"
        );
        assert_eq!(
            file("k", "notes.txt", false).reference(),
            "[notes.txt](attachment:k)"
        );
    }

    #[test]
    fn referenced_files_follow_draft_order_and_skip_removed_pills() {
        let files = vec![file("a", "a.txt", false), file("b", "b.txt", false)];
        let draft = "see [b.txt](attachment:b) then [a.txt](attachment:a) and [gone](attachment:z)";
        assert_eq!(
            referenced_files(draft, &files),
            vec![file("b", "b.txt", false), file("a", "a.txt", false)]
        );
    }

    #[test]
    fn oversized_files_are_reported_and_excluded() {
        let chosen = vec![
            ("ok.txt".to_owned(), 10, false),
            ("big.bin".to_owned(), MAX_FILE_BYTES + 1, false),
        ];
        let result = pick_files(
            &chosen,
            |i| format!("key{i}"),
            |name| name.ends_with(".png"),
        );
        assert_eq!(result.too_large.as_deref(), Some("big.bin"));
        assert_eq!(result.picked.len(), 1);
        assert_eq!(result.picked[0].key, "key0");
    }

    #[test]
    fn draft_round_trip_and_key_mismatch() {
        let encoded = encode_draft("s#1", "hello");
        assert_eq!(decode_draft(&encoded), Some(("s#1".into(), "hello".into())));
        assert_eq!(decode_draft("{bad json"), None);
        assert_eq!(decode_draft(r#"{"key":"s#1"}"#), None);
    }

    #[test]
    fn renaming_references_keeps_names_only() {
        assert_eq!(
            rename_references("see [pic.png](attachment:abc) now"),
            "see pic.png now"
        );
    }
}
