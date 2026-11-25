//! Live rootless container E2E tests against the host OCI runtime (`podman` or `docker`).
#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use pi_rs_evals::cli::{
    SystemEvalDockerOps, execute_eval_run, map_environment, parse_eval_cli, system_docker_context,
};
use pi_rs_evals::docker::{
    CONTAINER_RUNTIME_ENV, SystemCommandRunner, lexical_resolve, resolve_host_container_runtime,
};
use pi_rs_evals::plan::{DOCUMENTATION_VARIANTS, DocumentationVariant};
use pi_rs_evals::report::sha256_hex;
use serde_json::{Value, json};

const AUTH_SECRET_TOKEN: &str = "live-container-secret-token";

const ENTRYPOINT_SOURCE: &str = r##"#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_string_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|item| json_escape(item)).collect();
    format!("[{}]", parts.join(","))
}

fn write_artifact_file(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
        .map_err(|err| format!("failed to chmod {}: {err}", path.display()))?;
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let artifact_dir = env::var("PI_EVAL_ARTIFACT_DIR")
        .map_err(|_| "missing PI_EVAL_ARTIFACT_DIR".to_owned())?;
    let variant = env::var("PI_EVAL_VARIANT")
        .map_err(|_| "missing PI_EVAL_VARIANT".to_owned())?;
    let provider = env::var("PI_PROVIDER").map_err(|_| "missing PI_PROVIDER".to_owned())?;
    let model = env::var("PI_MODEL").map_err(|_| "missing PI_MODEL".to_owned())?;
    let runs_per_variant = env::var("PI_EVAL_RUNS_PER_VARIANT")
        .map_err(|_| "missing PI_EVAL_RUNS_PER_VARIANT".to_owned())?;
    let sandbox_uid = env::var("PI_EVAL_SANDBOX_UID")
        .map_err(|_| "missing PI_EVAL_SANDBOX_UID".to_owned())?;
    let sandbox_gid = env::var("PI_EVAL_SANDBOX_GID")
        .map_err(|_| "missing PI_EVAL_SANDBOX_GID".to_owned())?;

    let auth_text = fs::read_to_string("/run/pi-eval-secrets/auth.json")
        .map_err(|err| format!("failed to read readonly auth mount: {err}"))?;
    if !auth_text.contains("live-container-secret-token") {
        return Err("auth.json mount did not contain expected secret token".to_owned());
    }

    let root_write_err = match fs::write("/readonly-probe", b"denied") {
        Ok(()) => return Err("root filesystem was unexpectedly writable".to_owned()),
        Err(err) => format!("{:?}", err.kind()),
    };
    let auth_write_err = match fs::write("/run/pi-eval-secrets/auth.json", b"mutated") {
        Ok(()) => return Err("auth.json mount was unexpectedly writable".to_owned()),
        Err(err) => format!("{:?}", err.kind()),
    };

    fs::write("/tmp/probe.txt", b"tmp-ok")
        .map_err(|err| format!("failed to write /tmp/probe.txt: {err}"))?;
    if fs::read_to_string("/tmp/probe.txt")
        .map_err(|err| format!("failed to read /tmp/probe.txt: {err}"))?
        != "tmp-ok"
    {
        return Err("/tmp/probe.txt content mismatch".to_owned());
    }

    fs::write("/repo/node_modules/.vite-temp/probe.txt", b"vite-ok")
        .map_err(|err| format!("failed to write vite-temp probe: {err}"))?;
    if fs::read_to_string("/repo/node_modules/.vite-temp/probe.txt")
        .map_err(|err| format!("failed to read vite-temp probe: {err}"))?
        != "vite-ok"
    {
        return Err("vite-temp probe content mismatch".to_owned());
    }

    let is_discover = args.first().is_some_and(|arg| arg == "--discover");
    let mode = if is_discover { "discover" } else { "task" };
    let artifact_path = Path::new(&artifact_dir);

    let sandbox_check = format!(
        concat!(
            "{{",
            "\"mode\":{},",
            "\"variant\":{},",
            "\"authVerified\":true,",
            "\"rootReadonly\":true,",
            "\"rootWriteError\":{},",
            "\"authReadonly\":true,",
            "\"authWriteError\":{},",
            "\"tmpWritable\":true,",
            "\"viteTmpWritable\":true,",
            "\"artifactDir\":{},",
            "\"provider\":{},",
            "\"model\":{},",
            "\"runsPerVariant\":{},",
            "\"sandboxUid\":{},",
            "\"sandboxGid\":{},",
            "\"args\":{}",
            "}}\n"
        ),
        json_escape(mode),
        json_escape(&variant),
        json_escape(&root_write_err),
        json_escape(&auth_write_err),
        json_escape(&artifact_dir),
        json_escape(&provider),
        json_escape(&model),
        json_escape(&runs_per_variant),
        json_escape(&sandbox_uid),
        json_escape(&sandbox_gid),
        json_string_array(&args),
    );
    write_artifact_file(&artifact_path.join("sandbox-check.json"), &sandbox_check)?;

    if is_discover {
        let discovered = "[{\"name\":\"Container sandbox > verifies mounts and read-only rootfs\",\"file\":\"/repo/packages/evals/evals/sample.docs.eval.ts\"}]\n";
        write_artifact_file(&artifact_path.join("discovered-tests.json"), discovered)?;
        return Ok(());
    }

    let avg_score = if variant == "with_docs" { "1" } else { "0" };
    let session_jsonl = format!("{{\"type\":\"session\",\"variant\":\"{variant}\"}}\n");
    let vitest_report = format!(
        concat!(
            "{{",
            "\"numTotalTests\":1,",
            "\"numPassedTests\":1,",
            "\"numFailedTests\":0,",
            "\"numPendingTests\":0,",
            "\"numTodoTests\":0,",
            "\"startTime\":0,",
            "\"success\":true,",
            "\"testResults\":[{{",
            "\"message\":\"\",",
            "\"name\":\"/repo/packages/evals/evals/sample.docs.eval.ts\",",
            "\"status\":\"passed\",",
            "\"assertionResults\":[{{",
            "\"fullName\":\"Container sandbox verifies mounts and read-only rootfs\",",
            "\"title\":\"verifies mounts and read-only rootfs\",",
            "\"status\":\"passed\",",
            "\"failureMessages\":[],",
            "\"meta\":{{",
            "\"eval\":{{\"avgScore\":{}}},",
            "\"harness\":{{\"run\":{{",
            "\"usage\":{{",
            "\"provider\":{},",
            "\"model\":{},",
            "\"inputTokens\":12,",
            "\"outputTokens\":8,",
            "\"totalTokens\":20,",
            "\"toolCalls\":2",
            "}},",
            "\"timings\":{{\"totalMs\":25}},",
            "\"artifacts\":{{\"runId\":\"live-run\",\"piSessionJsonl\":{}}},",
            "\"errors\":[]",
            "}}}}",
            "}}",
            "}}]",
            "}}]",
            "}}\n"
        ),
        avg_score,
        json_escape(&provider),
        json_escape(&model),
        json_escape(&session_jsonl),
    );
    write_artifact_file(&artifact_path.join("vitest.json"), &vitest_report)?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("eval-entrypoint error: {err}");
            ExitCode::FAILURE
        }
    }
}
"##;

