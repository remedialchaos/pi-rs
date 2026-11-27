//! CLI parsing, path guards, artifact generation, and real binary exit codes.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use pi_rs_evals::cli::{
    EvalDockerOps, compare_discovery, container_path, execute_eval_run, map_environment,
    normalize_discovered_file, parse_eval_cli,
};
use pi_rs_evals::docker::{BuiltImage, BuiltImages, task_directory_name};
use pi_rs_evals::error::EvalResult;
use pi_rs_evals::plan::{DiscoveredEvalCase, DocumentationVariant, EvalTask};
use serde_json::Value;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn env(values: &[(&str, &str)]) -> HashMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn parses_space_and_equals_flag_forms() {
    let cli = parse_eval_cli(
        &args(&[
            "--provider",
            "fixture",
            "--model=fixture/model",
            "--runs-per-variant=3",
            "-t",
            "some test",
            "src/suites/extensions.rs",
            "--testNamePattern=other",
        ]),
        &map_environment(&env(&[])),
    )
    .expect("parse");
    assert_eq!(cli.provider.as_deref(), Some("fixture"));
    assert_eq!(cli.model.as_deref(), Some("fixture/model"));
    assert_eq!(cli.runs_per_variant, 3);
    assert_eq!(cli.requested_files, vec!["src/suites/extensions.rs"]);
    assert_eq!(
        cli.discovery_args,
        vec!["-t", "some test", "--testNamePattern=other"]
    );
}

#[test]
fn falls_back_to_environment_and_validates_errors() {
    let environment = env(&[
        ("PI_PROVIDER", " fixture "),
        ("PI_MODEL", " fixture/model "),
        ("PI_EVAL_RUNS_PER_VARIANT", "1e3"),
    ]);
    let cli = parse_eval_cli(&[], &map_environment(&environment)).expect("env fallback");
    assert_eq!(cli.provider.as_deref(), Some("fixture"));
    assert_eq!(cli.model.as_deref(), Some("fixture/model"));
    assert_eq!(cli.runs_per_variant, 1000);

    let error = parse_eval_cli(
        &args(&["--provider", "fixture"]),
        &map_environment(&env(&[])),
    )
    .expect_err("provider only");
    assert_eq!(
        error.to_string(),
        "CLI model selection requires both --provider and --model."
    );
    let error = parse_eval_cli(&[], &map_environment(&env(&[("PI_PROVIDER", "fixture")])))
        .expect_err("env mismatch");
    assert_eq!(
        error.to_string(),
        "Set both PI_PROVIDER and PI_MODEL, or neither."
    );
    let error = parse_eval_cli(
        &args(&["--runs-per-variant", "0", "--provider", "p", "--model", "m"]),
        &map_environment(&env(&[])),
    )
    .expect_err("bad runs");
    assert_eq!(
        error.to_string(),
        "Runs per variant must be a positive integer."
    );
    let error =
        parse_eval_cli(&args(&["--bogus"]), &map_environment(&env(&[]))).expect_err("unknown arg");
    assert_eq!(error.to_string(), "Unsupported eval argument: --bogus");
    for argument in ["-t", "--testNamePattern", "--testNamePattern="] {
        let error = parse_eval_cli(&args(&[argument]), &map_environment(&env(&[])))
            .expect_err("missing filter value");
        assert!(error.to_string().starts_with("Missing value for"));
    }
}

#[test]
fn container_path_rejects_traversal_and_normalizes_discovered_files() {
    let root = Path::new("/repo/crates/pi-rs-evals");
    assert_eq!(
        container_path(root, "src/suites/models.rs").expect("inside path"),
        "src/suites/models.rs"
    );
    assert_eq!(
        container_path(root, "./src/../src/suites/models.rs").expect("normalized path"),
        "src/suites/models.rs"
    );
    assert!(
        container_path(root, "../secret.rs")
            .expect_err("escaping path")
            .to_string()
            .contains("Eval file must be inside")
    );
    assert!(container_path(root, "/etc/passwd").is_err());

    assert_eq!(
        normalize_discovered_file("src/suites/models.rs").expect("normalize"),
        "src/suites/models.rs"
    );
    assert!(
        normalize_discovered_file("/other/path.ts")
            .expect_err("outside prefix")
            .to_string()
            .contains("outside the container package")
    );
}

#[test]
fn compares_discovery_by_case_identity_and_normalized_file() {
    let left = vec![DiscoveredEvalCase {
        file: "src/suites/extensions.rs".to_owned(),
        full_name: "Set A > case 1".to_owned(),
        eval_set: "Set A".to_owned(),
        case_id: "case 1".to_owned(),
    }];
    let right = left.clone();
    compare_discovery(&left, &right).expect("identical discovery");
    let mut different = left.clone();
    different[0].full_name = "Set A > case 2".to_owned();
    different[0].case_id = "case 2".to_owned();
    assert_eq!(
        compare_discovery(&left, &different)
            .expect_err("different discovery")
            .to_string(),
        "Documentation variants discovered different eval cases."
    );
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pi-evals")
}

#[test]
fn compiled_binary_exits_one_for_cli_errors() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--provider", "fixture"],
            "CLI model selection requires both --provider and --model.",
        ),
        (&["--bogus"], "Unsupported eval argument: --bogus"),
        (
            &[
                "--provider",
                "fixture",
                "--model",
                "m",
                "--runs-per-variant",
                "x",
            ],
            "Runs per variant must be a positive integer.",
        ),
    ];
    for (arguments, expected) in cases {
        let output = Command::new(binary())
            .args(*arguments)
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .output()
            .expect("run binary");
        assert_eq!(output.status.code(), Some(1), "args: {arguments:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "stderr: {stderr}");
    }
}

