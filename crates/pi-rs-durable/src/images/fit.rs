use bytes::Bytes;
use pi_rs_ai::{ImageContent, InputContent, Model};
use serde::{Deserialize, Serialize};

use crate::tools::{
    DiagnosticSeverity, ToolDiagnostic, ToolError, ToolResult,
    image::detect_supported_image_mime_type,
};

use super::{ImageLimits, ImageProcessor, from_base64, image_limits, prepared_notes};

pub const FIT_IMAGES_EXTENSION_NAME: &str = "fit-images";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FittedImage {
    Fitted {
        image: ImageContent,
        notes: Vec<String>,
    },
    Omitted {
        note: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FittedImages {
    Unchanged,
    Fitted {
        content: Vec<InputContent>,
        notes: Vec<String>,
    },
}

fn omitted(mime_type: &str, error: Option<&dyn std::fmt::Display>) -> FittedImage {
    let mut note =
        format!("An image ({mime_type}) cannot be prepared for the model and was left out");
    if let Some(error) = error {
        use std::fmt::Write as _;
        let _ = write!(note, ": {error}");
    }
    note.push('.');
    FittedImage::Omitted { note }
}

/// Validates and fits the actual image, correcting the declared MIME type if necessary.
pub async fn fit_image(
    bytes: impl Into<Bytes>,
    mime_type: &str,
    limits: ImageLimits,
    processor: &dyn ImageProcessor,
) -> FittedImage {
    let bytes = bytes.into();
    let preparation_type = detect_supported_image_mime_type(&bytes).unwrap_or(mime_type);
    match processor.prepare(bytes, preparation_type, limits).await {
        Ok(Some(prepared)) => {
            let notes = prepared_notes(&prepared);
            FittedImage::Fitted {
                image: prepared.into(),
                notes,
            }
        }
        Ok(None) => omitted(mime_type, None),
        Err(error) => omitted(mime_type, Some(&error)),
    }
}

/// Preserves content order and avoids copying the transcript when every image stays as it was.
pub async fn fit_images(
    content: &[InputContent],
    limits: ImageLimits,
    processor: &dyn ImageProcessor,
) -> FittedImages {
    let mut changes = None::<Vec<InputContent>>;
    let mut notes = Vec::new();
    for (index, item) in content.iter().enumerate() {
        let InputContent::Image(original) = item else {
            if let Some(changed) = &mut changes {
                changed.push(item.clone());
            }
            continue;
        };
        let result = match from_base64(&original.data) {
            Ok(bytes) => fit_image(bytes, &original.mime_type, limits, processor).await,
            Err(error) => omitted(&original.mime_type, Some(&error)),
        };
        match result {
            FittedImage::Fitted {
                image,
                notes: image_notes,
            } if &image != original => {
                changes
                    .get_or_insert_with(|| content[..index].to_vec())
                    .push(InputContent::Image(image));
                notes.extend(image_notes);
            }
            FittedImage::Fitted { .. } => {
                if let Some(changed) = &mut changes {
                    changed.push(item.clone());
                }
            }
            FittedImage::Omitted { note } => {
                changes.get_or_insert_with(|| content[..index].to_vec());
                notes.push(note);
            }
        }
    }
    changes.map_or(FittedImages::Unchanged, |content| FittedImages::Fitted {
        content,
        notes,
    })
}

/// Invoke before committing a tool result. Nested calls and results without images do not consult
/// the model hook. Returns whether the tool's result changed.
pub async fn fit_tool_result<D, F>(
    nested: bool,
    result: &mut ToolResult<D>,
    processor: &dyn ImageProcessor,
    model: impl FnOnce() -> F,
) -> Result<bool, ToolError>
where
    F: Future<Output = Result<Option<Model>, ToolError>>,
{
    let Some(output) = result.output.as_mut().filter(|output| {
        !nested
            && output
                .iter()
                .any(|item| matches!(item, InputContent::Image(_)))
    }) else {
        return Ok(false);
    };
    let model = model().await?;
    let FittedImages::Fitted { content, notes } =
        fit_images(output, image_limits(model.as_ref()), processor).await
    else {
        return Ok(false);
    };
    *output = content;
    if !notes.is_empty() {
        result
            .diagnostics
            .get_or_insert_with(Vec::new)
            .push(ToolDiagnostic::new(
                DiagnosticSeverity::Info,
                Some("image"),
                notes.join(" "),
            ));
    }
    Ok(true)
}
