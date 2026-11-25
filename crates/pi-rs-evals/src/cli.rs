//! CLI parsing and the documentation-eval orchestration (port of `cli.ts`).

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::docker::{
    BuiltImages, DockerContext, SystemCommandRunner, build_images, create_docker_context,
    discover_cases as docker_discover_cases, require_eval_auth_file, resolve_agent_dir,
    run_task as docker_run_task,
};
use crate::error::{EvalError, EvalResult};
use crate::plan::{
    DOCUMENTATION_VARIANTS, DiscoveredEvalCase, DocumentationVariant, EvalTask, create_task_plan,
    parse_discovered_cases,
};
use crate::report::{
    EvalComparisonReport, EvalObservation, errored_observation, format_eval_comparison_report,
    read_task_observation, summarize_eval_observations,
};

const DISCOVERY_PREFIX: &str = "/repo/packages/evals/";

/// Parsed documentation eval CLI options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCliOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub runs_per_variant: u64,
    pub requested_files: Vec<String>,
    pub discovery_args: Vec<String>,
}

fn parse_js_safe_integer(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()? as f64
    } else {
        text.parse::<f64>().ok()?
    };
    if value.is_finite() && value.fract() == 0.0 && (1.0..=9_007_199_254_740_991.0).contains(&value)
    {
        Some(value as u64)
    } else {
        None
    }
}