struct ImageCleanupGuard {
    runtime: String,
    images: Vec<String>,
}

impl ImageCleanupGuard {
    fn for_repo(runtime: String, repository_root: &Path) -> Self {
        let resolved = lexical_resolve(repository_root);
        let prefix = format!(
            "pi-evals-{}",
            sha256_hex(resolved.to_string_lossy().as_bytes())
                .chars()
                .take(12)
                .collect::<String>()
        );
        let images = vec![
            format!("{prefix}-without-docs:local"),
            format!("{prefix}-with-docs:local"),
        ];
        Self { runtime, images }
    }
}

impl Drop for ImageCleanupGuard {
    fn drop(&mut self) {
        if self.images.is_empty() {
            return;
        }
        let mut command = Command::new(&self.runtime);
        command.arg("rmi").arg("-f");
        for image in &self.images {
            command.arg(image);
        }
        let _ = command.output();
    }
}

fn compile_static_entrypoint(work_dir: &Path, output_bin: &Path) {
    let source_path = work_dir.join("eval_entrypoint.rs");
    fs::write(&source_path, ENTRYPOINT_SOURCE).expect("write entrypoint source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2024")
        .arg("-O")
        .arg("-C")
        .arg("target-feature=+crt-static")
        .arg(&source_path)
        .arg("-o")
        .arg(output_bin)
        .output()
        .expect("invoke rustc for static container entrypoint");
    assert!(
        output.status.success(),
        "rustc failed to compile static entrypoint: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::set_permissions(output_bin, fs::Permissions::from_mode(0o755))
        .expect("chmod entrypoint binary");
}

fn write_auth_file(agent_dir: &Path) {
    fs::create_dir_all(agent_dir).expect("create agent dir");
    let auth_path = agent_dir.join("auth.json");
    fs::write(
        &auth_path,
        json!({ "fixture": { "apiKey": AUTH_SECRET_TOKEN } }).to_string(),
    )
    .expect("write auth.json");
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600)).expect("chmod auth.json");
}

