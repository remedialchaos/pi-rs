//! Docker runner: argument construction, auth validation, SHA-256 task identity,
//! and injectable command execution.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pi_rs_evals::docker::{
    BuiltImage, BuiltImages, CommandOutput, CommandRunner, DockerContext, SystemCommandRunner,
    build_images, create_docker_context, docker_args, escape_regex, process_uid_gid,
    require_eval_auth_file, resolve_container_runtime, run_task, task_directory_name,
};
use pi_rs_evals::plan::{DocumentationVariant, EvalTask};
use serde_json::json;

#[derive(Default)]
struct RecordingRunner {
    calls: Mutex<Vec<(String, Vec<String>, bool)>>,
    outputs: Mutex<Vec<CommandOutput>>,
}

impl RecordingRunner {
    fn with_outputs(outputs: Vec<CommandOutput>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outputs: Mutex::new(outputs),
        }
    }

    fn calls(&self) -> Vec<(String, Vec<String>, bool)> {
        self.calls.lock().expect("lock").clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn execute(
        &self,
        command: &str,
        args: &[String],
        capture: bool,
    ) -> pi_rs_evals::error::EvalResult<CommandOutput> {
        self.calls
            .lock()
            .expect("lock")
            .push((command.to_owned(), args.to_vec(), capture));
        let mut outputs = self.outputs.lock().expect("lock");
        if !outputs.is_empty() {
            Ok(outputs.remove(0))
        } else {
            Ok(CommandOutput {
                status: 0,
                stdout: "image-id\n".to_owned(),
            })
        }
    }
}

fn images() -> BuiltImages {
    BuiltImages {
        without_docs: BuiltImage {
            name: "pi-evals-abc-without-docs:local".to_owned(),
            id: "id-without".to_owned(),
        },
        with_docs: BuiltImage {
            name: "pi-evals-abc-with-docs:local".to_owned(),
            id: "id-with".to_owned(),
        },
    }
}

fn context(root: &Path) -> DockerContext {
    create_docker_context(
        images(),
        root.join("artifacts"),
        PathBuf::from("/run/pi-eval-secrets/auth.json"),
        "fixture",
        "fixture/model",
        2,
    )
}

fn task() -> EvalTask {
    EvalTask {
        file: "evals/models.docs.eval.ts".to_owned(),
        full_name: "tool access > create".to_owned(),
        eval_set: "tool access".to_owned(),
        case_id: "create".to_owned(),
        variant: DocumentationVariant::WithoutDocs,
        model: "fixture/model".to_owned(),
        run_number: 1,
    }
}

#[test]
fn requires_a_present_valid_auth_file_with_provider_credential() {
    let dir = tempfile::tempdir().expect("tempdir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("create agent dir");
    let auth = agent.join("auth.json");
    let error = require_eval_auth_file("fixture", &agent).expect_err("missing auth");
    assert!(
        error
            .to_string()
            .contains("Eval authentication file does not exist:")
    );

    std::fs::write(&auth, "{").expect("write invalid auth");
    let error = require_eval_auth_file("fixture", &agent).expect_err("invalid auth");
    assert!(
        error
            .to_string()
            .contains("Eval authentication file is invalid:")
    );

    std::fs::write(&auth, json!({"other": {}}).to_string()).expect("write wrong provider");
    let error = require_eval_auth_file("fixture", &agent).expect_err("missing provider");
    assert_eq!(
        error.to_string(),
        "Eval authentication file has no credential for provider fixture."
    );

    std::fs::write(&auth, json!({"fixture": {"apiKey": "x"}}).to_string())
        .expect("write valid auth");
    assert_eq!(
        require_eval_auth_file("fixture", &agent).expect("valid auth"),
        auth
    );
}

#[test]
fn docker_args_are_exact_and_include_uid_gid_when_supplied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let context = context(dir.path());
    let output = dir.path().join("out");
    let args = docker_args(
        &context,
        DocumentationVariant::WithoutDocs,
        &output,
        &["--project".to_owned(), "docs".to_owned()],
        Some((1000, 1001)),
    )
    .expect("docker args");
    assert!(output.is_dir());
    let artifacts_mount = format!("type=bind,source={},target=/artifacts", output.display());
    let expected = vec![
        "run",
        "--rm",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,exec,mode=1777",
        "--tmpfs",
        "/repo/node_modules/.vite-temp:rw,exec,mode=1777",
        "--mount",
        &artifacts_mount,
        "--env",
        "PI_EVAL_ARTIFACT_DIR=/artifacts",
        "--env",
        "PI_EVAL_RUNS_PER_VARIANT=2",
        "--env",
        "PI_EVAL_SANDBOX_UID=65532",
        "--env",
        "PI_EVAL_SANDBOX_GID=65532",
        "--env",
        "PI_PROVIDER=fixture",
        "--env",
        "PI_MODEL=fixture/model",
        "--env",
        "PI_EVAL_ARTIFACT_UID=1000",
        "--env",
        "PI_EVAL_ARTIFACT_GID=1001",
        "--mount",
        "type=bind,source=/run/pi-eval-secrets/auth.json,target=/run/pi-eval-secrets/auth.json,readonly",
        "pi-evals-abc-without-docs:local",
        "--project",
        "docs",
    ];
    assert_eq!(args, expected);
}

