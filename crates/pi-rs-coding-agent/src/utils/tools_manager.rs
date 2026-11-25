//! Local discovery of helper binaries (`fd`, `rg`), ported from `utils/tools-manager.ts`.
//!
//! Upstream downloads releases from GitHub when a tool is missing. This module deliberately does
//! not: there is no network access in the port's test suite and the front door installs binaries
//! through the normal build/deploy procedure. Missing tools are reported with upstream's messages.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Fd,
    Rg,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fd => "fd",
            Self::Rg => "rg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolConfig {
    pub name: &'static str,
    pub repo: &'static str,
    pub binary_name: &'static str,
    pub system_binary_names: &'static [&'static str],
    pub tag_prefix: &'static str,
}

pub const TOOLS: [ToolConfig; 2] = [
    ToolConfig {
        name: "fd",
        repo: "sharkdp/fd",
        binary_name: "fd",
        system_binary_names: &["fd", "fdfind"],
        tag_prefix: "v",
    },
    ToolConfig {
        name: "rg",
        repo: "BurntSushi/ripgrep",
        binary_name: "rg",
        system_binary_names: &["rg"],
        tag_prefix: "",
    },
];

pub fn tool_config(tool: Tool) -> &'static ToolConfig {
    match tool {
        Tool::Fd => &TOOLS[0],
        Tool::Rg => &TOOLS[1],
    }
}

/// `PI_OFFLINE` in {1, true, yes} (case-insensitive) disables downloads upstream.
pub fn is_offline_mode(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_lowercase).as_deref(),
        Some("1" | "true" | "yes")
    )
}

/// Release target triple suffix, e.g. `aarch64-unknown-linux-musl`.
pub fn get_release_target(platform: &str, arch: &str) -> Result<String, String> {
    let arch = match arch {
        "arm64" | "aarch64" => "aarch64",
        "x64" | "x86_64" | "amd64" => "x86_64",
        _ => {
            return Err(format!("Unsupported platform: {platform}/{arch}"));
        }
    };
    let os = match platform {
        "darwin" | "macos" => "apple-darwin",
        "linux" => "unknown-linux-musl",
        "win32" | "windows" => "pc-windows-msvc",
        _ => return Err(format!("Unsupported platform: {platform}/{arch}")),
    };
    let extension = if os == "pc-windows-msvc" {
        ".zip"
    } else {
        ".tar.gz"
    };
    Ok(format!("{arch}-{os}{extension}"))
}

pub fn asset_name(tool: Tool, version: &str, target: &str) -> String {
    let config = tool_config(tool);
    match tool {
        Tool::Fd => format!("{}-{}{version}-{target}", config.name, config.tag_prefix),
        Tool::Rg => format!("ripgrep-{version}-{target}"),
    }
}

#[derive(Debug, Clone)]
pub struct ToolManager {
    pub bin_dir: PathBuf,
    pub offline: bool,
}

impl ToolManager {
    pub fn new(bin_dir: PathBuf) -> Self {
        let offline = is_offline_mode(std::env::var("PI_OFFLINE").ok().as_deref());
        Self { bin_dir, offline }
    }

    /// Look in the agent bin dir first, then on PATH using only filesystem checks (no spawning).
    pub fn get_tool_path(&self, tool: Tool) -> Option<PathBuf> {
        let config = tool_config(tool);
        let local_name = if cfg!(windows) {
            format!("{}.exe", config.binary_name)
        } else {
            config.binary_name.to_string()
        };
        let local = self.bin_dir.join(local_name);
        if local.is_file() {
            return Some(local);
        }
        for name in config.system_binary_names {
            if let Some(path) = find_on_path(name) {
                return Some(path);
            }
        }
        None
    }

    /// Local-only equivalent of upstream `ensureTool`: never downloads.
    pub fn ensure_tool_local(&self, tool: Tool) -> Result<PathBuf, String> {
        if let Some(path) = self.get_tool_path(tool) {
            return Ok(path);
        }
        if self.offline {
            return Err(offline_message(tool));
        }
        Err(format!("{} not found", tool_config(tool).name))
    }
}

pub fn offline_message(tool: Tool) -> String {
    format!(
        "{} not found. Offline mode enabled, skipping download.",
        tool_config(tool).name
    )
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn offline_mode_values() {
        assert!(is_offline_mode(Some("1")));
        assert!(is_offline_mode(Some(" TRUE ")));
        assert!(is_offline_mode(Some("YeS")));
        assert!(!is_offline_mode(Some("0")));
        assert!(!is_offline_mode(Some("maybe")));
        assert!(!is_offline_mode(None));
    }

    #[test]
    fn release_target_matrix() {
        assert_eq!(
            get_release_target("linux", "x64").as_deref(),
            Ok("x86_64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(
            get_release_target("darwin", "arm64").as_deref(),
            Ok("aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(
            get_release_target("win32", "x64").as_deref(),
            Ok("x86_64-pc-windows-msvc.zip")
        );
        assert_eq!(
            get_release_target("linux", "ia32").err().as_deref(),
            Some("Unsupported platform: linux/ia32")
        );
    }

    #[test]
    fn asset_names_match_upstream() -> Result<(), String> {
        let target = get_release_target("linux", "x64")?;
        assert_eq!(
            asset_name(Tool::Fd, "10.3.0", &target),
            "fd-v10.3.0-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            asset_name(Tool::Rg, "14.1.0", &target),
            "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn discovers_local_binary_in_bin_dir() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let name = if cfg!(windows) { "fd.exe" } else { "fd" };
        let binary = dir.path().join(name);
        fs::write(&binary, b"").map_err(|e| e.to_string())?;
        let manager = ToolManager::new(dir.path().to_path_buf());
        assert_eq!(manager.get_tool_path(Tool::Fd), Some(binary));
        Ok(())
    }

    #[test]
    fn missing_binary_reports_offline_message() {
        assert_eq!(
            offline_message(Tool::Rg),
            "rg not found. Offline mode enabled, skipping download."
        );
    }
}
