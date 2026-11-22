//! `pi-evals` documentation eval runner binary (Milestone 2.4).
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pi_rs_evals::cli::{
    SystemEvalDockerOps, artifact_run_id, iso_timestamp_utc, map_environment, parse_eval_cli,
    system_docker_context,
};
use pi_rs_evals::error::EvalResult;

fn host_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn environment_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn run() -> EvalResult<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_eval_cli(&args, &environment_lookup)?;
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

    let package_root = std::env::current_dir()?;
    let environment: std::collections::HashMap<String, String> = std::env::vars().collect();
    let run_id = artifact_run_id(
        &iso_timestamp_utc(chrono::Utc::now()),
        &uuid::Uuid::new_v4().to_string(),
    );
    let artifact_directory =
        pi_rs_evals::docker::lexical_resolve(&package_root.join(".eval").join(&run_id));
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

fn main() -> ExitCode {
    match run() {
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
