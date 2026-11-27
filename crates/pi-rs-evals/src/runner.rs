//! Native case selection and execution shared by the host and OCI entry point.
use std::path::Path;

use crate::{
    cli::EvalCliOptions,
    error::{EvalError, EvalResult},
    plan::{DiscoveredEvalCase, DocumentationVariant, EvalTask},
    report::{
        EvalObservation, PI_SESSION_SNAPSHOT_ARTIFACT, errored_observation, native_observation,
    },
    suites::{SuiteGroup, discover, run_case},
};

pub fn selected_cases(
    repo: &Path,
    group: SuiteGroup,
    cli: &EvalCliOptions,
) -> EvalResult<Vec<DiscoveredEvalCase>> {
    let mut patterns = Vec::new();
    let mut args = cli.discovery_args.iter();
    while let Some(flag) = args.next() {
        let pattern = if let Some(value) = flag.strip_prefix("--testNamePattern=") {
            value
        } else {
            args.next()
                .ok_or_else(|| EvalError::message("Missing name pattern."))?
        };
        patterns.push(
            regex::Regex::new(pattern)
                .map_err(|error| EvalError::message(format!("Invalid name pattern: {error}")))?,
        );
    }
    let mut cases = discover(repo, group)?;
    for file in &cli.requested_files {
        if !cases.iter().any(|case| case.file == *file) {
            return Err(EvalError::message(format!(
                "Unknown suite in selected mode: {file}"
            )));
        }
    }
    cases.retain(|case| {
        (cli.requested_files.is_empty() || cli.requested_files.contains(&case.file))
            && (patterns.is_empty() || patterns.iter().any(|p| p.is_match(&case.full_name)))
    });
    if cases.is_empty() {
        return Err(EvalError::message(
            "No evaluation cases matched the selection.",
        ));
    }
    Ok(cases)
}

pub async fn execute_case(
    repo: &Path,
    pi: &Path,
    task: &EvalTask,
    artifacts: Option<&Path>,
) -> EvalResult<EvalObservation> {
    let controller = pi_rs_coding_agent::utils::abort::AbortController::new();
    let signal = controller.signal();
    let case = run_case(repo, pi, task, Some(&signal));
    tokio::pin!(case);
    let result = match tokio::time::timeout(std::time::Duration::from_secs(300), &mut case).await {
        Ok(result) => result,
        Err(_) => {
            controller.abort(Some("Evaluation exceeded its five minute limit."));
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut case).await {
                Ok(Err(error)) => Err(error),
                _ => Err(crate::harness::HarnessError {
                    message: "Evaluation exceeded its five minute limit.".into(),
                    partial_run: None,
                }),
            }
        }
    };
    let (run, score) = match result {
        Ok((run, passed)) => (run, Some(passed)),
        Err(error) => {
            eprintln!("{} > {}: {}", task.eval_set, task.case_id, error.message);
            let Some(run) = error.partial_run else {
                return Ok(errored_observation(task));
            };
            (*run, None)
        }
    };
    if let (Some(dir), Some(snapshot)) = (
        artifacts,
        run.artifacts
            .get(PI_SESSION_SNAPSHOT_ARTIFACT)
            .and_then(|v| v.as_str()),
    ) {
        crate::docker::write_container_session(dir, snapshot)?;
    }
    native_observation(task, &run, score)
}

pub async fn run_host(
    repo: &Path,
    pi: &Path,
    group: SuiteGroup,
    options: &EvalCliOptions,
    provider: &str,
    model: &str,
) -> EvalResult<bool> {
    let mut failed = false;
    for case in selected_cases(repo, group, options)? {
        for run_number in 1..=options.runs_per_variant {
            let task = EvalTask {
                file: case.file.clone(),
                eval_set: case.eval_set.clone(),
                case_id: case.case_id.clone(),
                full_name: case.full_name.clone(),
                variant: DocumentationVariant::WithDocs,
                model: format!("{provider}/{model}"),
                run_number,
            };
            let result = execute_case(repo, pi, &task, None).await?;
            failed |= result.score != Some(1.0);
            println!("{}", serde_json::to_string(&result)?);
        }
    }
    Ok(failed)
}
