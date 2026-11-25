//! Combine piped stdin, file attachments, and the first positional message.

use std::path::Path;

use pi_rs_ai::ImageContent;

use super::args::Args;
use super::file_processor::{FileError, ProcessedFiles, process_file_arguments};

#[derive(Debug, Default, PartialEq)]
pub struct InitialMessage {
    pub initial_message: Option<String>,
    pub initial_images: Option<Vec<ImageContent>>,
}

pub fn build_initial_message(
    parsed: &mut Args,
    files: ProcessedFiles,
    stdin_content: Option<String>,
) -> InitialMessage {
    let mut initial_message = stdin_content;
    if !files.text.is_empty() {
        if let Some(message) = &mut initial_message {
            message.push_str(&files.text);
        } else {
            initial_message = Some(files.text);
        }
    }
    if !parsed.messages.is_empty() {
        let first_message = parsed.messages.remove(0);
        if let Some(message) = &mut initial_message {
            message.push_str(&first_message);
        } else {
            initial_message = Some(first_message);
        }
    }
    InitialMessage {
        initial_message,
        initial_images: (!files.images.is_empty()).then_some(files.images),
    }
}

pub fn prepare_initial_message(
    parsed: &mut Args,
    cwd: &Path,
    stdin_content: Option<String>,
) -> Result<InitialMessage, FileError> {
    // Model-specific resizing happens in AgentSession after extension hooks.
    let files = process_file_arguments(&parsed.file_args, cwd, false)?;
    Ok(build_initial_message(parsed, files, stdin_content))
}
