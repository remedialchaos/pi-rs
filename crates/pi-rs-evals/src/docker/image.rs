//! Prepare a minimal Linux runtime from the installed native binaries. This
//! keeps offline runs independent of base image registries and Cargo caches.
use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn install(source: &Path, target: &Path) -> EvalResult<()> {
    let parent = target
        .parent()
        .ok_or_else(|| EvalError::message("Missing install directory"))?;
    fs::create_dir_all(parent)?;
    fs::copy(source, target)
        .map_err(|e| EvalError::message(format!("Install {}: {e}", source.display())))?;
    Ok(())
}

fn install_binary(source: &Path, root: &Path, name: &str, strip: bool) -> EvalResult<()> {
    let destination = root.join(name);
    if strip {
        let parent = destination
            .parent()
            .ok_or_else(|| EvalError::message("Missing binary directory"))?;
        fs::create_dir_all(parent)?;
        let status = Command::new("strip")
            .arg("--strip-unneeded")
            .arg("-o")
            .arg(&destination)
            .arg(source)
            .status()?;
        if !status.success() {
            return Err(EvalError::message(format!(
                "Cannot strip native binary {}",
                source.display()
            )));
        }
    } else {
        install(source, &destination)?;
    }
    // Only inspect installed (operator-selected) programs, never evaluation outputs.
    let libs = Command::new("ldd")
        .env("LC_ALL", "C")
        .arg(source)
        .output()?;
    let stdout = String::from_utf8_lossy(&libs.stdout);
    if !libs.status.success() {
        let stderr = String::from_utf8_lossy(&libs.stderr);
        if stdout.contains("not a dynamic executable")
            || stderr.contains("not a dynamic executable")
        {
            return Ok(());
        }
        return Err(EvalError::message(format!(
            "Cannot resolve libraries for {}: {stdout}{stderr}",
            source.display()
        )));
    }
    for line in stdout.lines() {
        if line.contains("=> not found") {
            return Err(EvalError::message(format!(
                "{}: {}",
                source.display(),
                line.trim()
            )));
        }
        // ldd prints either “soname => /path (address)” or “/loader (address)”.
        let name = line
            .trim()
            .split_once(" => ")
            .map_or(line.trim(), |(_, path)| path);
        let name = name.split(" (").next().unwrap_or(name);
        if let Some(relative) = name.strip_prefix('/') {
            let target = root.join(relative);
            if !target.exists() {
                install(Path::new(name), &target)?;
            }
        }
    }
    Ok(())
}

pub(super) fn stage(repository: &Path) -> EvalResult<tempfile::TempDir> {
    let stage = tempfile::Builder::new()
        .prefix("pi-eval-image-")
        .tempdir()?;
    let root = stage.path().join("runtime");
    let docs = stage.path().join("documentation");
    let exe = std::env::current_exe()?;
    let bin = exe
        .parent()
        .ok_or_else(|| EvalError::message("Cannot locate pi-evals executable"))?;
    let bin = if bin.ends_with("deps") {
        bin.parent().unwrap_or(bin)
    } else {
        bin
    };
    let evaluator = std::env::var_os("PI_EVALS_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if bin == exe.parent().unwrap_or(bin) {
                exe.clone()
            } else {
                bin.join("pi-evals")
            }
        });
    let pi = std::env::var_os("PI_EVAL_PI_BINARY")
        .map(PathBuf::from)
        .or_else(|| bin.join("pi").is_file().then(|| bin.join("pi")))
        .or_else(|| find_on_path("pi", std::env::var_os("PATH").as_deref()))
        .ok_or_else(|| EvalError::message("Missing native pi executable; set PI_EVAL_PI_BINARY"))?;
    install_binary(&pi, &root, "usr/local/bin/pi", true)?;
    install_binary(&evaluator, &root, "usr/local/bin/pi-evals", true)?;
    fs::set_permissions(
        root.join("usr/local/bin/pi-evals"),
        fs::Permissions::from_mode(0o700),
    )?;
    for tool in [
        "sh", "bash", "env", "script", "cat", "ls", "grep", "find", "sed", "head", "tail", "mkdir",
        "cp", "stty",
    ] {
        let source = [Path::new("/usr/bin"), Path::new("/bin")]
            .iter()
            .map(|dir| dir.join(tool))
            .find(|p| p.is_file())
            .ok_or_else(|| EvalError::message(format!("Missing native runtime utility: {tool}")))?;
        install_binary(&source, &root, &format!("bin/{tool}"), false)?;
    }
    for path in ["tmp", "artifacts", "opt/pi", "run/pi-eval-secrets", "etc"] {
        fs::create_dir_all(root.join(path))?;
    }
    fs::set_permissions(
        root.join("run/pi-eval-secrets"),
        fs::Permissions::from_mode(0o700),
    )?;
    fs::write(
        root.join("etc/passwd"),
        "root:x:0:0:root:/root:/bin/sh\nsandbox:x:65532:65532:Sandbox:/tmp:/bin/sh\n",
    )?;
    fs::write(root.join("etc/group"), "root:x:0:\nsandbox:x:65532:\n")?;
    let ca = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ]
    .into_iter()
    .map(Path::new)
    .find(|p| p.is_file())
    .ok_or_else(|| EvalError::message("Missing host CA trust bundle"))?;
    install(ca, &root.join("etc/ssl/certs/ca-certificates.crt"))?;
    let agent = repository.join("crates/pi-rs-coding-agent");
    install(
        &agent.join("wit/extension.wit"),
        &root.join("opt/pi/wit/extension.wit"),
    )?;
    for theme in ["light.json", "dark.json", "theme.schema.json"] {
        install(
            &agent.join("assets/theme").join(theme),
            &root.join("opt/pi/theme").join(theme),
        )?;
    }
    install(&repository.join("README.md"), &docs.join("README.md"))?;
    install(
        &agent.join("assets/CHANGELOG.md"),
        &docs.join("CHANGELOG.md"),
    )?;
    for entry in fs::read_dir(agent.join("assets/docs"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            install(&entry.path(), &docs.join("docs").join(entry.file_name()))?;
        }
    }
    Ok(stage)
}
