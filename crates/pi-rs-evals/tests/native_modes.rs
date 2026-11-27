//! Exercise host-mode routing and the audit verdict using a real native agent.
#[path = "common/mock_model.rs"]
mod mock_model;

use std::{path::Path, process::Command};

fn run(mode: &str, filters: &[&str]) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let agent = tempfile::tempdir()?;
    let _model = mock_model::MockModel::start(agent.path())?;
    let binary = Path::new(env!("CARGO_BIN_EXE_pi-evals"));
    Ok(Command::new(binary)
        .arg(mode)
        .args(filters)
        .args(["--provider", "eval-fixture", "--model", "eval-model"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env(
            "PI_EVAL_PI_BINARY",
            binary.with_file_name(format!("pi{}", std::env::consts::EXE_SUFFIX)),
        )
        .env("PI_CODING_AGENT_DIR", agent.path())
        .env_remove("PI_EVAL_REPO_ROOT")
        .env_remove("PI_EVAL_CONTAINER")
        .env_remove("PI_EVAL_VARIANT")
        .env_remove("PI_EVAL_RUNS_PER_VARIANT")
        .output()?)
}

#[test]
fn host_scores_the_agent_reply() -> Result<(), Box<dyn std::error::Error>> {
    let smoke = run("eval:host", &[])?;
    assert!(
        smoke.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&smoke.stdout),
        String::from_utf8_lossy(&smoke.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&smoke.stdout)?;
    assert_eq!(result["score"], 1);
    assert!(
        result["totalTokens"]
            .as_f64()
            .is_some_and(|tokens| tokens > 0.0)
    );
    Ok(())
}

#[test]
fn audit_rejects_a_verdict_without_source_evidence() -> Result<(), Box<dyn std::error::Error>> {
    let audit = run("eval:audit", &["-t", "models[.]md"])?;
    assert_eq!(audit.status.code(), Some(1));
    let verdict: serde_json::Value = serde_json::from_slice(&audit.stdout)?;
    assert_eq!(
        verdict["score"],
        0,
        "{}",
        String::from_utf8_lossy(&audit.stderr)
    );
    assert_eq!(verdict["outcome"], "scored");
    Ok(())
}
