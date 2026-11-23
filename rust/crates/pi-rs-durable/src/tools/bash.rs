use std::collections::BTreeMap;

use async_trait::async_trait;
use pi_rs_chord::{
    context::Context,
    output::{OutputBuffer, OutputLimits, OutputSkip, Retain},
};
use pi_rs_schema::{Annotations, NumOptions, Properties, bool as bool_schema, num, obj, opt};
use serde::{Deserialize, Serialize};

use crate::{
    env::{
        ExecutionEnv, ExecutionError, ExecutionErrorCode, ShellCommand, ShellExecOptions,
        ShellOutputInfo, ShellOutputSkip, ShellOutputWindow, ShellSpillOptions,
    },
    truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES},
};

use super::{
    DiagnosticSeverity, ToolDiagnostic, ToolError, ToolResult, ToolSpec, described_num,
    described_str, require_env,
};

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;
const UTF8_OUTPUT: &str = "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BashToolInput {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

pub type PowerShellToolInput = BashToolInput;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellStructuredOutput {
    pub output: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub exit_code: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BashExecution {
    pub command: String,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
}

#[async_trait]
pub trait BashPrepare: Send + Sync {
    async fn prepare(
        &self,
        execution: &mut BashExecution,
        env: &dyn ExecutionEnv,
        output: &mut dyn ShellOutput,
        context: &Context,
    ) -> Result<(), ToolError>;
}

#[derive(Clone, Copy, Default)]
pub struct BashToolOptions<'a> {
    pub command_prefix: Option<&'a str>,
    pub prepare: Option<&'a dyn BashPrepare>,
}

#[derive(Clone, Copy, Default)]
pub struct PowerShellToolOptions<'a> {
    pub command_prefix: Option<&'a str>,
    pub prepare: Option<&'a dyn BashPrepare>,
    pub programs: Option<&'a [&'a str]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedShellOutput {
    pub text: String,
    pub truncated: bool,
}

/// Per-call output hook. A host can publish streaming output, keep its own tail, and record spill
/// diagnostics even when the shell itself returns an error.
pub trait ShellOutput: Send {
    fn window(&self) -> Option<ShellOutputWindow> {
        None
    }
    fn output(
        &mut self,
        text: &str,
        skipped: Option<ShellOutputSkip>,
    ) -> Result<(), ExecutionError>;
    fn retained_output(&mut self) -> RetainedShellOutput;
    fn diagnostic(&mut self, diagnostic: ToolDiagnostic);
}

/// Standalone consumer with the same bounded tail as the coding tools.
pub struct TailShellOutput {
    buffer: OutputBuffer,
    window: Option<ShellOutputWindow>,
    diagnostics: Vec<ToolDiagnostic>,
}

impl Default for TailShellOutput {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TailShellOutput {
    pub fn new(window: Option<ShellOutputWindow>) -> Self {
        let (max_bytes, max_lines) = window
            .map_or((DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES), |window| {
                (window.max_bytes, window.max_lines)
            });
        Self {
            buffer: OutputBuffer::new(OutputLimits {
                max_bytes,
                max_lines,
                retain: Retain::Tail,
            }),
            window,
            diagnostics: Vec::new(),
        }
    }

    pub fn diagnostics(&self) -> &[ToolDiagnostic] {
        &self.diagnostics
    }
}

impl ShellOutput for TailShellOutput {
    fn window(&self) -> Option<ShellOutputWindow> {
        self.window
    }

    fn output(
        &mut self,
        text: &str,
        skipped: Option<ShellOutputSkip>,
    ) -> Result<(), ExecutionError> {
        let skipped = skipped.map(|skip| OutputSkip {
            bytes: skip.bytes,
            newlines: skip.newlines,
            ends_with_newline: skip.ends_with_newline,
        });
        // The environment already decoded this as text, so preserve an initial literal BOM.
        self.buffer
            .push_text(&text.into(), skipped)
            .map_err(|error| {
                ExecutionError::new(ExecutionErrorCode::CallbackError, error.to_string())
            })?;
        Ok(())
    }

