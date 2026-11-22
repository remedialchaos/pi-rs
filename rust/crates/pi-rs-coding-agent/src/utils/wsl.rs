//! WSL detection and path translation, ported from `utils/wsl.ts`.

use std::fs;

/// True when `WSL_DISTRO_NAME` or `WSLENV` is set, or when `/proc/version` mentions Microsoft/WSL.
pub fn is_wsl() -> bool {
    is_wsl_with(|key| std::env::var(key).ok())
}

/// Environment-injectable variant for tests and for callers with a synthetic environment.
pub fn is_wsl_with<F>(env: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    if env("WSL_DISTRO_NAME").is_some() || env("WSLENV").is_some() {
        return true;
    }
    match fs::read_to_string("/proc/version") {
        Ok(release) => proc_version_is_wsl(&release),
        Err(_) => false,
    }
}

/// Pure predicate over the contents of `/proc/version`.
pub fn proc_version_is_wsl(release: &str) -> bool {
    let lower = release.to_lowercase();
    lower.contains("microsoft") || lower.contains("wsl")
}

/// Translate a WSL `/mnt/<drive>/...` path to a Windows path (`C:\...`).
/// Paths outside `/mnt/<drive>` are left unchanged.
pub fn wsl_to_windows_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/mnt/")?;
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next()?;
    if drive.len() != 1 || !drive.bytes().next()?.is_ascii_alphabetic() {
        return None;
    }
    let suffix = parts.next().unwrap_or("");
    let windows = suffix.replace('/', "\\");
    if windows.is_empty() {
        Some(format!("{}:\\", drive.to_ascii_uppercase()))
    } else {
        Some(format!("{}:\\{windows}", drive.to_ascii_uppercase()))
    }
}

/// Translate a Windows drive path (`C:\foo\bar`) to its WSL `/mnt/c/foo/bar` form.
pub fn windows_to_wsl_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return None;
    }
    if bytes[2] != b'\\' && bytes[2] != b'/' {
        return None;
    }
    let suffix = path[3..].replace('\\', "/");
    let drive = bytes[0].to_ascii_lowercase() as char;
    if suffix.is_empty() {
        Some(format!("/mnt/{drive}"))
    } else {
        Some(format!("/mnt/{drive}/{suffix}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_markers_win_without_reading_proc() {
        let env = |key: &str| match key {
            "WSL_DISTRO_NAME" => Some("Ubuntu".to_string()),
            _ => None,
        };
        assert!(is_wsl_with(env));
        let wslenv = |key: &str| match key {
            "WSLENV" => Some(String::new()),
            _ => None,
        };
        assert!(is_wsl_with(wslenv));
        let none = |_: &str| None;
        // The host may itself be WSL; only assert the predicate is total here.
        let _ = is_wsl_with(none);
    }

    #[test]
    fn proc_version_predicate_is_case_insensitive() {
        assert!(proc_version_is_wsl(
            "Linux version 5.15.0-Microsoft-standard-WSL2"
        ));
        assert!(proc_version_is_wsl("microsoft wsl"));
        assert!(!proc_version_is_wsl("Linux version 6.1.0-generic"));
    }

    #[test]
    fn wsl_to_windows_path_maps_drives() {
        assert_eq!(
            wsl_to_windows_path("/mnt/c/Users/pi/file.txt").as_deref(),
            Some("C:\\Users\\pi\\file.txt")
        );
        assert_eq!(wsl_to_windows_path("/mnt/d").as_deref(), Some("D:\\"));
        assert_eq!(wsl_to_windows_path("/home/pi"), None);
        assert_eq!(wsl_to_windows_path("/mnt/cc/x"), None);
    }

    #[test]
    fn windows_to_wsl_path_maps_drives() {
        assert_eq!(
            windows_to_wsl_path("C:\\Users\\pi\\file.txt").as_deref(),
            Some("/mnt/c/Users/pi/file.txt")
        );
        assert_eq!(windows_to_wsl_path("D:/x").as_deref(), Some("/mnt/d/x"));
        assert_eq!(windows_to_wsl_path("relative\\path"), None);
        assert_eq!(windows_to_wsl_path("CC:\\x"), None);
    }
}
