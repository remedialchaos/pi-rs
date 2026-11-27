use super::*;
use crate::harness::broker::Broker;
use pi_rs_coding_agent::{
    core::extensions::loader::{LoadOptions, discover_extension_paths},
    modes::rpc::RpcClient,
    utils::abort::{AbortSignal, race_with_abort},
};
use serde_json::json;
use std::{process::Stdio, time::Duration};
use tempfile::TempDir;
use tokio::process::{Child, Command};

/// Isolated run directories. Keep this handle alive while inspecting model-created
/// resources. Dropping it removes the workspace, private home and saved session.
pub struct EvalWorkspace {
    root: TempDir,
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    sessions: PathBuf,
    identity: Option<SandboxIdentity>,
}

impl EvalWorkspace {
    pub fn new(files: &BTreeMap<String, String>) -> EvalResult<Self> {
        let environment = sandbox_environment();
        let identity = resolve_sandbox_identity(&environment)?;
        let root = tempfile::Builder::new().prefix("pi-eval-").tempdir()?;
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let agent_dir = home.join(".pi/agent");
        let sessions = root.path().join("sessions");
        for dir in [&workspace, &agent_dir, &sessions] {
            std::fs::create_dir_all(dir)?;
        }
        seed_workspace(&workspace, files)?;
        if let Some(id) = identity {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o711))?;
                for dir in [&workspace, &home, &sessions] {
                    assign_owner(dir, id)?;
                }
            }
            #[cfg(not(unix))]
            return Err(EvalError::message(format!(
                "Sandbox uid {} requires Unix",
                id.uid
            )));
        }
        Ok(Self {
            root,
            workspace,
            home,
            agent_dir,
            sessions,
            identity,
        })
    }

    /// Explicit cleanup reports filesystem errors; Drop also cleans up on early return.
    pub fn close(self) -> EvalResult<()> {
        Ok(self.root.close()?)
    }

    /// Configure a native Pi process (including post-run fixture probes) to use the
    /// same workspace and privilege boundary as the original run.
    pub fn command(&self, pi: &Path, fixture: &BTreeMap<String, String>) -> EvalResult<Command> {
        let mut cmd = Command::new(pi);
        cmd.current_dir(&self.workspace).env_clear();
        for key in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "LANG",
            "LC_ALL",
            "TERM",
            "PI_PACKAGE_DIR",
        ] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.envs(fixture)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("TMPDIR", &self.workspace)
            .kill_on_drop(true);
        #[cfg(unix)]
        if let Some(id) = self.identity {
            cmd.uid(id.uid).gid(id.gid);
        }
        Ok(cmd)
    }
}

fn sandbox_environment() -> HashMap<String, String> {
    ["PI_EVAL_SANDBOX_UID", "PI_EVAL_SANDBOX_GID"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_owned(), v)))
        .collect()
}

#[cfg(unix)]
fn assign_owner(path: &Path, id: SandboxIdentity) -> EvalResult<()> {
    if path.symlink_metadata()?.is_dir() {
        for entry in std::fs::read_dir(path)? {
            assign_owner(&entry?.path(), id)?;
        }
    }
    Ok(std::os::unix::fs::lchown(path, Some(id.uid), Some(id.gid))?)
}

