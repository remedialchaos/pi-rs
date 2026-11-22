//! Expand `@file` arguments into prompt text and inline images.

use std::error::Error;
use std::fmt::{self, Write};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pi_rs_ai::{ImageContent, ImageContentKind};

use crate::core::tools::path_utils::resolve_read_path;
use crate::utils::image_process::{ProcessImageOptions, ProcessImageResult, process_image};
use crate::utils::mime::{IMAGE_TYPE_SNIFF_BYTES, detect_supported_image_mime_type};
use crate::utils::text::strip_bom;

#[derive(Debug, Default, PartialEq)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ImageContent>,
}

#[derive(Debug)]
pub enum FileError {
    NotFound(PathBuf),
    Read { path: PathBuf, source: io::Error },
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(path) => write!(formatter, "File not found: {}", path.display()),
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "Could not read file {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for FileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound(_) => None,
            Self::Read { source, .. } => Some(source),
        }
    }
}

pub fn process_file_arguments(
    file_args: &[String],
    cwd: &Path,
    auto_resize_images: bool,
) -> Result<ProcessedFiles, FileError> {
    let mut result = ProcessedFiles::default();
    for file in file_args {
        let path = resolve_read_path(file, cwd);
        let Ok(metadata) = fs::metadata(&path) else {
            return Err(FileError::NotFound(path));
        };
        if metadata.len() == 0 {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) => return Err(FileError::Read { path, source }),
        };
        let _ = write!(result.text, "<file name=\"{}\">", path.display());
        let prefix = &bytes[..bytes.len().min(IMAGE_TYPE_SNIFF_BYTES)];
        if let Some(mime_type) = detect_supported_image_mime_type(prefix) {
            let options = ProcessImageOptions {
                auto_resize_images: Some(auto_resize_images),
                ..ProcessImageOptions::default()
            };
            match process_image(&bytes, mime_type, options) {
                ProcessImageResult::Ok {
                    data,
                    mime_type,
                    hints,
                } => {
                    result.images.push(ImageContent {
                        kind: ImageContentKind::Image,
                        data,
                        mime_type,
                    });
                    for (index, hint) in hints.iter().enumerate() {
                        if index > 0 {
                            result.text.push('\n');
                        }
                        result.text.push_str(hint);
                    }
                }
                ProcessImageResult::Err { message } => result.text.push_str(&message),
            }
        } else {
            result.text.push('\n');
            result
                .text
                .push_str(strip_bom(&String::from_utf8_lossy(&bytes)));
            result.text.push('\n');
        }
        result.text.push_str("</file>\n");
    }
    Ok(result)
}