#[test]
fn docker_args_omit_uid_gid_when_unavailable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let args = docker_args(
        &context(dir.path()),
        DocumentationVariant::WithDocs,
        &dir.path().join("out"),
        &[],
        None,
    )
    .expect("docker args");
    assert!(!args.iter().any(|arg| arg.contains("PI_EVAL_ARTIFACT_UID")));
    assert!(args.contains(&"pi-evals-abc-with-docs:local".to_owned()));
    #[cfg(not(target_os = "linux"))]
    assert_eq!(process_uid_gid(), None);
    #[cfg(target_os = "linux")]
    let _ = process_uid_gid();
}

#[test]
fn task_directory_name_matches_upstream_sha256() {
    assert_eq!(
        task_directory_name(&task()),
        "ad79d6b71492403a40f4ce7d2acfd5e73bfed0b350d0b7b53c863e4b717add49"
    );
}

#[test]
fn regex_escaping_matches_upstream_test_name_pattern() {
    assert_eq!(escape_regex("a.b(c)[d]"), "a\\.b\\(c\\)\\[d\\]");
    let dir = tempfile::tempdir().expect("tempdir");
    let context = context(dir.path());
    let runner = RecordingRunner::default();
    let report = run_task(&context, &task(), &runner, None).expect("run task");
    assert_eq!(report, None);
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "docker");
    assert!(calls[0].1.contains(&"^tool access create$".to_owned()));
    assert!(calls[0].1.contains(&"--testNamePattern".to_owned()));
}

#[test]
fn build_images_uses_both_variants_and_inspects_ids() {
    let runner = RecordingRunner::with_outputs(vec![
        CommandOutput {
            status: 0,
            stdout: String::new(),
        },
        CommandOutput {
            status: 0,
            stdout: "sha-without\n".to_owned(),
        },
        CommandOutput {
            status: 0,
            stdout: String::new(),
        },
        CommandOutput {
            status: 0,
            stdout: "sha-with\n".to_owned(),
        },
    ]);
    let images = build_images(
        Path::new("/repo"),
        Path::new("/repo/packages/evals"),
        &runner,
    )
    .expect("build images");
    assert_eq!(images.without_docs.id, "sha-without");
    assert_eq!(images.with_docs.id, "sha-with");
    let calls = runner.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].1[0], "build");
    assert_eq!(calls[0].1[2], "without_docs");
    assert_eq!(calls[1].1[0], "image");
    assert!(calls[1].2);
    assert_eq!(calls[2].1[2], "with_docs");
    assert_eq!(calls[3].1[3], "{{.Id}}");
}

fn create_executable(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
    }
    path
}

