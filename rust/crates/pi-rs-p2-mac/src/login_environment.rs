use crate::server_config::{Environment, home};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const MARKER: &str = "__P2MAC_ENV__";

pub fn login_shell(env: &Environment) -> PathBuf {
    #[cfg(unix)]
    if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::getuid()) {
        return user.shell;
    }
    env.get("SHELL").map_or_else(
        || {
            PathBuf::from(if cfg!(target_os = "macos") {
                "/bin/zsh"
            } else {
                "/bin/sh"
            })
        },
        PathBuf::from,
    )
}

pub async fn capture(shell: &Path, inherited: &Environment) -> Environment {
    let mut child = tokio::process::Command::new(shell);
    child
        .args([
            "-l",
            "-i",
            "-c",
            "printf __P2MAC_ENV__; /usr/bin/env -0; printf __P2MAC_ENV__",
        ])
        .env_clear()
        .envs(inherited)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(8), child.output()).await {
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some((_, payload)) = text.split_once(MARKER)
            && let Some((payload, _)) = payload.split_once(MARKER)
        {
            let mut env: Environment = payload
                .split('\0')
                .filter_map(|entry| entry.split_once('='))
                .map(|(k, v)| (k.into(), v.into()))
                .collect();
            for key in ["SHLVL", "PWD", "OLDPWD", "_"] {
                env.remove(key);
            }
            if env.contains_key("PATH") {
                return env;
            }
        }
    }
    let mut env = inherited.clone();
    let default = env
        .get("PATH")
        .map_or("/usr/bin:/bin:/usr/sbin:/sbin", String::as_str);
    let paths = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home(&env).join(".local/bin"),
    ]
    .into_iter()
    .chain(std::env::split_paths(default));
    if let Ok(path) = std::env::join_paths(paths) {
        env.insert("PATH".into(), path.to_string_lossy().into_owned());
    }
    env
}

pub(crate) fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

pub fn which(name: &str, path: Option<&str>) -> Option<PathBuf> {
    std::env::split_paths(path.unwrap_or_default())
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join(name))
        .find(|candidate| executable(candidate))
}
