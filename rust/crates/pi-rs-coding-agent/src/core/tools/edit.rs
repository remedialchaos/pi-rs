//! Targeted file editing tool ported from `core/tools/edit.ts`.

use std::fs;
use std::fs::OpenOptions;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, apply_replacements_to_original, generate_edit_diff,
    normalize_to_lf, split_bom,
};
use super::path_utils::resolve_to_cwd;

#[derive(Clone, Debug, Deserialize)]
pub struct EditInput {
    pub path: String,
    #[serde(default)]
    pub edits: Vec<Edit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDetails {
    pub diff: String,
    pub patch: String,
    pub first_changed_line: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct EditExecution {
    pub output: String,
    pub details: EditDetails,
}

fn edit_from_value(value: &Value) -> Option<Edit> {
    let old_text = value.get("oldText")?.as_str()?.to_string();
    let new_text = value.get("newText")?.as_str()?.to_string();
    Some(Edit { old_text, new_text })
}

fn edits_from_value(value: &Value) -> Vec<Edit> {
    match value {
        Value::Array(items) => items.iter().filter_map(edit_from_value).collect(),
        Value::Object(_) => edit_from_value(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Accept `edits` as a JSON string, array, or single object, plus legacy
/// top-level `oldText`/`newText` fields (`prepareEditArguments`).
pub fn prepare_edit_arguments(value: &Value) -> Result<EditInput, String> {
    let Some(path) = value.get("path").and_then(Value::as_str) else {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    };

    let mut edits = match value.get("edits") {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(parsed) => edits_from_value(&parsed),
            Err(_) => Vec::new(),
        },
        Some(other) => edits_from_value(other),
        None => Vec::new(),
    };

    if let (Some(old_text), Some(new_text)) = (
        value.get("oldText").and_then(Value::as_str),
        value.get("newText").and_then(Value::as_str),
    ) {
        edits.push(Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }

    if edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    }

    Ok(EditInput {
        path: path.to_string(),
        edits,
    })
}

fn io_error_code(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(2) => "ENOENT".to_string(),
        Some(13) => "EACCES".to_string(),
        Some(code) => format!("E{code}"),
        None => error.kind().to_string(),
    }
}

/// Verify/read/diff/write an edit, returning the success text and diff details.
pub fn execute_edit(input: &EditInput, cwd: &Path) -> Result<EditExecution, String> {
    if input.edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    }
    let absolute_path = resolve_to_cwd(&input.path, cwd);

    if let Err(error) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&absolute_path)
    {
        return Err(format!(
            "Could not edit file: {}. Error code: {}.",
            input.path,
            io_error_code(&error)
        ));
    }

    let raw = fs::read(&absolute_path).map_err(|error| {
        format!(
            "Could not edit file: {}. Error code: {}.",
            input.path,
            io_error_code(&error)
        )
    })?;
    let raw_content = String::from_utf8_lossy(&raw).into_owned();
    let (bom, content) = split_bom(&raw_content);
    let normalized_content = normalize_to_lf(&content);
    let applied =
        apply_edits_to_normalized_content(&normalized_content, &input.edits, &input.path)?;

    // Diff before writing: a diff failure must never leave a half-applied edit.
    let edit_diff = generate_edit_diff(
        &input.path,
        &applied.base_content,
        &applied.new_content,
        &applied.replacements,
        4,
    );

    let final_content = if content == normalized_content {
        format!("{bom}{}", applied.new_content)
    } else {
        format!(
            "{bom}{}",
            apply_replacements_to_original(&content, &applied.replacements)
        )
    };
    fs::write(&absolute_path, final_content).map_err(|error| {
        format!(
            "Could not edit file: {}. Error code: {}.",
            input.path,
            io_error_code(&error)
        )
    })?;

    Ok(EditExecution {
        output: format!(
            "Successfully replaced {} block(s) in {}.",
            input.edits.len(),
            input.path
        ),
        details: EditDetails {
            diff: edit_diff.diff,
            patch: edit_diff.patch,
            first_changed_line: edit_diff.first_changed_line,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_edit_arguments_accepts_json_string_and_legacy_fields() -> Result<(), String> {
        let value = serde_json::json!({
            "path": "f.txt",
            "edits": "[{\"oldText\": \"a\", \"newText\": \"b\"}]"
        });
        let prepared = prepare_edit_arguments(&value)?;
        assert_eq!(prepared.edits.len(), 1);
        assert_eq!(prepared.edits[0].old_text, "a");

        let legacy = serde_json::json!({"path": "f.txt", "oldText": "x", "newText": "y"});
        let prepared = prepare_edit_arguments(&legacy)?;
        assert_eq!(prepared.edits.len(), 1);
        assert_eq!(prepared.edits[0].new_text, "y");

        let empty = serde_json::json!({"path": "f.txt"});
        assert!(prepare_edit_arguments(&empty).is_err());
        Ok(())
    }

    #[test]
    fn edits_file_and_returns_diff_details() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "hello world\n")?;
        let input = EditInput {
            path: "f.txt".into(),
            edits: vec![Edit {
                old_text: "world".into(),
                new_text: "testing".into(),
            }],
        };
        let result = match execute_edit(&input, dir.path()) {
            Ok(result) => result,
            Err(error) => return Err(std::io::Error::other(error)),
        };
        assert_eq!(result.output, "Successfully replaced 1 block(s) in f.txt.");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt"))?,
            "hello testing\n"
        );
        assert!(result.details.patch.starts_with("--- f.txt\n+++ f.txt\n"));
        Ok(())
    }

    #[test]
    fn preserves_bom_and_crlf_across_edits() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("f.txt"),
            "\u{feff}first\r\nsecond\r\nthird\r\n",
        )?;
        let input = EditInput {
            path: "f.txt".into(),
            edits: vec![Edit {
                old_text: "second".into(),
                new_text: "REPLACED".into(),
            }],
        };
        if let Err(error) = execute_edit(&input, dir.path()) {
            return Err(std::io::Error::other(error));
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt"))?,
            "\u{feff}first\r\nREPLACED\r\nthird\r\n"
        );
        Ok(())
    }

    #[test]
    fn missing_file_error_message() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = EditInput {
            path: "missing.txt".into(),
            edits: vec![Edit {
                old_text: "a".into(),
                new_text: "b".into(),
            }],
        };
        let error = match execute_edit(&input, dir.path()) {
            Ok(_) => return Err(std::io::Error::other("expected missing file error")),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "Could not edit file: missing.txt. Error code: ENOENT."
        );
        Ok(())
    }

    #[test]
    fn multiple_disjoint_edits() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\ndelta\n")?;
        let input = EditInput {
            path: "f.txt".into(),
            edits: vec![
                Edit {
                    old_text: "alpha".into(),
                    new_text: "ALPHA".into(),
                },
                Edit {
                    old_text: "gamma".into(),
                    new_text: "GAMMA".into(),
                },
            ],
        };
        let result = match execute_edit(&input, dir.path()) {
            Ok(result) => result,
            Err(error) => return Err(std::io::Error::other(error)),
        };
        assert_eq!(result.output, "Successfully replaced 2 block(s) in f.txt.");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt"))?,
            "ALPHA\nbeta\nGAMMA\ndelta\n"
        );
        Ok(())
    }
}
