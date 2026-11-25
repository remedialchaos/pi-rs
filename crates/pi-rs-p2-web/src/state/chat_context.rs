//! What the chat's parts get from where a transcript is shown (upstream `chat-context.ts`).

/// A path as the session sees it: relative to `cwd` when the path is inside it.
///
/// Upstream `relativePath`. Both arguments must be absolute (start with `/`) for a
/// relative path to be produced; otherwise the path is returned unchanged. A path
/// outside the cwd keeps the `..` segments needed to climb back to a common ancestor.
pub fn relative_path(cwd: Option<&str>, path: &str) -> String {
    let (Some(cwd), true) = (cwd, path.starts_with('/')) else {
        return path.to_owned();
    };
    if !cwd.starts_with('/') {
        return path.to_owned();
    }
    let from: Vec<&str> = cwd.split('/').filter(|part| !part.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let mut shared = 0;
    while shared < from.len() && from[shared] == to[shared] {
        shared += 1;
    }
    let mut parts: Vec<&str> = vec![".."; from.len() - shared];
    parts.extend_from_slice(&to[shared..]);
    let joined = parts.join("/");
    if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_inside_cwd_is_relative() {
        assert_eq!(
            relative_path(Some("/home/pi/project"), "/home/pi/project/src/main.rs"),
            "src/main.rs"
        );
    }

    #[test]
    fn path_outside_cwd_uses_dotdot() {
        assert_eq!(
            relative_path(Some("/home/pi/project"), "/home/pi/other/a.txt"),
            "../other/a.txt"
        );
    }

    #[test]
    fn identical_paths_return_dot() {
        assert_eq!(relative_path(Some("/a/b"), "/a/b"), ".");
    }

    #[test]
    fn relative_or_non_absolute_inputs_are_returned_unchanged() {
        assert_eq!(relative_path(Some("not/absolute"), "/a/b"), "/a/b");
        assert_eq!(
            relative_path(Some("/a/b"), "relative/file"),
            "relative/file"
        );
        assert_eq!(relative_path(None, "/a/b"), "/a/b");
    }
}
