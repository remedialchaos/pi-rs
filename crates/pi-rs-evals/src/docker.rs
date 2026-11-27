//! Docker image building, discovery, and task execution (port of `docker.ts`).
//!
//! All process execution goes through [`CommandRunner`] so offline tests can
//! record and assert the exact argument vector.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod image;

use crate::error::{EvalError, EvalResult};
use crate::plan::{DOCUMENTATION_VARIANTS, DocumentationVariant, EvalTask};
use crate::report::sha256_hex;

/// Default OCI container runtime binary name (`docker`).
pub const DEFAULT_CONTAINER_RUNTIME: &str = "docker";
/// Automatic fallback OCI container runtime binary name (`podman`).
pub const FALLBACK_CONTAINER_RUNTIME: &str = "podman";
/// Environment variable overriding the OCI container runtime binary.
pub const CONTAINER_RUNTIME_ENV: &str = "PI_EVALS_CONTAINER_BIN";

/// A built documentation-variant image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltImage {
    pub name: String,
    pub id: String,
}

/// The pair of images used by the comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltImages {
    #[serde(rename = "without_docs")]
    pub without_docs: BuiltImage,
    #[serde(rename = "with_docs")]
    pub with_docs: BuiltImage,
}

impl BuiltImages {
    /// Look up an image by documentation variant.
    pub fn get(&self, variant: DocumentationVariant) -> &BuiltImage {
        match variant {
            DocumentationVariant::WithoutDocs => &self.without_docs,
            DocumentationVariant::WithDocs => &self.with_docs,
        }
    }
}

/// Immutable configuration for docker invocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContext {
    pub images: BuiltImages,
    pub artifact_directory: PathBuf,
    pub auth_path: PathBuf,
    pub provider: String,
    pub model: String,
    pub runs_per_variant: u64,
}

/// Result of one command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
}

/// Injectable process runner matching `spawnSync` in upstream `docker.ts`.
pub trait CommandRunner {
    fn execute(&self, command: &str, args: &[String], capture: bool) -> EvalResult<CommandOutput>;
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn find_on_path(binary: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    for directory in std::env::split_paths(path_env) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe_candidate = directory.join(format!("{binary}.exe"));
            if is_executable_file(&exe_candidate) {
                return Some(exe_candidate);
            }
        }
    }
    None
}

/// Resolve the OCI container runtime executable name or path.
///
/// Precedence:
/// 1. Non-empty `PI_EVALS_CONTAINER_BIN` override (`env_override`), trimmed.
/// 2. `docker` when an executable `docker` binary is found on `PATH`.
/// 3. `podman` fallback when `docker` is absent and `podman` is found on `PATH`.
pub fn resolve_container_runtime(
    env_override: Option<&str>,
    path_env: Option<&OsStr>,
) -> EvalResult<String> {
    if let Some(override_bin) = env_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(override_bin.to_owned());
    }
    if find_on_path(DEFAULT_CONTAINER_RUNTIME, path_env).is_some() {
        return Ok(DEFAULT_CONTAINER_RUNTIME.to_owned());
    }
    if find_on_path(FALLBACK_CONTAINER_RUNTIME, path_env).is_some() {
        return Ok(FALLBACK_CONTAINER_RUNTIME.to_owned());
    }
    Err(EvalError::message(
        "No OCI container runtime found on PATH (checked docker, podman); set PI_EVALS_CONTAINER_BIN to override.",
    ))
}

/// Resolve the host OCI container runtime from `PI_EVALS_CONTAINER_BIN` and `PATH`.
pub fn resolve_host_container_runtime() -> EvalResult<String> {
    let env_override = std::env::var(CONTAINER_RUNTIME_ENV).ok();
    let path_env = std::env::var_os("PATH");
    resolve_container_runtime(env_override.as_deref(), path_env.as_deref())
}

/// Runs real processes from the eval package root.
#[derive(Debug, Clone)]
pub struct SystemCommandRunner {
    pub cwd: PathBuf,
    container_runtime: Option<String>,
}