fn prepare_container_workspace(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let repo_root = root.join("repo");
    let package_root = repo_root.join("packages").join("evals");
    let docker_dir = package_root.join("docker");
    let evals_dir = package_root.join("evals");
    let rootfs = repo_root.join("rootfs");
    let agent_dir = root.join("agent");

    fs::create_dir_all(&docker_dir).expect("create docker dir");
    fs::create_dir_all(&evals_dir).expect("create evals dir");
    fs::create_dir_all(rootfs.join("tmp")).expect("create rootfs tmp");
    fs::create_dir_all(rootfs.join("repo").join("node_modules").join(".vite-temp"))
        .expect("create rootfs vite-temp");
    fs::create_dir_all(rootfs.join("artifacts")).expect("create rootfs artifacts");
    fs::create_dir_all(rootfs.join("run").join("pi-eval-secrets"))
        .expect("create rootfs secret dir");

    fs::write(rootfs.join("tmp").join(".keep"), b"").expect("keep tmp");
    fs::write(
        rootfs
            .join("repo")
            .join("node_modules")
            .join(".vite-temp")
            .join(".keep"),
        b"",
    )
    .expect("keep vite-temp");
    fs::write(rootfs.join("artifacts").join(".keep"), b"").expect("keep artifacts");
    fs::write(
        rootfs.join("run").join("pi-eval-secrets").join("auth.json"),
        b"",
    )
    .expect("placeholder auth.json");

    compile_static_entrypoint(root, &repo_root.join("eval-entrypoint"));

    let dockerfile = "\
FROM scratch AS without_docs\n\
COPY rootfs/ /\n\
COPY eval-entrypoint /eval-entrypoint\n\
ENV PI_EVAL_VARIANT=without_docs\n\
ENTRYPOINT [\"/eval-entrypoint\"]\n\
\n\
FROM scratch AS with_docs\n\
COPY rootfs/ /\n\
COPY eval-entrypoint /eval-entrypoint\n\
ENV PI_EVAL_VARIANT=with_docs\n\
ENTRYPOINT [\"/eval-entrypoint\"]\n";
    fs::write(docker_dir.join("Dockerfile"), dockerfile).expect("write Dockerfile");
    fs::write(evals_dir.join("sample.docs.eval.ts"), "// live e2e eval\n")
        .expect("write eval file");
    write_auth_file(&agent_dir);

    (repo_root, package_root, agent_dir)
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", path.display());
    });
    serde_json::from_str(&text).unwrap_or_else(|err| {
        panic!("failed to parse {}: {err}", path.display());
    })
}

fn assert_sandbox_check(
    check_path: &Path,
    expected_mode: &str,
    expected_variant: DocumentationVariant,
    expected_args: &[&str],
) {
    let check = read_json(check_path);
    assert_eq!(check["mode"], expected_mode);
    assert_eq!(check["variant"], expected_variant.as_str());
    assert_eq!(check["authVerified"], true);
    assert_eq!(check["rootReadonly"], true);
    assert_eq!(check["authReadonly"], true);
    assert_eq!(check["tmpWritable"], true);
    assert_eq!(check["viteTmpWritable"], true);
    assert_eq!(check["artifactDir"], "/artifacts");
    assert_eq!(check["provider"], "fixture");
    assert_eq!(check["model"], "test-model");
    assert_eq!(check["runsPerVariant"], "1");
    assert_eq!(check["sandboxUid"], "65532");
    assert_eq!(check["sandboxGid"], "65532");
    let expected: Vec<Value> = expected_args.iter().map(|arg| json!(arg)).collect();
    assert_eq!(check["args"], Value::Array(expected));
}