pub fn create_pi_documentation_eval_harness(
    mut options: PiCodingAgentHarnessOptions,
) -> EvalResult<PiCodingAgentHarness> {
    if std::env::var("PI_EVAL_CONTAINER").as_deref() != Ok("1")
        || resolve_sandbox_identity(&sandbox_environment())?.is_none()
    {
        return Err(EvalError::message(
            "Documentation evals must run in the isolated container sandbox.",
        ));
    }
    let variant = resolve_documentation_variant(std::env::var("PI_EVAL_VARIANT").ok().as_deref())?;
    options.name = Some(variant.as_str().into());
    options.expected_pi_documentation = Some(variant == DocumentationVariant::WithDocs);
    options.tools.get_or_insert_with(|| {
        DOCUMENTATION_EVAL_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    Ok(PiCodingAgentHarness::new(options))
}

fn check_extensions(workspace: &EvalWorkspace, expected: &[PathBuf]) -> EvalResult<()> {
    let mut opts = LoadOptions::new(&workspace.workspace, &workspace.agent_dir);
    opts.project_trusted = true;
    let paths: Vec<_> = discover_extension_paths(&opts)
        .into_iter()
        .filter(|p| !expected.iter().any(|e| e == Path::new(p)))
        .collect();
    if !paths.is_empty() {
        return Err(EvalError::message(format!(
            "Isolated eval loaded unexpected extensions: {}",
            paths.join(", ")
        )));
    }
    Ok(())
}

struct Agent {
    child: Child,
    rpc: RpcClient,
}

impl Agent {
    fn start(
        pi: &Path,
        workspace: &EvalWorkspace,
        options: &PiCodingAgentHarnessOptions,
        model: &Model,
        session_id: &str,
        initial: bool,
    ) -> EvalResult<Self> {
        let mut cmd = workspace.command(pi, &options.environment)?;
        cmd.args([
            "--mode",
            "rpc",
            "--provider",
            "pi-eval",
            "--model",
            &model.id,
            "--thinking",
            "off",
            "--approve",
            "--session-id",
            session_id,
            "--session-dir",
        ])
        .arg(&workspace.sessions);
        if initial {
            cmd.arg("--no-extensions");
        }
        for path in &options.extensions {
            cmd.arg("--extension").arg(path);
        }
        if let Some(tools) = &options.tools {
            if tools.is_empty() {
                cmd.arg("--no-tools");
            } else {
                cmd.arg("--tools").arg(tools.join(","));
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| EvalError::message("Missing Pi RPC output"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| EvalError::message("Missing Pi RPC input"))?;
        Ok(Self {
            child,
            rpc: RpcClient::from_stream(stdout, stdin),
        })
    }

    async fn close(self) -> EvalResult<()> {
        let Self { mut child, rpc } = self;
        drop(rpc);
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(status) => {
                status?;
            }
            Err(_) => child.kill().await?,
        }
        Ok(())
    }
}

fn empty_result(model: &Model) -> SimpleHarnessResult<Value> {
    SimpleHarnessResult {
        output: Value::Null,
        events: Vec::new(),
        metadata: Map::new(),
        errors: Vec::new(),
        artifacts: BTreeMap::new(),
        usage: UsageSummary {
            provider: Some(model.provider.clone()),
            model: Some(model.id.clone()),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            tool_calls: None,
            metadata: Map::new(),
        },
        timings: TimingSummary { total_ms: 0.0 },
    }
}

impl From<EvalError> for HarnessError {
    fn from(e: EvalError) -> Self {
        Self {
            message: e.to_string(),
            partial_run: None,
        }
    }
}

/// Run the installed Pi binary, preserving the workspace for the suite's checks.
/// Provider authorization stays in the evaluator; only the run-local broker
/// token is available to model-created resources.
pub async fn run_pi_coding_agent(
    pi: &Path,
    workspace: &EvalWorkspace,
    input: &PiCodingAgentInput,
    options: &PiCodingAgentHarnessOptions,
    signal: Option<&AbortSignal>,
) -> Result<SimpleHarnessResult<Value>, HarnessError> {
    let started = Instant::now();
    if let Some(s) = signal.filter(|s| s.is_aborted()) {
        return Err(EvalError::message(s.reason().unwrap_or_else(|| "Aborted".into())).into());
    }
    check_extensions(workspace, &options.extensions)?;
    let selection = resolve_model_selection(
        options.model.as_ref(),
        std::env::var("PI_PROVIDER").ok().as_deref(),
        std::env::var("PI_MODEL").ok().as_deref(),
    )?;
    let broker = Broker::start(&selection, options.expected_pi_documentation).await?;
    let config = workspace.agent_dir.join("models.json");
    let mut definition = if config.try_exists().map_err(EvalError::from)? {
        serde_json::from_slice::<Value>(&std::fs::read(&config).map_err(EvalError::from)?)
            .map_err(EvalError::from)?
    } else {
        json!({"providers":{}})
    };
    let providers = definition
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| EvalError::message("Invalid eval models.json providers"))?;
    if providers.contains_key("pi-eval") {
        return Err(
            EvalError::message("Reserved eval broker provider is already configured").into(),
        );
    }
    providers.insert("pi-eval".into(), broker.config.clone());
    write_private(
        &config,
        &serde_json::to_vec(&definition).map_err(EvalError::from)?,
    )?;
    #[cfg(unix)]
    if let Some(id) = workspace.identity {
        assign_owner(&config, id)?;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut result = empty_result(&broker.model);
    result.artifacts.insert("runId".into(), json!(id));
    let mut agent = None;
    let mut session_file = None;
    let steps = match input {
        PiCodingAgentInput::Text(text) => vec![HarnessStep::Prompt(text.clone())],
        PiCodingAgentInput::Steps(steps) => steps.clone(),
    };
    let run = async {
        let mut initial = true;
        for step in steps {
            if let HarnessStep::Reload = step {
                if let Some(a) = agent.take() {
                    Agent::close(a).await?;
                }
                initial = false;
                continue;
            }
            let HarnessStep::Prompt(prompt) = step else {
                continue;
            };
            if agent.is_none() {
                agent = Some(Agent::start(
                    pi,
                    workspace,
                    options,
                    &broker.model,
                    &id,
                    initial,
                )?);
            }
            let a = agent
                .as_ref()
                .ok_or_else(|| EvalError::message("Missing Pi process"))?;
            let state = a.rpc.get_state().await?;
            session_file = state.session_file.map(PathBuf::from);
            let previous = a.rpc.get_messages().await?.len();
            a.rpc
                .prompt_and_wait(prompt, Vec::new(), Duration::from_secs(3600))
                .await?;
            let messages = a.rpc.get_messages().await?;
            let new: Vec<_> = messages
                .iter()
                .skip(previous)
                .filter_map(|m| m.to_llm())
                .collect();
            result.events = to_transcript_events(messages.iter().filter_map(|m| m.to_llm()));
            let stats = a.rpc.get_session_stats().await?;
            result.usage.input_tokens = Some(stats.tokens.input);
            result.usage.output_tokens = Some(stats.tokens.output);
            result.usage.total_tokens = Some(stats.tokens.total);
            result.usage.tool_calls = Some(stats.tool_calls as f64);
            result.usage.metadata.insert(
                "cacheReadTokens".into(),
                json_number(stats.tokens.cache_read),
            );
            result.usage.metadata.insert(
                "cacheWriteTokens".into(),
                json_number(stats.tokens.cache_write),
            );
            if has_pricing(&broker.model) {
                result
                    .usage
                    .metadata
                    .insert("estimatedCostUsd".into(), json_number(stats.cost));
            }
            result.output = json!(assistant_response(&new)?);
        }
        if result.output.is_null() {
            return Err(EvalError::message(
                "Pi eval input must include at least one prompt step.",
            ));
        }
        Ok(())
    };
    let mut errors = Vec::new();
    match race_with_abort(run, signal.cloned()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => errors.push(e.to_string()),
        Err(e) => {
            errors.push(signal.and_then(AbortSignal::reason).unwrap_or(e));
            if let Some(a) = &agent {
                let _ = a.rpc.abort().await;
            }
        }
    }
    if let Some(a) = agent
        && let Err(e) = a.close().await
    {
        errors.push(e.to_string());
    }
    let snapshot = match session_file.as_deref() {
        Some(path) if path.parent() != Some(workspace.sessions.as_path()) || path.is_symlink() => {
            Err(EvalError::message("Unexpected Pi session file path."))
        }
        path => snapshot(path),
    };
    if let Some(hash) = broker
        .prompt_hash
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        result
            .metadata
            .insert("systemPromptSha256".into(), json!(hash));
    }
    finish(result, started, errors, snapshot)
}

pub(super) fn finish(
    mut result: SimpleHarnessResult<Value>,
    started: Instant,
    mut errors: Vec<String>,
    snapshot: EvalResult<String>,
) -> Result<SimpleHarnessResult<Value>, HarnessError> {
    match snapshot {
        Ok(text) => {
            result
                .artifacts
                .insert(PI_SESSION_SNAPSHOT_ARTIFACT.into(), json!(text));
        }
        Err(e) => errors.push(e.to_string()),
    }
    result.timings.total_ms = started.elapsed().as_secs_f64() * 1000.0;
    if errors.is_empty() {
        return Ok(result);
    }
    result.errors = errors.iter().map(|e| json!({"message":e})).collect();
    Err(HarnessError {
        message: errors.join(" "),
        partial_run: Some(Box::new(result)),
    })
}