#[test]
fn resolve_container_runtime_honors_env_override_and_trims_whitespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    create_executable(dir.path(), "docker");
    create_executable(dir.path(), "podman");
    let path = std::env::join_paths([dir.path()]).expect("join path");

    assert_eq!(
        resolve_container_runtime(Some(" /custom/bin/podman "), Some(&path))
            .expect("override runtime"),
        "/custom/bin/podman"
    );
    assert_eq!(
        resolve_container_runtime(Some("   "), Some(&path)).expect("blank override falls back"),
        "docker"
    );
    assert_eq!(
        resolve_container_runtime(Some(""), Some(&path)).expect("empty override falls back"),
        "docker"
    );
}

#[test]
fn resolve_container_runtime_prefers_docker_over_podman_regardless_of_path_order() {
    let podman_dir = tempfile::tempdir().expect("podman tempdir");
    let docker_dir = tempfile::tempdir().expect("docker tempdir");
    create_executable(podman_dir.path(), "podman");
    create_executable(docker_dir.path(), "docker");
    let path = std::env::join_paths([podman_dir.path(), docker_dir.path()]).expect("join path");

    assert_eq!(
        resolve_container_runtime(None, Some(&path)).expect("docker preferred"),
        "docker"
    );
}

#[test]
fn resolve_container_runtime_falls_back_to_podman_when_docker_absent_or_non_executable() {
    let podman_dir = tempfile::tempdir().expect("podman tempdir");
    create_executable(podman_dir.path(), "podman");
    let only_podman = std::env::join_paths([podman_dir.path()]).expect("join path");
    assert_eq!(
        resolve_container_runtime(None, Some(&only_podman)).expect("podman fallback"),
        "podman"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let non_exec_dir = tempfile::tempdir().expect("non-exec tempdir");
        let non_exec_docker = non_exec_dir.path().join("docker");
        std::fs::write(&non_exec_docker, "not executable").expect("write non-exec file");
        std::fs::set_permissions(&non_exec_docker, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");

        let dir_docker_dir = tempfile::tempdir().expect("dir tempdir");
        std::fs::create_dir(dir_docker_dir.path().join("docker")).expect("mkdir docker");

        let path = std::env::join_paths([
            non_exec_dir.path(),
            dir_docker_dir.path(),
            podman_dir.path(),
        ])
        .expect("join path");
        assert_eq!(
            resolve_container_runtime(None, Some(&path))
                .expect("skips non-executable and directory"),
            "podman"
        );
    }
}

#[test]
fn resolve_container_runtime_errors_when_no_runtime_found() {
    let empty_dir = tempfile::tempdir().expect("empty tempdir");
    let empty_dir_path = std::env::join_paths([empty_dir.path()]).expect("join path");
    for path in [
        Some(empty_dir_path.as_os_str()),
        Some(std::ffi::OsStr::new("")),
        None,
    ] {
        let error =
            resolve_container_runtime(None, path).expect_err("missing runtime should error");
        let message = error.to_string();
        assert!(message.contains("docker"), "message: {message}");
        assert!(message.contains("podman"), "message: {message}");
        assert!(
            message.contains("PI_EVALS_CONTAINER_BIN"),
            "message: {message}"
        );
    }
}

#[test]
fn system_command_runner_with_container_runtime_overrides_default_command_only() {
    let custom = SystemCommandRunner::with_container_runtime(PathBuf::from("."), "/opt/bin/podman");
    assert_eq!(custom.container_runtime(), Some("/opt/bin/podman"));
    assert_eq!(
        custom.resolve_command("docker").expect("resolve docker"),
        "/opt/bin/podman"
    );
    assert_eq!(
        custom.resolve_command("git").expect("resolve non-docker"),
        "git"
    );

    let default_runner = SystemCommandRunner::new(PathBuf::from("."));
    assert_eq!(default_runner.container_runtime(), None);
    assert_eq!(
        default_runner
            .resolve_command("git")
            .expect("resolve non-docker"),
        "git"
    );
}