#[test]
fn live_container_eval_run_end_to_end() {
    let runtime = resolve_host_container_runtime()
        .expect("a real OCI container runtime (docker or podman) must be installed");
    let workspace = tempfile::tempdir().expect("tempdir");
    let (repo_root, package_root, agent_dir) = prepare_container_workspace(workspace.path());
    let artifact_directory = workspace.path().join("artifacts");

    let cleanup_guard = ImageCleanupGuard::for_repo(runtime.clone(), &repo_root);
    let expected_images = cleanup_guard.images.clone();

    let cli = parse_eval_cli(
        &[
            "--provider".to_owned(),
            "fixture".to_owned(),
            "--model".to_owned(),
            "test-model".to_owned(),
            "--runs-per-variant".to_owned(),
            "1".to_owned(),
        ],
        &map_environment(&std::collections::HashMap::new()),
    )
    .expect("parse eval cli");

    let env = std::collections::HashMap::from([(
        "PI_CODING_AGENT_DIR".to_owned(),
        agent_dir.to_string_lossy().into_owned(),
    )]);
    let (context, _) = system_docker_context(
        artifact_directory.clone(),
        "fixture",
        "test-model",
        cli.runs_per_variant,
        &map_environment(&env),
        None,
    )
    .expect("system docker context");
    let ops = SystemEvalDockerOps::new(&package_root, context);

    let summary = execute_eval_run(&cli, &package_root, &artifact_directory, &ops)
        .expect("execute live container eval run");

    assert!(summary.report.blocked_pairs.is_empty());
    assert_eq!(summary.report.comparisons.len(), 1);
    let comparison = &summary.report.comparisons[0];
    assert_eq!(comparison.eval_set, "Container sandbox");
    assert_eq!(comparison.eligible_pairs, 1);
    assert_eq!(comparison.control_pass_rate, Some(0.0));
    assert_eq!(comparison.treatment_pass_rate, Some(1.0));
    assert_eq!(comparison.lift, Some(1.0));

    let protocol = read_json(&artifact_directory.join("protocol.json"));
    assert_eq!(
        protocol["images"]["without_docs"]["name"],
        expected_images[0]
    );
    assert_eq!(protocol["images"]["with_docs"]["name"], expected_images[1]);
    let without_id = protocol["images"]["without_docs"]["id"]
        .as_str()
        .expect("without_docs image id");
    let with_id = protocol["images"]["with_docs"]["id"]
        .as_str()
        .expect("with_docs image id");
    assert!(!without_id.is_empty());
    assert!(!with_id.is_empty());
    assert_ne!(without_id, with_id);

    for filename in [
        "protocol.json",
        "expected-runs.json",
        "observations.jsonl",
        "report.json",
        "report.txt",
    ] {
        assert!(
            artifact_directory.join(filename).is_file(),
            "missing artifact {filename}"
        );
    }

    for variant in DOCUMENTATION_VARIANTS {
        let discovery_check = artifact_directory
            .join("discovery")
            .join(variant.as_str())
            .join("sandbox-check.json");
        assert_sandbox_check(
            &discovery_check,
            "discover",
            variant,
            &[
                "--discover",
                "--project",
                "docs",
                "evals/sample.docs.eval.ts",
            ],
        );
    }

    let expected_runs = read_json(&artifact_directory.join("expected-runs.json"));
    let runs = expected_runs.as_array().expect("expected-runs array");
    assert_eq!(runs.len(), 2);
    for run in runs {
        let eval_set = run["evalSet"].as_str().expect("evalSet");
        let case_id = run["caseId"].as_str().expect("caseId");
        let variant_str = run["variant"].as_str().expect("variant");
        let model = run["model"].as_str().expect("model");
        let run_number = run["runNumber"].as_u64().expect("runNumber");
        let variant = match variant_str {
            "without_docs" => DocumentationVariant::WithoutDocs,
            "with_docs" => DocumentationVariant::WithDocs,
            other => panic!("unexpected variant {other}"),
        };

        let identity = serde_json::json!([eval_set, case_id, variant_str, model, run_number]);
        let task_hash = sha256_hex(identity.to_string().as_bytes());
        let task_check = artifact_directory
            .join("tasks")
            .join(&task_hash)
            .join("sandbox-check.json");
        assert_sandbox_check(
            &task_check,
            "task",
            variant,
            &[
                "--project",
                "docs",
                "evals/sample.docs.eval.ts",
                "--testNamePattern",
                "^Container sandbox verifies mounts and read-only rootfs$",
            ],
        );

        let session_path = artifact_directory
            .join(variant_str)
            .join("sessions")
            .join(&task_hash)
            .join("session.jsonl");
        let session_text = fs::read_to_string(&session_path).expect("read session snapshot");
        assert_eq!(
            session_text,
            format!("{{\"type\":\"session\",\"variant\":\"{variant_str}\"}}\n")
        );
    }

    drop(cleanup_guard);
    for image in &expected_images {
        let inspect = Command::new(&runtime)
            .args(["image", "inspect", image])
            .output()
            .expect("inspect cleaned-up image");
        assert!(
            !inspect.status.success(),
            "ephemeral test image {image} should have been removed by Drop guard"
        );
    }
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
    write_auth_file(&agent_dir);
    let evals_dir = dir.path().join("evals");
    fs::create_dir_all(&evals_dir).expect("create evals dir");
    fs::write(evals_dir.join("sample.docs.eval.ts"), "// fixture\n").expect("write eval file");

    let binary = env!("CARGO_BIN_EXE_pi-evals");
    let bin_output = Command::new(binary)
        .current_dir(dir.path())
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
