//! Where p3's own files are, and how its processes start each other.
//!
//! Upstream resolves a TypeScript `src/` or a built `dist/`; this port is one compiled binary, so the
//! command, worker, and supervisor processes are that binary with a hidden first argument.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// p3's version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The hidden first argument of a session worker process.
pub const WORKER_ARG: &str = "__worker";

/// The hidden first argument of the background server's supervisor.
pub const SUPERVISOR_ARG: &str = "__supervisor";

/// The environment a supervised server runs with, so it knows not to print an owner link into a log.
pub const SUPERVISED_ENV: &str = "P3_SUPERVISED";

/// How a session's worker process starts: the executable and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

impl WorkerSpec {
    /// The worker of this install: this binary, in its worker mode.
    pub fn current() -> Result<Self, std::io::Error> {
        Ok(Self {
            executable: std::env::current_exe()?,
            args: vec![WORKER_ARG.to_string()],
        })
    }

    /// The worker of the binary at `executable`.
    pub fn at(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: vec![WORKER_ARG.to_string()],
        }
    }
}

/// The built-in plugins' root inside a data directory's layout: plugins are compiled in (ADR-002), so
/// the directory exists for a user's own plugins and for layout parity, but nothing is built there.
pub fn plugins_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

/// The arguments of a supervisor that runs `server_args` on `data_dir`.
pub fn supervisor_args(data_dir: &Path, server_args: &[OsString]) -> Vec<OsString> {
    let mut args = vec![
        OsString::from(SUPERVISOR_ARG),
        data_dir.as_os_str().to_owned(),
    ];
    args.extend(server_args.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_of_this_install_is_this_binary_in_worker_mode() {
        let worker = WorkerSpec::current().expect("current executable");
        assert_eq!(worker.args, [WORKER_ARG]);
        assert_eq!(
            WorkerSpec::at("/opt/p3").executable,
            PathBuf::from("/opt/p3")
        );
    }

    #[test]
    fn supervisor_arguments_carry_the_data_directory_then_the_server_command() {
        let args = supervisor_args(
            Path::new("/tmp/pi"),
            &[OsString::from("server"), OsString::from("--dir")],
        );
        assert_eq!(
            args,
            [
                OsString::from(SUPERVISOR_ARG),
                OsString::from("/tmp/pi"),
                OsString::from("server"),
                OsString::from("--dir")
            ]
        );
    }
}