impl SystemCommandRunner {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            container_runtime: None,
        }
    }

    /// Construct a runner with an explicit OCI container runtime binary or path.
    pub fn with_container_runtime(cwd: PathBuf, runtime: impl Into<String>) -> Self {
        Self {
            cwd,
            container_runtime: Some(runtime.into()),
        }
    }

    /// Configured container runtime override, if any.
    pub fn container_runtime(&self) -> Option<&str> {
        self.container_runtime.as_deref()
    }

    /// Resolve the executable used for `command`.
    ///
    /// Non-container commands pass through unchanged; `DEFAULT_CONTAINER_RUNTIME`
    /// resolves via `container_runtime` when set or `resolve_host_container_runtime`.
    pub fn resolve_command(&self, command: &str) -> EvalResult<String> {
        if command != DEFAULT_CONTAINER_RUNTIME {
            return Ok(command.to_owned());
        }
        if let Some(runtime) = self
            .container_runtime
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(runtime.to_owned());
        }
        resolve_host_container_runtime()
    }
}

impl CommandRunner for SystemCommandRunner {
    fn execute(&self, command: &str, args: &[String], capture: bool) -> EvalResult<CommandOutput> {
        let executable = self.resolve_command(command)?;
        if capture {
            let output = Command::new(&executable)
                .args(args)
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .stderr(Stdio::inherit())
                .output()?;
            Ok(CommandOutput {
                status: output.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            })
        } else {
            let status = Command::new(&executable)
                .args(args)
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?;
            Ok(CommandOutput {
                status: status.code().unwrap_or(1),
                stdout: String::new(),
            })
        }
    }
}

/// Resolve the host agent directory from `PI_CODING_AGENT_DIR` or a home path.
pub fn resolve_agent_dir(env_value: Option<&str>, home: Option<&Path>) -> PathBuf {
    match env_value {
        Some(value) if !value.is_empty() => lexical_resolve(Path::new(value)),
        _ => home
            .map(|home| home.join(".pi").join("agent"))
            .unwrap_or_else(|| PathBuf::from(".pi").join("agent")),
    }
}

/// Resolve `.` and `..` lexically without requiring the path to exist.
pub fn lexical_resolve(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    let mut resolved = base;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => resolved.push(part),
        }
    }
    resolved
}

