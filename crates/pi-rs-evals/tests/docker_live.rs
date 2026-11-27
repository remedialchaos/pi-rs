//! Real native Pi and evaluator images against a credential-free loopback model.
#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

#[path = "common/mock_model.rs"]
mod mock_model;

use pi_rs_evals::docker::{
    CONTAINER_RUNTIME_ENV, SystemCommandRunner, resolve_host_container_runtime,
};
use pi_rs_evals::plan::EvalTask;
use pi_rs_evals::report::{session_snapshot_path, sha256_hex};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
};

struct Images {
    runtime: String,
    tags: [String; 2],
}
impl Drop for Images {
    fn drop(&mut self) {
        let _ = Command::new(&self.runtime)
            .arg("rmi")
            .args(&self.tags)
            .output();
    }
}

#[test]
fn real_native_images_compare_documentation_cases_and_preserve_pi_sessions()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = resolve_host_container_runtime()?;
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("repo");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("Missing workspace root")?;
    // Give the test its own image tags while building from the actual workspace and shipped Dockerfile.
    symlink(repo, &root)?;
    let hash = sha256_hex(root.to_string_lossy().as_bytes());
    let prefix = format!("pi-evals-{}", &hash[..12]);
    let images = Images {
        runtime: runtime.clone(),
        tags: [
            format!("{prefix}-without-docs:local"),
            format!("{prefix}-with-docs:local"),
        ],
    };
    let agent = tmp.path().join("agent");
    let _model = mock_model::MockModel::start(&agent)?;
    let artifacts = tmp.path().join("artifacts");
    let wrapper = tmp.path().join("test-oci");
    fs::write(
        &wrapper,
        "#!/bin/sh\nif [ \"$1\" = run ]; then shift; exec \"$PI_EVAL_TEST_RUNTIME\" run --network=host \"$@\"; fi\nexec \"$PI_EVAL_TEST_RUNTIME\" \"$@\"\n",
    )?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
    let binary = Path::new(env!("CARGO_BIN_EXE_pi-evals"));
    let output = Command::new(binary)
        .current_dir(&root)
        .env("PI_EVAL_REPO_ROOT", &root)
        .env("PI_EVAL_ARTIFACT_DIR", &artifacts)
        .env("PI_EVAL_TEST_RUNTIME", &runtime)
        .env(CONTAINER_RUNTIME_ENV, &wrapper)
        .env("PI_EVAL_PI_BINARY", binary.with_file_name("pi"))
        .env("PI_CODING_AGENT_DIR", &agent)
        .args([
            "--provider",
            "eval-fixture",
            "--model",
            "eval-model",
            "--runs-per-variant",
            "1",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let protocol: Value = serde_json::from_slice(&fs::read(artifacts.join("protocol.json"))?)?;
    assert_ne!(
        protocol["images"]["with_docs"]["id"],
        protocol["images"]["without_docs"]["id"]
    );
    let tasks: Vec<EvalTask> =
        serde_json::from_slice(&fs::read(artifacts.join("expected-runs.json"))?)?;
    assert_eq!(tasks.len(), 14);
    assert_eq!(protocol["files"].as_array().map(Vec::len), Some(6));
    assert_eq!(protocol["cases"].as_array().map(Vec::len), Some(7));
    let observations = fs::read_to_string(artifacts.join("observations.jsonl"))?;
    let outcomes = observations
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(outcomes.len(), 14);
    for (task, outcome) in tasks.iter().zip(outcomes) {
        assert_eq!(outcome["outcome"], "scored", "{outcome}");
        let session = fs::read_to_string(session_snapshot_path(&artifacts, task))?;
        assert!(
            session.contains("Paris"),
            "real assistant response is missing from session"
        );
        assert!(!session.contains("local-test-key"));
    }
    let report: Value = serde_json::from_slice(&fs::read(artifacts.join("report.json"))?)?;
    assert!(report["blockedPairs"].as_array().is_some_and(Vec::is_empty));
    for (index, image) in images.tags.iter().enumerate() {
        let expected = if index == 0 {
            "! test -e /opt/pi/docs"
        } else {
            "test -s /opt/pi/docs/models.md"
        };
        let check = format!(
            "test ! -r /usr/local/bin/pi-evals && test ! -e /repo && test ! -w /opt/pi && test ! -r /run/pi-eval-secrets && {expected}"
        );
        let status = Command::new(&runtime)
            .args([
                "run",
                "--rm",
                "--read-only",
                "--user",
                "65532:65532",
                "--entrypoint",
                "/bin/sh",
                image,
                "-c",
                &check,
            ])
            .status()?;
        assert!(
            status.success(),
            "unexpected image permissions or documentation in {image}"
        );
    }
    Ok(())
}

fn find_runtime_executable(binary_name: &str) -> PathBuf {
    let path_env = std::env::var_os("PATH").expect("PATH must be set");
    for directory in std::env::split_paths(&path_env) {
        let candidate = directory.join(binary_name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("could not locate {binary_name} on PATH");
}

#[test]
fn system_command_runner_and_binary_honor_explicit_runtime_override() {
    use pi_rs_evals::docker::CommandRunner;

    let runtime = resolve_host_container_runtime().expect("host container runtime");
    let abs_runtime = if Path::new(&runtime).is_absolute() {
        PathBuf::from(&runtime)
    } else {
        find_runtime_executable(&runtime)
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let runner = SystemCommandRunner::with_container_runtime(
        dir.path().to_path_buf(),
        abs_runtime.to_string_lossy().into_owned(),
    );
    let output = runner
        .execute("docker", &["--version".to_owned()], true)
        .expect("execute --version via explicit runtime");
    assert_eq!(output.status, 0);
    assert!(
        !output.stdout.trim().is_empty(),
        "expected non-empty --version output"
    );

    let agent_dir = dir.path().join("agent");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::write(agent_dir.join("auth.json"), br#"{"fixture":{}}"#).expect("write mock auth");

    let binary = env!("CARGO_BIN_EXE_pi-evals");
    let bin_output = Command::new(binary)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["--provider", "fixture", "--model", "test-model"])
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env(
            CONTAINER_RUNTIME_ENV,
            "/nonexistent/pi-evals-missing-runtime",
        )
        .output()
        .expect("run pi-evals binary");
    assert_eq!(bin_output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&bin_output.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "expected error on stderr when PI_EVALS_CONTAINER_BIN is invalid"
    );
}