/// Parse eval CLI arguments with an injectable environment lookup.
pub fn parse_eval_cli(
    args: &[String],
    environment: &dyn Fn(&str) -> Option<String>,
) -> EvalResult<EvalCliOptions> {
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut runs_text: Option<String> = None;
    let mut cli_selected_model = false;
    let mut requested_files = Vec::new();
    let mut discovery_args = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument.ends_with(".docs.eval.ts") {
            requested_files.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == "-t" || argument == "--testNamePattern" {
            let Some(value) = args.get(index + 1).filter(|value| !value.is_empty()) else {
                return Err(EvalError::message(format!("Missing value for {argument}.")));
            };
            discovery_args.push(argument.clone());
            discovery_args.push(value.clone());
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--testNamePattern=") {
            if value.is_empty() {
                return Err(EvalError::message("Missing value for --testNamePattern."));
            }
            discovery_args.push(argument.clone());
            index += 1;
            continue;
        }

        let equals = argument.find('=');
        let name = match equals {
            Some(position) => &argument[..position],
            None => argument.as_str(),
        };
        if matches!(name, "--provider" | "--model" | "--runs-per-variant") {
            let (value, inline) = match equals {
                Some(position) => (argument[position + 1..].to_owned(), true),
                None => (
                    args.get(index + 1)
                        .filter(|value| !value.is_empty() && !value.starts_with('-'))
                        .cloned()
                        .ok_or_else(|| EvalError::message(format!("Missing value for {name}.")))?,
                    false,
                ),
            };
            if value.is_empty() || (!inline && value.starts_with('-')) {
                return Err(EvalError::message(format!("Missing value for {name}.")));
            }
            match name {
                "--provider" => provider = Some(value),
                "--model" => model = Some(value),
                _ => runs_text = Some(value),
            }
            if name != "--runs-per-variant" {
                cli_selected_model = true;
            }
            index += if inline { 1 } else { 2 };
            continue;
        }
        return Err(EvalError::message(format!(
            "Unsupported eval argument: {argument}"
        )));
    }

    let trim_option = |value: Option<String>| -> Option<String> {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    provider = trim_option(provider);
    model = trim_option(model);
    if cli_selected_model {
        if provider.is_none() || model.is_none() {
            return Err(EvalError::message(
                "CLI model selection requires both --provider and --model.",
            ));
        }
    } else {
        provider = trim_option(environment("PI_PROVIDER"));
        model = trim_option(environment("PI_MODEL"));
        if provider.is_some() != model.is_some() {
            return Err(EvalError::message(
                "Set both PI_PROVIDER and PI_MODEL, or neither.",
            ));
        }
    }

    let configured_runs = match runs_text {
        Some(value) => Some(value),
        None => environment("PI_EVAL_RUNS_PER_VARIANT"),
    }
    .map(|value| value.trim().to_owned())
    .filter(|value| !value.is_empty());
    let runs_per_variant = match configured_runs {
        Some(value) => parse_js_safe_integer(&value)
            .ok_or_else(|| EvalError::message("Runs per variant must be a positive integer."))?,
        None => 1,
    };

    Ok(EvalCliOptions {
        provider,
        model,
        runs_per_variant,
        requested_files,
        discovery_args,
    })
}

/// Lexically relative path from `base` to `path`, or `None` when unrelated.
fn relative_path(base: &Path, path: &Path) -> Option<PathBuf> {
    let base: Vec<_> = base.components().collect();
    let path: Vec<_> = path.components().collect();
    let common = base
        .iter()
        .zip(path.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 && base.first() != path.first() {
        return None;
    }
    let mut result = PathBuf::new();
    for _ in common..base.len() {
        result.push("..");
    }
    for component in &path[common..] {
        result.push(component.as_os_str());
    }
    Some(result)
}

/// Convert a package-relative eval file to the container path used by Docker.
pub fn container_path(package_root: &Path, path: &str) -> EvalResult<String> {
    let package_root = crate::docker::lexical_resolve(package_root);
    let absolute = if Path::new(path).is_absolute() {
        crate::docker::lexical_resolve(Path::new(path))
    } else {
        crate::docker::lexical_resolve(&package_root.join(path))
    };
    let relative = relative_path(&package_root, &absolute).ok_or_else(|| {
        EvalError::message(format!(
            "Eval file must be inside {}: {path}",
            package_root.display()
        ))
    })?;
    let relative_text = relative.to_string_lossy();
    if relative_text == ".."
        || relative_text.starts_with("../")
        || relative_text.starts_with("..\\")
    {
        return Err(EvalError::message(format!(
            "Eval file must be inside {}: {path}",
            package_root.display()
        )));
    }
    Ok(relative_text.replace('\\', "/"))
}

/// Strip the container package prefix from a discovered eval file path.
pub fn normalize_discovered_file(path: &str) -> EvalResult<String> {
    path.strip_prefix(DISCOVERY_PREFIX)
        .map(str::to_owned)
        .ok_or_else(|| {
            EvalError::message(format!(
                "Discovered eval path is outside the container package: {path}"
            ))
        })
}

/// Assert that two documentation variants discovered identical eval cases.
pub fn compare_discovery(
    left: &[DiscoveredEvalCase],
    right: &[DiscoveredEvalCase],
) -> EvalResult<()> {
    let identities = |cases: &[DiscoveredEvalCase]| -> EvalResult<Vec<String>> {
        let mut values: Vec<String> = cases
            .iter()
            .map(|case| {
                Ok(
                    serde_json::json!([case.full_name, normalize_discovered_file(&case.file)?])
                        .to_string(),
                )
            })
            .collect::<EvalResult<Vec<_>>>()?;
        values.sort();
        Ok(values)
    };
    if identities(left)? != identities(right)? {
        return Err(EvalError::message(
            "Documentation variants discovered different eval cases.",
        ));
    }
    Ok(())
}

/// Format a timestamp like JavaScript `Date.prototype.toISOString`.
pub fn iso_timestamp_utc(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Build the artifact run identifier.
pub fn artifact_run_id(timestamp: &str, uuid: &str) -> String {
    format!("{}_{}", timestamp.replace(':', "-"), uuid)
}

/// Select eval files from explicit CLI arguments or the package glob.
pub fn select_eval_files(
    package_root: &Path,
    requested_files: &[String],
) -> EvalResult<Vec<String>> {
    let files = if !requested_files.is_empty() {
        requested_files.to_vec()
    } else {
        let pattern = Glob::new("evals/**/*.docs.eval.ts")
            .map_err(|error| EvalError::message(format!("Invalid eval glob: {error}")))?;
        let mut builder = GlobSetBuilder::new();
        builder.add(pattern);
        let set = builder
            .build()
            .map_err(|error| EvalError::message(format!("Invalid eval glob: {error}")))?;
        let mut files = Vec::new();
        let mut stack = vec![package_root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(relative) = path.strip_prefix(package_root)
                    && set.is_match(relative)
                {
                    files.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        files
    };
    let mut files: Vec<String> = files
        .iter()
        .map(|file| container_path(package_root, file))
        .collect::<EvalResult<_>>()?;
    files.sort();
    Ok(files)
}

#[derive(Serialize)]
struct ProtocolCase {
    #[serde(rename = "evalSet")]
    eval_set: String,
    #[serde(rename = "caseId")]
    case_id: String,
    file: String,
}

#[derive(Serialize)]
struct Protocol {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    model: String,
    #[serde(rename = "runsPerVariant")]
    runs_per_variant: u64,
    images: BuiltImages,
    files: Vec<String>,
    cases: Vec<ProtocolCase>,
    tasks: Vec<EvalTask>,
}

/// Abstraction over the docker-dependent steps of an eval run.
pub trait EvalDockerOps {
    fn build_images(&self) -> EvalResult<BuiltImages>;
    fn discover_cases(
        &self,
        variant: DocumentationVariant,
        files: &[String],
        args: &[String],
    ) -> EvalResult<Vec<DiscoveredEvalCase>>;
    fn run_task(&self, task: &EvalTask) -> EvalResult<Option<PathBuf>>;
    fn artifact_uid_gid(&self) -> Option<(u32, u32)>;
}

/// Summary of a completed documentation eval run.
pub struct RunSummary {
    pub artifact_directory: PathBuf,
    pub report: EvalComparisonReport,
    pub report_text: String,
}

#[cfg(unix)]
fn create_artifact_directory(path: &Path) -> EvalResult<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_artifact_directory(path: &Path) -> EvalResult<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn write_json_pretty(path: &Path, value: &impl Serialize) -> EvalResult<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Execute the full documentation eval comparison flow.
pub fn execute_eval_run(
    cli: &EvalCliOptions,
    package_root: &Path,
    artifact_directory: &Path,
    ops: &dyn EvalDockerOps,
) -> EvalResult<RunSummary> {
    let (Some(provider), Some(model)) = (&cli.provider, &cli.model) else {
        return Err(EvalError::message(
            "Set PI_PROVIDER and PI_MODEL, or pass --provider and --model.",
        ));
    };
    let model_identity = format!("{provider}/{model}");
    let files = select_eval_files(package_root, &cli.requested_files)?;
    if files.is_empty() {
        return Err(EvalError::message(
            "No documentation eval files were selected.",
        ));
    }
    for file in &files {
        if !file.ends_with(".docs.eval.ts") {
            return Err(EvalError::message(format!(
                "Documentation runner cannot execute non-doc eval: {file}"
            )));
        }
    }

    create_artifact_directory(artifact_directory)?;
    let images = ops.build_images()?;
    if images.with_docs.id == images.without_docs.id {
        return Err(EvalError::message(
            "Documentation variants resolved to the same image.",
        ));
    }

    let mut discoveries = Vec::new();
    for variant in DOCUMENTATION_VARIANTS {
        let cases = ops.discover_cases(variant, &files, &cli.discovery_args)?;
        discoveries.push(cases);
    }
    compare_discovery(&discoveries[0], &discoveries[1])?;
    let cases: Vec<DiscoveredEvalCase> = discoveries[0]
        .iter()
        .map(|case| {
            Ok(DiscoveredEvalCase {
                file: normalize_discovered_file(&case.file)?,
                full_name: case.full_name.clone(),
                eval_set: case.eval_set.clone(),
                case_id: case.case_id.clone(),
            })
        })
        .collect::<EvalResult<_>>()?;
    if cases.is_empty() {
        return Err(EvalError::message(
            "No documentation eval cases matched the selection.",
        ));
    }
    let tasks = create_task_plan(&cases, &model_identity, cli.runs_per_variant)?;

    let protocol = Protocol {
        schema_version: 1,
        model: model_identity,
        runs_per_variant: cli.runs_per_variant,
        images: images.clone(),
        files: files.clone(),
        cases: cases
            .iter()
            .map(|case| ProtocolCase {
                eval_set: case.eval_set.clone(),
                case_id: case.case_id.clone(),
                file: case.file.clone(),
            })
            .collect(),
        tasks: tasks.clone(),
    };
    let protocol_text = serde_json::to_string(&protocol)?;
    let protocol_digest = sha256_hex(protocol_text.as_bytes());
    let mut protocol_with_digest = serde_json::to_value(&protocol)?;
    if let Some(object) = protocol_with_digest.as_object_mut() {
        object.insert(
            "protocolDigest".to_owned(),
            serde_json::Value::String(protocol_digest.clone()),
        );
    }
    write_json_pretty(
        &artifact_directory.join("protocol.json"),
        &protocol_with_digest,
    )?;
    write_json_pretty(&artifact_directory.join("expected-runs.json"), &tasks)?;

    let mut observations: Vec<EvalObservation> = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        println!(
            "\n[{}/{}] {} > {} | {} | {} | run {}",
            index + 1,
            tasks.len(),
            task.eval_set,
            task.case_id,
            task.variant,
            task.model,
            task.run_number
        );
        let observation = match ops.run_task(task)? {
            Some(report_path) => read_task_observation(task, &report_path, artifact_directory)?,
            None => errored_observation(task),
        };
        observations.push(observation);
        let mut observation_lines: Vec<String> = observations
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?;
        observation_lines.push(String::new());
        std::fs::write(
            artifact_directory.join("observations.jsonl"),
            observation_lines.join("\n"),
        )?;
    }

    let expected_runs: Vec<crate::report::EvalRunIdentity> = tasks
        .iter()
        .map(|task| crate::report::EvalRunIdentity {
            eval_set: task.eval_set.clone(),
            case_id: task.case_id.clone(),
            variant: task.variant,
            model: task.model.clone(),
            run_number: task.run_number,
        })
        .collect();
    let report = summarize_eval_observations(&protocol_digest, &expected_runs, &observations);
    let report_text = format_eval_comparison_report(&report);
    write_json_pretty(&artifact_directory.join("report.json"), &report)?;
    std::fs::write(
        artifact_directory.join("report.txt"),
        format!("{report_text}\n"),
    )?;
    println!("\n{report_text}");
    println!("\nArtifacts: {}", artifact_directory.display());
    Ok(RunSummary {
        artifact_directory: artifact_directory.to_path_buf(),
        report,
        report_text,
    })
}

/// Real docker-backed operations used by the `pi-evals` binary.
pub struct SystemEvalDockerOps {
    context: DockerContext,
    runner: SystemCommandRunner,
    built_images: std::cell::RefCell<Option<BuiltImages>>,
}

impl SystemEvalDockerOps {
    pub fn new(package_root: &Path, context: DockerContext) -> Self {
        Self {
            context,
            runner: SystemCommandRunner::new(package_root.to_path_buf()),
            built_images: std::cell::RefCell::new(None),
        }
    }

    /// Construct docker ops using an explicit OCI container runtime binary or path.
    pub fn with_container_runtime(
        package_root: &Path,
        context: DockerContext,
        runtime: impl Into<String>,
    ) -> Self {
        Self {
            context,
            runner: SystemCommandRunner::with_container_runtime(
                package_root.to_path_buf(),
                runtime,
            ),
            built_images: std::cell::RefCell::new(None),
        }
    }

    pub fn context(&self) -> &DockerContext {
        &self.context
    }

    fn current_context(&self) -> DockerContext {
        let mut context = self.context.clone();
        if let Some(images) = self.built_images.borrow().as_ref() {
            context.images = images.clone();
        }
        context
    }
}

impl EvalDockerOps for SystemEvalDockerOps {
    fn build_images(&self) -> EvalResult<BuiltImages> {
        let package_root = crate::docker::lexical_resolve(self.runner.cwd.as_path());
        let repository_root = crate::docker::lexical_resolve(&package_root.join("../.."));
        let images = build_images(&repository_root, &package_root, &self.runner)?;
        *self.built_images.borrow_mut() = Some(images.clone());
        Ok(images)
    }

    fn discover_cases(
        &self,
        variant: DocumentationVariant,
        files: &[String],
        args: &[String],
    ) -> EvalResult<Vec<DiscoveredEvalCase>> {
        let context = self.current_context();
        let path = docker_discover_cases(
            &context,
            variant,
            files,
            args,
            &self.runner,
            self.artifact_uid_gid(),
        )?;
        let text = std::fs::read_to_string(path)?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        parse_discovered_cases(&value)
    }

    fn run_task(&self, task: &EvalTask) -> EvalResult<Option<PathBuf>> {
        let context = self.current_context();
        docker_run_task(&context, task, &self.runner, self.artifact_uid_gid())
    }

    fn artifact_uid_gid(&self) -> Option<(u32, u32)> {
        crate::docker::process_uid_gid()
    }
}

/// Construct the real docker context for a binary invocation.
pub fn system_docker_context(
    artifact_directory: PathBuf,
    provider: &str,
    model: &str,
    runs_per_variant: u64,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> EvalResult<(DockerContext, PathBuf)> {
    let agent_dir = resolve_agent_dir(env("PI_CODING_AGENT_DIR").as_deref(), home);
    let auth_path = require_eval_auth_file(provider, &agent_dir)?;
    // Images are populated by `build_images` at run time; empty IDs are placeholders.
    let images = BuiltImages {
        without_docs: crate::docker::BuiltImage {
            name: String::new(),
            id: String::new(),
        },
        with_docs: crate::docker::BuiltImage {
            name: String::new(),
            id: String::new(),
        },
    };
    let context = create_docker_context(
        images,
        artifact_directory,
        auth_path.clone(),
        provider,
        model,
        runs_per_variant,
    );
    Ok((context, agent_dir))
}

/// Helper to format an environment map for tests.
pub fn map_environment(
    map: &std::collections::HashMap<String, String>,
) -> impl Fn(&str) -> Option<String> + '_ {
    move |key| map.get(key).cloned()
}

#[cfg(test)]
mod tests {
    use super::parse_js_safe_integer;

    #[test]
    fn js_integer_parser_matches_expected_cases() {
        assert_eq!(parse_js_safe_integer("2"), Some(2));
        assert_eq!(parse_js_safe_integer("1e3"), Some(1000));
        assert_eq!(parse_js_safe_integer("0x10"), Some(16));
        assert_eq!(parse_js_safe_integer("0"), None);
        assert_eq!(parse_js_safe_integer("1.5"), None);
        assert_eq!(parse_js_safe_integer("abc"), None);
    }
}