/// Validate that `agent_dir/auth.json` exists and has a credential for `provider`.
pub fn require_eval_auth_file(provider: &str, agent_dir: &Path) -> EvalResult<PathBuf> {
    let path = agent_dir.join("auth.json");
    if !path.is_file() {
        return Err(EvalError::message(format!(
            "Eval authentication file does not exist: {}",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(&path).map_err(|_| {
        EvalError::message(format!(
            "Eval authentication file is invalid: {}",
            path.display()
        ))
    })?;
    let credentials: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
        EvalError::message(format!(
            "Eval authentication file is invalid: {}",
            path.display()
        ))
    })?;
    if !credentials.is_object() || credentials.get(provider).is_none() {
        return Err(EvalError::message(format!(
            "Eval authentication file has no credential for provider {provider}."
        )));
    }
    Ok(path)
}

fn environment(name: &str, value: &str) -> [String; 2] {
    ["--env".to_owned(), format!("{name}={value}")]
}

/// Read the real process identity using the POSIX id utility on Unix.
pub fn process_uid_gid() -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        fn real_id(flag: &str) -> Option<u32> {
            let output = Command::new("id").args(["-r", flag]).output().ok()?;
            if !output.status.success() {
                return None;
            }
            std::str::from_utf8(&output.stdout)
                .ok()?
                .trim()
                .parse()
                .ok()
        }
        Some((real_id("-u")?, real_id("-g")?))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
fn create_output_directory(path: &Path) -> EvalResult<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_output_directory(path: &Path) -> EvalResult<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Build the exact `docker run` argument vector for one task.
///
/// `artifact_uid_gid` mirrors `process.getuid()`/`getgid()` on POSIX; tests pass
/// fixed values to keep the vector deterministic.
///
/// Verified compatible with rootless `podman` 4.9.3 (`crun`): `--read-only`,
/// `--tmpfs`, and `--mount type=bind,...,readonly` behave identically. Under
/// rootless user namespaces, container UID `0` maps to the host invoking UID
/// (so artifacts written by UID `0` are host-user-owned without `chown` to
/// `PI_EVAL_ARTIFACT_UID`), while `PI_EVAL_SANDBOX_UID=65532` requires a
/// subordinate UID mapping range of at least 65532 IDs.
pub fn docker_args(
    context: &DockerContext,
    variant: DocumentationVariant,
    output_directory: &Path,
    entrypoint_args: &[String],
    artifact_uid_gid: Option<(u32, u32)>,
) -> EvalResult<Vec<String>> {
    create_output_directory(output_directory)?;
    let mut args = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "--read-only".to_owned(),
        "--tmpfs".to_owned(),
        "/tmp:rw,exec,mode=1777".to_owned(),
        "--mount".to_owned(),
        format!(
            "type=bind,source={},target=/artifacts",
            output_directory.display()
        ),
    ];
    args.extend(environment("PI_EVAL_ARTIFACT_DIR", "/artifacts"));
    args.extend(environment(
        "PI_EVAL_RUNS_PER_VARIANT",
        &context.runs_per_variant.to_string(),
    ));
    args.extend(environment("PI_EVAL_SANDBOX_UID", "65532"));
    args.extend(environment("PI_EVAL_SANDBOX_GID", "65532"));
    args.extend(environment("PI_PROVIDER", &context.provider));
    args.extend(environment("PI_MODEL", &context.model));
    if let Some((uid, gid)) = artifact_uid_gid {
        args.extend(environment("PI_EVAL_ARTIFACT_UID", &uid.to_string()));
        args.extend(environment("PI_EVAL_ARTIFACT_GID", &gid.to_string()));
    }
    args.push("--mount".to_owned());
    args.push(format!(
        "type=bind,source={},target=/run/pi-eval-secrets/auth.json,readonly",
        context.auth_path.display()
    ));
    let models = context.auth_path.with_file_name("models.json");
    if models.try_exists()? {
        args.push("--mount".to_owned());
        args.push(format!(
            "type=bind,source={},target=/run/pi-eval-secrets/models.json,readonly",
            models.display()
        ));
    }
    args.push(context.images.get(variant).name.clone());
    args.extend(entrypoint_args.iter().cloned());
    Ok(args)
}

/// Assemble an immutable docker context.
pub fn create_docker_context(
    images: BuiltImages,
    artifact_directory: PathBuf,
    auth_path: PathBuf,
    provider: &str,
    model: &str,
    runs_per_variant: u64,
) -> DockerContext {
    DockerContext {
        images,
        artifact_directory,
        auth_path,
        provider: provider.to_owned(),
        model: model.to_owned(),
        runs_per_variant,
    }
}

/// Build both documentation-variant images and inspect their IDs.
pub fn build_images(
    repository_root: &Path,
    package_root: &Path,
    runner: &dyn CommandRunner,
) -> EvalResult<BuiltImages> {
    #[cfg(target_os = "linux")]
    {
        let stage = image::stage(repository_root)?;
        build_images_from_context(
            repository_root,
            &package_root.join("docker/Dockerfile"),
            stage.path(),
            runner,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        build_images_from_context(
            repository_root,
            &package_root.join("docker/Dockerfile.build"),
            repository_root,
            runner,
        )
    }
}

/// Tag and inspect both images using an already prepared OCI build context.
pub fn build_images_from_context(
    repository_root: &Path,
    dockerfile: &Path,
    context: &Path,
    runner: &dyn CommandRunner,
) -> EvalResult<BuiltImages> {
    let prefix = format!(
        "pi-evals-{}",
        sha256_hex(repository_root.to_string_lossy().as_bytes())
            .chars()
            .take(12)
            .collect::<String>()
    );
    let mut images = BuiltImages {
        without_docs: BuiltImage {
            name: format!("{prefix}-without-docs:local"),
            id: String::new(),
        },
        with_docs: BuiltImage {
            name: format!("{prefix}-with-docs:local"),
            id: String::new(),
        },
    };
    for variant in DOCUMENTATION_VARIANTS {
        let image = match variant {
            DocumentationVariant::WithoutDocs => &mut images.without_docs,
            DocumentationVariant::WithDocs => &mut images.with_docs,
        };
        let build_args = vec![
            "build".to_owned(),
            "--target".to_owned(),
            variant.as_str().to_owned(),
            "--tag".to_owned(),
            image.name.clone(),
            "--file".to_owned(),
            dockerfile.to_string_lossy().into_owned(),
            context.to_string_lossy().into_owned(),
        ];
        let built = runner.execute(DEFAULT_CONTAINER_RUNTIME, &build_args, false)?;
        if built.status != 0 {
            return Err(EvalError::message(format!(
                "docker exited with status {}.",
                built.status
            )));
        }
        let inspect_args = vec![
            "image".to_owned(),
            "inspect".to_owned(),
            "--format".to_owned(),
            "{{.Id}}".to_owned(),
            image.name.clone(),
        ];
        let inspected = runner.execute(DEFAULT_CONTAINER_RUNTIME, &inspect_args, true)?;
        if inspected.status != 0 || inspected.stdout.trim().is_empty() {
            return Err(EvalError::message(format!(
                "Cannot inspect {}.",
                image.name
            )));
        }
        image.id = inspected.stdout.trim().to_owned();
    }
    Ok(images)
}

/// Run discovery inside one variant image and return the discovered JSON path.
pub fn discover_cases(
    context: &DockerContext,
    variant: DocumentationVariant,
    files: &[String],
    selection_args: &[String],
    runner: &dyn CommandRunner,
    artifact_uid_gid: Option<(u32, u32)>,
) -> EvalResult<PathBuf> {
    let output_directory = context
        .artifact_directory
        .join("discovery")
        .join(variant.as_str());
    let mut entrypoint_args = vec!["__discover".to_owned()];
    entrypoint_args.extend(files.iter().cloned());
    entrypoint_args.extend(selection_args.iter().cloned());
    let args = docker_args(
        context,
        variant,
        &output_directory,
        &entrypoint_args,
        artifact_uid_gid,
    )?;
    let result = runner.execute(DEFAULT_CONTAINER_RUNTIME, &args, true)?;
    if result.status != 0 {
        return Err(EvalError::message(format!(
            "{variant} eval discovery failed."
        )));
    }
    let path = output_directory.join("discovered-tests.json");
    std::fs::write(&path, result.stdout)?;
    Ok(path)
}

/// SHA-256 directory name for a task identity.
pub fn task_directory_name(task: &EvalTask) -> String {
    sha256_hex(task.identity_json().as_bytes())
}

/// Run one native task. A container failure has no observation.
pub fn run_task(
    context: &DockerContext,
    task: &EvalTask,
    runner: &dyn CommandRunner,
    artifact_uid_gid: Option<(u32, u32)>,
) -> EvalResult<Option<PathBuf>> {
    let output_directory = context
        .artifact_directory
        .join("tasks")
        .join(task_directory_name(task));
    let entrypoint_args = vec!["__run".to_owned(), serde_json::to_string(task)?];
    let args = docker_args(
        context,
        task.variant,
        &output_directory,
        &entrypoint_args,
        artifact_uid_gid,
    )?;
    let session = output_directory.join("session.jsonl");
    // A retry must never pick up a previous invocation's snapshot.
    if let Err(error) = std::fs::remove_file(&session)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    let result = runner.execute(DEFAULT_CONTAINER_RUNTIME, &args, true)?;
    if session.try_exists()? {
        let snapshot = crate::report::session_snapshot_path(&context.artifact_directory, task);
        let parent = snapshot
            .parent()
            .ok_or_else(|| EvalError::message("Invalid session path"))?;
        create_output_directory(parent)?;
        std::fs::rename(session, snapshot)?;
    }
    if result.status != 0 {
        return Ok(None);
    }
    let path = output_directory.join("observation.json");
    std::fs::write(&path, result.stdout)?;
    Ok(Some(path))
}

/// Publish a private snapshot owned by the operator who owns the artifact mount.
/// The mount metadata reflects the container's UID mapping under rootless engines.
pub fn write_container_session(directory: &Path, text: &str) -> EvalResult<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let path = directory.join("session.jsonl");
    let mut file = options.open(&path)?;
    file.write_all(text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, chown};
        let owner = directory.metadata()?;
        let current = file.metadata()?;
        if (current.uid(), current.gid()) != (owner.uid(), owner.gid()) {
            chown(&path, Some(owner.uid()), Some(owner.gid()))?;
        }
    }
    Ok(())
}