    fn retained_output(&mut self) -> RetainedShellOutput {
        let output = self.buffer.snapshot();
        RetainedShellOutput {
            text: String::from_utf16_lossy(&output.text.0),
            truncated: output.dropped_bytes != 0 || output.dropped_lines != 0,
        }
    }

    fn diagnostic(&mut self, diagnostic: ToolDiagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

pub struct BashTool<'a> {
    spec: ToolSpec,
    options: BashToolOptions<'a>,
}
pub struct PowerShellTool<'a> {
    spec: ToolSpec,
    options: PowerShellToolOptions<'a>,
}

fn shell_spec(name: &'static str, shell: &str) -> ToolSpec {
    let annotations = Annotations::new();
    let display = if name == "bash" { name } else { shell };
    ToolSpec {
        name,
        description: format!(
            "Execute a {display} command in the current working directory. Returns combined stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            DEFAULT_MAX_BYTES / 1024
        ),
        parameters: obj(
            &Properties::new()
                .with(
                    "command",
                    &described_str(&format!("{shell} command to execute")),
                )
                .with_optional(
                    "timeout",
                    &opt(&described_num(
                        "Timeout in seconds (optional, no default timeout)",
                    )),
                ),
            &annotations,
            true,
        ),
        structured_output_schema: Some(obj(
            &Properties::new()
                .with(
                    "output",
                    &described_str(
                        "Combined stdout and stderr: the retained tail, as the model sees it",
                    ),
                )
                .with(
                    "truncated",
                    &bool_schema(
                        &Annotations::new().description("Whether earlier output was dropped"),
                    ),
                )
                .with_optional(
                    "fullOutputPath",
                    &opt(&described_str(
                        "File with the complete output, when it was spilled",
                    )),
                )
                .with("exitCode", &num(&NumOptions::new())),
            &annotations,
            true,
        )),
        output_limits: Some(OutputLimits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            retain: Retain::Tail,
        }),
    }
}

pub fn create_bash_tool(options: BashToolOptions<'_>) -> BashTool<'_> {
    BashTool {
        spec: shell_spec("bash", "Bash"),
        options,
    }
}

pub fn create_powershell_tool(options: PowerShellToolOptions<'_>) -> PowerShellTool<'_> {
    PowerShellTool {
        spec: shell_spec("powershell", "PowerShell"),
        options,
    }
}

fn validate_timeout(timeout: Option<f64>) -> Result<(), ToolError> {
    if let Some(timeout) = timeout {
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err(ToolError::Message(
                "Invalid timeout: must be a finite number of seconds".into(),
            ));
        }
        if timeout > MAX_TIMEOUT_SECONDS {
            return Err(ToolError::Message(format!(
                "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
            )));
        }
    }
    Ok(())
}

async fn prepare_execution(
    command: String,
    prefix: Option<&str>,
    prepare: Option<&dyn BashPrepare>,
    env: &dyn ExecutionEnv,
    output: &mut dyn ShellOutput,
    context: &Context,
) -> Result<BashExecution, ToolError> {
    let mut execution = BashExecution {
        command: match prefix.filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command,
        },
        cwd: env.cwd().to_owned(),
        env: BTreeMap::new(),
        inherit_env: true,
    };
    if let Some(prepare) = prepare {
        prepare
            .prepare(&mut execution, env, output, context)
            .await?;
    }
    Ok(execution)
}

