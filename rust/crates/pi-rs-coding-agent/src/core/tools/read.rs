//! File reading tool ported from `core/tools/read.ts`.
//!
//! Image attachments keep the magic-byte detection and note text; resizing and
//! model-image capability checks are handled by the caller in the Rust port.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use serde::Deserialize;

use super::path_utils::resolve_read_path;
use super::truncate::{
    DEFAULT_MAX_BYTES, TruncationOptions, TruncationResult, format_size, truncate_head,
};

#[derive(Clone, Debug, Deserialize)]
pub struct ReadInput {
    pub path: String,
    #[serde(default)]
    pub offset: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOutput {
    Text(String),
    Image {
        data: String,
        mime_type: String,
        note: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadDetails {
    pub truncation: Option<TruncationResult>,
    pub image_unsupported_by_model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ReadExecution {
    pub output: ReadOutput,
    pub details: Option<ReadDetails>,
}

/// Magic-byte MIME detection for the image formats supported upstream.
pub fn detect_image_mime_type(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 16];
    let count = file.read(&mut header).ok()?;
    let bytes = &header[..count];

    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png".to_string())
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg".to_string())
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif".to_string())
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp".to_string())
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp".to_string())
    } else {
        None
    }
}

fn io_error_message(path: &str, error: &std::io::Error) -> String {
    if error.raw_os_error() == Some(2) {
        format!("ENOENT: no such file or directory, open '{path}'")
    } else if error.raw_os_error() == Some(13) {
        format!("EACCES: permission denied, open '{path}'")
    } else {
        error.to_string()
    }
}

/// Execute the read tool and return its structured output plus optional details.
pub fn execute_read(input: &ReadInput, cwd: &Path) -> Result<ReadExecution, String> {
    let absolute_path = resolve_read_path(&input.path, cwd);

    let metadata =
        std::fs::metadata(&absolute_path).map_err(|error| io_error_message(&input.path, &error))?;
    if metadata.is_dir() {
        return Err(format!(
            "EISDIR: illegal operation on a directory, read '{path}'",
            path = input.path
        ));
    }

    if let Some(mime_type) = detect_image_mime_type(&absolute_path) {
        let bytes =
            std::fs::read(&absolute_path).map_err(|error| io_error_message(&input.path, &error))?;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let note = if mime_type == "image/bmp" {
            "Read image file [image/bmp]\n[Image converted from image/bmp to image/png.]"
                .to_string()
        } else {
            format!("Read image file [{mime_type}]")
        };
        return Ok(ReadExecution {
            output: ReadOutput::Image {
                data,
                mime_type,
                note,
            },
            details: None,
        });
    }

    let bytes =
        std::fs::read(&absolute_path).map_err(|error| io_error_message(&input.path, &error))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let all_lines: Vec<&str> = text.split('\n').collect();

    let start_line = match input.offset {
        Some(offset) if offset != 0.0 => ((offset - 1.0).floor().max(0.0)) as usize,
        _ => 0,
    };
    let end_line = match input.limit {
        Some(limit) => {
            let raw = ((start_line as f64 + limit).floor().max(0.0)) as usize;
            raw.max(start_line).min(all_lines.len())
        }
        None => all_lines.len(),
    };
    let total_file_lines = all_lines.len();

    if start_line >= total_file_lines {
        let displayed = input
            .offset
            .map(|value| value.to_string())
            .unwrap_or_else(|| "undefined".to_string());
        return Err(format!(
            "Offset {displayed} is beyond end of file ({total_file_lines} lines total)"
        ));
    }

    let selected = &all_lines[start_line..end_line];
    let truncation = truncate_head(&selected.join("\n"), &TruncationOptions::default());
    let first_line_bytes = if truncation.first_line_exceeds_limit {
        all_lines[start_line].len()
    } else {
        0
    };

    let content = truncation.content.clone();
    let start_line_display = start_line + 1;
    let user_limited_lines = input.limit.map(|limit| {
        let raw = ((start_line as f64 + limit).floor().max(0.0)) as usize;
        raw.min(total_file_lines).saturating_sub(start_line)
    });

    let output_text = if truncation.first_line_exceeds_limit {
        format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {}]",
            format_size(first_line_bytes),
            format_size(DEFAULT_MAX_BYTES),
            input.path,
            DEFAULT_MAX_BYTES
        )
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        if truncation.truncated_by == super::truncate::TruncatedBy::Lines {
            format!(
                "{}\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]",
                content
            )
        } else {
            format!(
                "{}\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                content,
                format_size(DEFAULT_MAX_BYTES)
            )
        }
    } else if let Some(limited) = user_limited_lines {
        if start_line + limited < total_file_lines {
            let remaining = total_file_lines - (start_line + limited);
            let next_offset = start_line + limited + 1;
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                content
            )
        } else {
            content
        }
    } else {
        content
    };

    let details = truncation.truncated.then_some(ReadDetails {
        truncation: Some(truncation),
        image_unsupported_by_model: None,
    });

    Ok(ReadExecution {
        output: ReadOutput::Text(output_text),
        details,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_small_file_verbatim() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "hello\nworld\n")?;
        let input = ReadInput {
            path: "f.txt".into(),
            offset: None,
            limit: None,
        };
        let result = execute_read(&input, dir.path()).map_err(std::io::Error::other)?;
        assert_eq!(
            result.output,
            ReadOutput::Text("hello\nworld\n".to_string())
        );
        assert!(result.details.is_none());
        Ok(())
    }

    #[test]
    fn detects_png_and_text_with_image_extension() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest")?;
        assert_eq!(detect_image_mime_type(&png).as_deref(), Some("image/png"));
        let fake = dir.path().join("b.png");
        std::fs::write(&fake, "just text")?;
        assert_eq!(detect_image_mime_type(&fake), None);
        Ok(())
    }

    #[test]
    fn offset_beyond_end_error() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc")?;
        let input = ReadInput {
            path: "f.txt".into(),
            offset: Some(100.0),
            limit: None,
        };
        let error = match execute_read(&input, dir.path()) {
            Ok(_) => return Err(std::io::Error::other("expected offset error")),
            Err(error) => error,
        };
        assert_eq!(error, "Offset 100 is beyond end of file (3 lines total)");
        Ok(())
    }

    #[test]
    fn user_limit_notice() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let content = (1..=100)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("f.txt"), content)?;
        let input = ReadInput {
            path: "f.txt".into(),
            offset: None,
            limit: Some(10.0),
        };
        let result = execute_read(&input, dir.path()).map_err(std::io::Error::other)?;
        let ReadOutput::Text(text) = result.output else {
            return Err(std::io::Error::other("expected text output"));
        };
        assert!(text.ends_with("[90 more lines in file. Use offset=11 to continue.]"));
        Ok(())
    }

    #[test]
    fn line_limit_notice() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let content = (1..=2500)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("f.txt"), content)?;
        let input = ReadInput {
            path: "f.txt".into(),
            offset: None,
            limit: None,
        };
        let result = execute_read(&input, dir.path()).map_err(std::io::Error::other)?;
        let ReadOutput::Text(text) = result.output else {
            return Err(std::io::Error::other("expected text output"));
        };
        assert!(text.contains("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
        assert!(result.details.is_some());
        Ok(())
    }

    #[test]
    fn first_line_over_byte_cap_notice() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("f.txt"),
            format!("{}\nnext", "x".repeat(60 * 1024)),
        )?;
        let input = ReadInput {
            path: "f.txt".into(),
            offset: None,
            limit: None,
        };
        let result = execute_read(&input, dir.path()).map_err(std::io::Error::other)?;
        let ReadOutput::Text(text) = result.output else {
            return Err(std::io::Error::other("expected text output"));
        };
        assert!(text.starts_with("[Line 1 is 60.0KB, exceeds 50.0KB limit."));
        assert!(text.contains("head -c 51200"));
        Ok(())
    }
}
