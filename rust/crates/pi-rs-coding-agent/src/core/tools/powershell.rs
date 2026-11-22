//! PowerShell tool ported from `core/tools/powershell.ts`.
//!
//! Thin wrapper around the shared shell executor: same truncation and exit
//! reporting as bash, with PowerShell-specific args and exit-code suffix.

use std::path::Path;

use super::bash::{BashDetails, BashInput, BashOutput, BashResult, execute_shell};

pub type PowerShellInput = BashInput;
pub type PowerShellDetails = BashDetails;
pub type PowerShellOutput = BashOutput;
pub type PowerShellResult = BashResult;

pub const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

pub const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// Windows PowerShell 5.1 turns redirected native stderr into error records;
/// keep `-Command`'s 0/1 exit rule while exempting native-command errors.
pub const WINDOWS_POWERSHELL_EXIT_SUFFIX: &str = "\n;\nif ($?) { exit 0 }\nif (($LASTEXITCODE -eq 0) -and (\"$($Error[0].FullyQualifiedErrorId)\" -like \"NativeCommandError*\")) { exit 0 }\nexit 1";

fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_powershell() -> String {
    find_on_path("pwsh.exe")
        .or_else(|| find_on_path("powershell.exe"))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "powershell.exe".to_string())
}

/// Execute a PowerShell command. Only available on Windows, as upstream.
pub async fn execute_powershell(
    input: &PowerShellInput,
    cwd: &Path,
) -> Result<PowerShellResult, String> {
    if !cfg!(windows) {
        return Err("The powershell tool is only available on Windows.".to_string());
    }

    let shell = resolve_powershell();
    let is_windows_powershell = shell
        .rsplit(['/', '\\'])
        .next()
        .map(|name| name.eq_ignore_ascii_case("powershell.exe"))
        .unwrap_or(false);
    let suffix = if is_windows_powershell && !input.command.ends_with('`') {
        WINDOWS_POWERSHELL_EXIT_SUFFIX
    } else {
        ""
    };
    let command = format!("{UTF8_OUTPUT_PREFIX}{}{suffix}", input.command);

    let wrapped = PowerShellInput {
        command,
        timeout: input.timeout,
    };
    execute_shell(
        &wrapped,
        cwd,
        &shell,
        &POWERSHELL_ARGS,
        "PowerShell",
        "pi-powershell",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_args_match_upstream() {
        assert_eq!(
            POWERSHELL_ARGS,
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command"
            ]
        );
    }

    #[test]
    fn utf8_prefix_and_suffix_exact() {
        assert_eq!(
            UTF8_OUTPUT_PREFIX,
            "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n"
        );
        assert!(WINDOWS_POWERSHELL_EXIT_SUFFIX.contains("if ($?) { exit 0 }"));
        assert!(WINDOWS_POWERSHELL_EXIT_SUFFIX.ends_with("exit 1"));
    }

    #[tokio::test]
    async fn unavailable_off_windows() {
        let input = PowerShellInput {
            command: "echo hi".into(),
            timeout: None,
        };
        let result = execute_powershell(&input, Path::new("/")).await;
        if cfg!(windows) {
            assert!(result.is_ok());
        } else {
            let error = match result {
                Ok(_) => return,
                Err(error) => error,
            };
            assert_eq!(error, "The powershell tool is only available on Windows.");
        }
    }
}