async fn run_commands<'a>(
    commands: impl Iterator<Item = ShellCommand<'a>>,
    execution: &BashExecution,
    timeout: Option<f64>,
    env: &dyn ExecutionEnv,
    output: &mut dyn ShellOutput,
    context: &Context,
) -> Result<ToolResult, ToolError> {
    let mut result = None;
    let window = output.window();
    for command in commands {
        let mut forward =
            |text: &str, _: &Context, info: ShellOutputInfo| output.output(text, info.skipped);
        let current = env
            .exec(
                command,
                Some(ShellExecOptions {
                    cwd: Some(&execution.cwd),
                    env: Some(&execution.env),
                    inherit_env: Some(execution.inherit_env),
                    timeout,
                    on_output: Some(&mut forward),
                    window,
                    spill: Some(ShellSpillOptions {
                        after_bytes: DEFAULT_MAX_BYTES,
                        after_lines: DEFAULT_MAX_LINES,
                    }),
                }),
                context,
            )
            .await;
        let retry = current
            .as_ref()
            .is_err_and(|err| err.code == ExecutionErrorCode::SpawnError);
        result = Some(current);
        if !retry {
            break;
        }
    }
    let result = result.ok_or_else(|| ToolError::Message("No command to run".into()))?;
    let spill = match &result {
        Ok(result) => &result.spill_path,
        Err(error) => &error.spill_path,
    };
    if let Some(path) = spill {
        output.diagnostic(ToolDiagnostic::new(
            DiagnosticSeverity::Info,
            Some("full_output"),
            format!("Full output: {path}"),
        ));
    }
    let result = result.map_err(|error| match error.code {
        ExecutionErrorCode::Timeout => ToolError::Message(if let Some(timeout) = timeout {
            format!("Command timed out after {timeout} seconds")
        } else {
            "Command timed out after undefined seconds".into()
        }),
        ExecutionErrorCode::Aborted if !context.is_aborted() => {
            ToolError::Message("Command aborted".into())
        }
        _ => ToolError::Execution(error),
    })?;
    let retained = output.retained_output();
    let structured = ShellStructuredOutput {
        output: retained.text,
        truncated: retained.truncated,
        full_output_path: result.spill_path,
        exit_code: result.exit_code,
    };
    let mut response = ToolResult {
        structured_output: Some(serde_json::to_value(structured)?),
        ..ToolResult::default()
    };
    if result.exit_code != 0 {
        response.is_error = Some(true);
        response.diagnostics = Some(vec![ToolDiagnostic::new(
            DiagnosticSeverity::Error,
            Some("exit_code"),
            format!("Command exited with code {}", result.exit_code),
        )]);
    }
    Ok(response)
}

impl BashTool<'_> {
    pub fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub async fn execute(
        &self,
        args: BashToolInput,
        env: Option<&dyn ExecutionEnv>,
        output: &mut dyn ShellOutput,
        context: &Context,
    ) -> Result<ToolResult, ToolError> {
        validate_timeout(args.timeout)?;
        let env = require_env(env)?;
        let execution = prepare_execution(
            args.command,
            self.options.command_prefix,
            self.options.prepare,
            env,
            output,
            context,
        )
        .await?;
        run_commands(
            std::iter::once(ShellCommand::Script(&execution.command)),
            &execution,
            args.timeout,
            env,
            output,
            context,
        )
        .await
    }
}

impl PowerShellTool<'_> {
    pub fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub async fn execute(
        &self,
        args: PowerShellToolInput,
        env: Option<&dyn ExecutionEnv>,
        output: &mut dyn ShellOutput,
        context: &Context,
    ) -> Result<ToolResult, ToolError> {
        validate_timeout(args.timeout)?;
        let env = require_env(env)?;
        let execution = prepare_execution(
            args.command,
            self.options.command_prefix,
            self.options.prepare,
            env,
            output,
            context,
        )
        .await?;
        let script = format!("{UTF8_OUTPUT}\n{}", execution.command);
        let commands: Vec<_> = self
            .options
            .programs
            .unwrap_or(&["pwsh", "powershell"])
            .iter()
            .map(|&program| {
                [
                    program,
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    script.as_str(),
                ]
            })
            .collect();
        run_commands(
            commands.iter().map(|argv| ShellCommand::Argv(argv)),
            &execution,
            args.timeout,
            env,
            output,
            context,
        )
        .await
    }
}
