use std::borrow::Cow;

use pi_rs_chord::context::Context;
use pi_rs_schema::{Annotations, ArrOptions, Properties, arr, obj};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::env::{ExecutionEnv, FileError, FileErrorCode, FileKind};

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_edit_diffs,
    normalize_to_lf, restore_line_endings, strip_bom,
};
use super::{
    ToolError, ToolResult, ToolSpec, check_aborted, described_str, require_env, resolve_tool_path,
    text_output, with_file_mutation_queue,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditToolInput {
    pub path: String,
    pub edits: Vec<Edit>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    pub diff: String,
    pub patch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}

fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
}

/// Repairs common model argument shapes without changing the caller's original input.
pub fn prepare_edit_arguments(input: &Value) -> Value {
    let mut input = input.clone();
    let Some(args) = input.as_object_mut() else {
        return input;
    };
    if let Some(edits) = args.get_mut("edits") {
        if let Some(string) = edits.as_str() {
            match serde_json::from_str::<Value>(string) {
                Ok(parsed) if parsed.is_array() => *edits = parsed,
                Ok(parsed) if is_single_edit(&parsed) => *edits = Value::Array(vec![parsed]),
                _ => {}
            }
        } else if is_single_edit(edits) {
            *edits = Value::Array(vec![std::mem::take(edits)]);
        }
    }
    if args.get("oldText").is_some_and(Value::is_string)
        && args.get("newText").is_some_and(Value::is_string)
    {
        let old = args.shift_remove("oldText");
        let new = args.shift_remove("newText");
        let value = args
            .entry("edits")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !value.is_array() {
            *value = Value::Array(Vec::new());
        }
        if let Some(edits) = value.as_array_mut() {
            edits.push(json!({ "oldText": old, "newText": new }));
        }
    }
    input
}

pub struct EditTool(ToolSpec);

pub fn create_edit_tool() -> EditTool {
    let edit = obj(&Properties::new()
        .with("oldText", &described_str("Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."))
        .with("newText", &described_str("Replacement text for this targeted edit.")), &Annotations::new(), true);
    let edits = arr(&edit, &ArrOptions::new().annotations(Annotations::new().description(
        "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.")));
    EditTool(ToolSpec {
        name: "edit",
        description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".into(),
        parameters: obj(&Properties::new()
            .with("path", &described_str("Path to the file to edit (relative or absolute)"))
            .with("edits", &edits), &Annotations::new(), true),
        structured_output_schema: None,
        output_limits: None,
    })
}

fn edit_access(path: &str, source: FileError) -> ToolError {
    let code = match source.code {
        FileErrorCode::Aborted => "aborted",
        FileErrorCode::NotFound => "not_found",
        FileErrorCode::PermissionDenied => "permission_denied",
        FileErrorCode::NotDirectory => "not_directory",
        FileErrorCode::IsDirectory => "is_directory",
        FileErrorCode::Invalid => "invalid",
        FileErrorCode::NotSupported => "not_supported",
        FileErrorCode::Unknown => "unknown",
    };
    ToolError::EditAccess {
        path: path.to_owned(),
        code,
        source,
    }
}

impl EditTool {
    pub fn spec(&self) -> &ToolSpec {
        &self.0
    }

    pub async fn execute_json(
        &self,
        args: &Value,
        env: Option<&dyn ExecutionEnv>,
        context: &Context,
    ) -> Result<ToolResult<EditToolDetails>, ToolError> {
        self.execute(
            serde_json::from_value(prepare_edit_arguments(args))?,
            env,
            context,
        )
        .await
    }

    pub async fn execute(
        &self,
        args: EditToolInput,
        env: Option<&dyn ExecutionEnv>,
        context: &Context,
    ) -> Result<ToolResult<EditToolDetails>, ToolError> {
        let EditToolInput { path, edits } = args;
        if edits.is_empty() {
            return Err(ToolError::Message(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            ));
        }
        let env = require_env(env)?;
        let absolute = resolve_tool_path(env, &path, context).await?;
        with_file_mutation_queue(
            env,
            &absolute,
            async || {
                check_aborted(context)?;
                let info = env
                    .file_info(&absolute, context)
                    .await
                    .map_err(|err| edit_access(&path, err))?;
                if !matches!(info.kind, FileKind::File | FileKind::Symlink) {
                    return Err(ToolError::Message(format!(
                        "Could not edit file: {path}. Path is not a file."
                    )));
                }
                let raw = env
                    .read_text_file(&absolute, context)
                    .await
                    .map_err(|err| edit_access(&path, err))?;
                check_aborted(context)?;
                let (bom, content) = strip_bom(&raw);
                let ending = detect_line_ending(content);
                let normalized = normalize_to_lf(content);
                let applied = apply_edits_to_normalized_content(&normalized, &edits, &path)?;
                check_aborted(context)?;
                let final_text = restore_line_endings(&applied.new_content, ending);
                let final_text = if bom.is_empty() {
                    final_text
                } else {
                    Cow::Owned(format!("{bom}{final_text}"))
                };
                env.write_file(&absolute, final_text.as_bytes(), context)
                    .await
                    .map_err(|err| edit_access(&path, err))?;
                check_aborted(context)?;
                let (display, patch) =
                    generate_edit_diffs(&path, applied.base_content, &applied.new_content);
                Ok(ToolResult {
                    output: Some(vec![text_output(format!(
                        "Successfully replaced {} block(s) in {path}.",
                        edits.len()
                    ))]),
                    details: Some(EditToolDetails {
                        diff: display.diff,
                        patch,
                        first_changed_line: display.first_changed_line,
                    }),
                    ..ToolResult::default()
                })
            },
            context,
        )
        .await
    }
}