struct FakeOps {
    cases: Vec<DiscoveredEvalCase>,
    produce_reports: bool,
    artifact_directory: PathBuf,
}

impl EvalDockerOps for FakeOps {
    fn build_images(&self) -> EvalResult<BuiltImages> {
        Ok(BuiltImages {
            without_docs: BuiltImage {
                name: "without:local".to_owned(),
                id: "id-a".to_owned(),
            },
            with_docs: BuiltImage {
                name: "with:local".to_owned(),
                id: "id-b".to_owned(),
            },
        })
    }

    fn discover_cases(
        &self,
        _variant: DocumentationVariant,
        _files: &[String],
        _args: &[String],
    ) -> EvalResult<Vec<DiscoveredEvalCase>> {
        Ok(self.cases.clone())
    }

    fn run_task(&self, task: &EvalTask) -> EvalResult<Option<PathBuf>> {
        if !self.produce_reports {
            return Ok(None);
        }
        let output_dir = self
            .artifact_directory
            .join("tasks")
            .join(task_directory_name(task));
        std::fs::create_dir_all(&output_dir)?;
        let report_path = output_dir.join("observation.json");
        let mut report = pi_rs_evals::report::errored_observation(task);
        report.outcome = pi_rs_evals::report::EvalOutcome::Scored;
        report.score = Some(if task.variant == DocumentationVariant::WithDocs {
            1.0
        } else {
            0.0
        });
        std::fs::write(&report_path, serde_json::to_string(&report)?)?;
        Ok(Some(report_path))
    }

    fn artifact_uid_gid(&self) -> Option<(u32, u32)> {
        None
    }
}

fn setup_package() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn discovered_cases() -> Vec<DiscoveredEvalCase> {
    vec![DiscoveredEvalCase {
        file: "src/suites/models.rs".to_owned(),
        full_name: "Add model > adds the model".to_owned(),
        eval_set: "Add model".to_owned(),
        case_id: "adds the model".to_owned(),
    }]
}

fn cli_run() -> pi_rs_evals::cli::EvalCliOptions {
    parse_eval_cli(
        &args(&["--provider", "fixture", "--model", "model"]),
        &map_environment(&env(&[])),
    )
    .expect("parse cli")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read json")).expect("parse json")
}

#[test]
fn orchestration_writes_all_artifacts_and_computes_protocol_digest() {
    let package = setup_package();
    let artifact_directory = package.path().join(".eval").join("run-1");
    std::fs::create_dir_all(&artifact_directory).expect("artifact dir");
    let ops = FakeOps {
        cases: discovered_cases(),
        produce_reports: true,
        artifact_directory: artifact_directory.clone(),
    };
    let summary =
        execute_eval_run(&cli_run(), package.path(), &artifact_directory, &ops).expect("execute");
    assert!(summary.report.blocked_pairs.is_empty());
    assert_eq!(summary.report.schema_version, 3);

    let protocol = read_json(&artifact_directory.join("protocol.json"));
    let digest = protocol["protocolDigest"]
        .as_str()
        .expect("digest")
        .to_owned();
    assert_eq!(digest.len(), 64);
    let mut without_digest = protocol.clone();
    without_digest
        .as_object_mut()
        .expect("object")
        .remove("protocolDigest");
    let compact = serde_json::to_string(&without_digest).expect("compact");
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(compact.as_bytes());
    assert_eq!(
        digest,
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );

    let expected = read_json(&artifact_directory.join("expected-runs.json"));
    assert_eq!(expected.as_array().expect("array").len(), 2);
    let observations = std::fs::read_to_string(artifact_directory.join("observations.jsonl"))
        .expect("observations");
    assert_eq!(observations.lines().count(), 2);
    assert!(
        summary
            .report_text
            .contains("Documentation Eval Comparisons")
    );
    let report_txt =
        std::fs::read_to_string(artifact_directory.join("report.txt")).expect("report.txt");
    assert_eq!(report_txt, format!("{}\n", summary.report_text));
    let _ = read_json(&artifact_directory.join("report.json"));
}

#[test]
fn orchestration_marks_blocked_pairs_when_reports_are_missing() {
    let package = setup_package();
    let artifact_directory = package.path().join(".eval").join("run-2");
    std::fs::create_dir_all(&artifact_directory).expect("artifact dir");
    let ops = FakeOps {
        cases: discovered_cases(),
        produce_reports: false,
        artifact_directory: artifact_directory.clone(),
    };
    let summary =
        execute_eval_run(&cli_run(), package.path(), &artifact_directory, &ops).expect("execute");
    assert_eq!(summary.report.blocked_pairs.len(), 1);
    assert!(summary.report.comparisons[0].lift.is_none());
}

#[test]
fn host_audit_and_documentation_discovery_select_their_native_cases()
-> Result<(), Box<dyn std::error::Error>> {
    let discover = |mode: &str| -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let output = Command::new(binary())
            .args([mode, "--list"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env_remove("PI_EVAL_REPO_ROOT")
            .env_remove("PI_MODEL")
            .env_remove("PI_PROVIDER")
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let host = discover("eval:host")?;
    assert_eq!(host.len(), 1);
    assert_eq!(host[0]["file"], "src/suites/smoke.rs");
    let audit = discover("eval:audit")?;
    assert!(!audit.is_empty());
    assert!(
        audit
            .iter()
            .all(|c| c["file"] == "src/suites/documentation_audit.rs")
    );
    let docs = discover("eval:docs")?;
    assert_eq!(
        docs.iter()
            .filter_map(|c| c["file"].as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        6
    );
    Ok(())
}
