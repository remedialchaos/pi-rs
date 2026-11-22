//! Cleanup for native add-ons quarantined by the npm-distributed Windows Pi.

use std::path::{Path, PathBuf};

const QUARANTINE_DIR: &str = ".pi-native-quarantine";

fn quarantine_root(package_dir: &Path) -> Option<PathBuf> {
    package_dir
        .ancestors()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("node_modules"))
        })
        .map(|path| path.join(QUARANTINE_DIR))
}

pub(super) fn cleanup(package_dir: &Path) {
    if let Some(root) = quarantine_root(package_dir) {
        // Another Pi process may still hold a quarantined DLL. Try again at the
        // next startup instead of preventing this process from starting.
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Rust resolves `.exe` automatically but not the npm/pnpm `.cmd` shims.
/// Passing a found shim directly preserves Rust's Windows batch argument escaping.
#[cfg(windows)]
pub(super) fn resolve_command(program: &str) -> std::ffi::OsString {
    let path = Path::new(program);
    if path.extension().is_some() {
        return program.into();
    }
    let candidate = |path: &Path| -> Option<PathBuf> {
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .map(|extension| path.with_extension(extension))
            .find(|candidate| candidate.is_file())
    };
    let found = if path.components().count() > 1 {
        candidate(path)
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| candidate(&dir.join(path)))
        })
    };
    found.map_or_else(|| program.into(), PathBuf::into_os_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_nearest_node_modules_quarantine() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let outer = root.path().join("node_modules");
        let modules = outer.join("wrapper").join("Node_Modules");
        let package = modules.join("@scope").join("package");
        let quarantine = modules.join(QUARANTINE_DIR);
        let other = outer.join(QUARANTINE_DIR);
        std::fs::create_dir_all(&package)?;
        std::fs::create_dir_all(&quarantine)?;
        std::fs::create_dir_all(&other)?;
        std::fs::write(quarantine.join("addon.node"), b"old")?;
        cleanup(&package);
        assert!(!quarantine.exists());
        assert!(package.is_dir());
        assert!(other.is_dir());
        assert_eq!(quarantine_root(root.path()), None);
        Ok(())
    }
}
