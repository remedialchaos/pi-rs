//! `pi-evals` documentation eval runner binary (Milestone 2.4).
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pi_rs_evals::cli::{
    SystemEvalDockerOps, artifact_run_id, iso_timestamp_utc, map_environment, parse_eval_cli,
    system_docker_context,
};
use pi_rs_evals::{
    error::{EvalError, EvalResult},
    runner::{execute_case, run_host, selected_cases},
    suites::SuiteGroup,
};

fn host_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn environment_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn repository_root() -> EvalResult<PathBuf> {
    if let Some(root) = std::env::var_os("PI_EVAL_REPO_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|root| root.join("crates/pi-rs-evals/Cargo.toml").is_file())
        .map(Path::to_owned)
        .ok_or_else(|| {
            EvalError::message("Run pi-evals from the pi-rs repository or set PI_EVAL_REPO_ROOT.")
        })
}

fn pi_binary() -> PathBuf {
    std::env::var_os("PI_EVAL_PI_BINARY")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| {
                    e.parent()
                        .map(|p| p.join(format!("pi{}", std::env::consts::EXE_SUFFIX)))
                })
                .filter(|p| p.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("pi"))
}

async fn run() -> EvalResult<bool> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: pi-evals [eval:docs|eval:host|eval:audit] [--list] [suite.rs ...] [-t PATTERN] [--provider PROVIDER --model MODEL] [--runs-per-variant N]"
        );
        return Ok(false);
    }
    let mode = match args.first().map(String::as_str) {
        Some("eval:docs" | "eval:host" | "eval:audit" | "__discover" | "__run") => args.remove(0),
        _ => String::from("eval:docs"),
    };
    if mode == "__run" {
        let [task] = args.as_slice() else {
            return Err(EvalError::message("Expected one serialized eval task."));
        };
        let task = serde_json::from_str(task)?;
        let artifacts = std::env::var_os("PI_EVAL_ARTIFACT_DIR").map(PathBuf::from);
        let observation = execute_case(
            &repository_root()?,
            &pi_binary(),
            &task,
            artifacts.as_deref(),
        )
        .await?;
        println!("{}", serde_json::to_string(&observation)?);
        return Ok(false);
    }
    let list = args.iter().any(|arg| arg == "--list");
    args.retain(|arg| arg != "--list");
    let cli = parse_eval_cli(&args, &environment_lookup)?;
    let group = match mode.as_str() {
        "eval:host" => SuiteGroup::Smoke,
        "eval:audit" => SuiteGroup::Audit,
        _ => SuiteGroup::Documentation,
    };
    if mode == "__discover" || list {
        let cases = selected_cases(&repository_root()?, group, &cli)?;
        let entries: Vec<_> = cases
            .into_iter()
            .map(|case| serde_json::json!({"name":case.full_name,"file":case.file}))
            .collect();
        println!("{}", serde_json::to_string(&entries)?);
        return Ok(false);
    }
    let Some(provider) = cli.provider.clone() else {
        return Err(pi_rs_evals::error::EvalError::message(
            "Set PI_PROVIDER and PI_MODEL, or pass --provider and --model.",
        ));
    };
    let Some(model) = cli.model.clone() else {
        return Err(pi_rs_evals::error::EvalError::message(
            "Set PI_PROVIDER and PI_MODEL, or pass --provider and --model.",
        ));
    };

    let repo = repository_root()?;
    if mode == "eval:host" || mode == "eval:audit" {
        return run_host(&repo, &pi_binary(), group, &cli, &provider, &model).await;
    }
    let package_root = repo.join("crates/pi-rs-evals");
    let environment: std::collections::HashMap<String, String> = std::env::vars().collect();
    let run_id = artifact_run_id(
        &iso_timestamp_utc(chrono::Utc::now()),
        &uuid::Uuid::new_v4().to_string(),
    );
    let artifact_directory = std::env::var_os("PI_EVAL_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| package_root.join(".eval").join(run_id));
    let artifact_directory = pi_rs_evals::docker::lexical_resolve(&artifact_directory);
    let (context, _) = system_docker_context(
        artifact_directory.clone(),
        &provider,
        &model,
        cli.runs_per_variant,
        &map_environment(&environment),
        host_home().as_deref(),
    )?;
    let ops = SystemEvalDockerOps::new(Path::new(&package_root), context);
    let summary =
        pi_rs_evals::cli::execute_eval_run(&cli, &package_root, &artifact_directory, &ops)?;
    Ok(!summary.report.blocked_pairs.is_empty())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(has_blocked_pairs) => {
            if has_blocked_pairs {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
