//! File writing tool ported from `core/tools/write.ts`.

use std::path::Path;

use serde::Deserialize;

use super::path_utils::resolve_to_cwd;

#[derive(Clone, Debug, Deserialize)]
pub struct WriteInput {
    pub path: String,
    pub content: String,
}

/// Create parent directories, write the file, and return the tool's success text.
pub async fn write_file(input: &WriteInput, cwd: &Path) -> Result<String, String> {
    let absolute_path = resolve_to_cwd(&input.path, cwd);
    let parent = absolute_path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("Failed to create directory: {error}"))?;
    tokio::fs::write(&absolute_path, input.content.as_bytes())
        .await
        .map_err(|error| format!("Failed to write file: {error}"))?;
    Ok(format!("Successfully wrote to {}", input.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_file_and_returns_success_text() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = WriteInput {
            path: "a.txt".into(),
            content: "hello".into(),
        };
        let result = write_file(&input, dir.path()).await;
        match result {
            Ok(text) => assert_eq!(text, "Successfully wrote to a.txt"),
            Err(error) => return Err(std::io::Error::other(error)),
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn creates_nested_parent_directories() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = WriteInput {
            path: "nested/deep/file.txt".into(),
            content: "x".into(),
        };
        if let Err(error) = write_file(&input, dir.path()).await {
            return Err(std::io::Error::other(error));
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/deep/file.txt"))?,
            "x"
        );
        Ok(())
    }
}
