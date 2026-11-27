//! Native evaluation definitions and deterministic runtime judges.
use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{EvalError, EvalResult};
use crate::harness::{
    DOCUMENTATION_EVAL_TOOLS, EvalWorkspace, HarnessError, HarnessStep,
    PiCodingAgentHarnessOptions, PiCodingAgentInput, PiCodingAgentModelSelection,
    SimpleHarnessResult, TranscriptEvent, TranscriptRole, run_pi_coding_agent,
};
use crate::plan::{DiscoveredEvalCase, DocumentationVariant, EvalTask};
use serde_json::Value;

mod acme_server;
mod configured_runtime;
mod custom_provider;
mod documentation_audit;
mod extensions;
mod models;
mod openai_provider;
mod prompt_templates;
mod smoke;
mod tui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuiteGroup {
    Documentation,
    Smoke,
    Audit,
    All,
}

struct Case {
    file: &'static str,
    title: &'static str,
    case: String,
}
impl Case {
    fn new(file: &'static str, title: &'static str, case: impl Into<String>) -> Self {
        Self {
            file,
            title,
            case: case.into(),
        }
    }
    fn discovered(self) -> DiscoveredEvalCase {
        DiscoveredEvalCase {
            file: format!("src/suites/{}.rs", self.file),
            full_name: format!("{} > {}", self.title, self.case),
            eval_set: self.title.to_owned(),
            case_id: self.case,
        }
    }
}

pub fn discover(repository: &Path, group: SuiteGroup) -> EvalResult<Vec<DiscoveredEvalCase>> {
    let mut cases = Vec::new();
    if matches!(group, SuiteGroup::Documentation | SuiteGroup::All) {
        cases.extend([
            custom_provider::case(),
            extensions::case(),
            models::case(),
            openai_provider::case(),
        ]);
        cases.extend(prompt_templates::cases());
        cases.push(tui::case());
    }
    if matches!(group, SuiteGroup::Smoke | SuiteGroup::All) {
        cases.push(smoke::case());
    }
    if matches!(group, SuiteGroup::Audit | SuiteGroup::All) {
        cases.extend(documentation_audit::cases(repository)?);
    }
    Ok(cases.into_iter().map(Case::discovered).collect())
}

fn error(message: impl std::fmt::Display) -> HarnessError {
    HarnessError {
        message: message.to_string(),
        partial_run: None,
    }
}

fn steps(prompt: String, followup: Option<&str>) -> PiCodingAgentInput {
    let mut steps = vec![HarnessStep::Prompt(prompt), HarnessStep::Reload];
    if let Some(text) = followup {
        steps.push(HarnessStep::Prompt(text.to_owned()));
    }
    PiCodingAgentInput::Steps(steps)
}

pub async fn run_case(
    repository: &Path,
    pi: &Path,
    task: &EvalTask,
    signal: Option<&pi_rs_coding_agent::utils::abort::AbortSignal>,
) -> Result<(SimpleHarnessResult<Value>, bool), HarnessError> {
    let file = task.file.as_str();
    let group = match file {
        "src/suites/documentation_audit.rs" => SuiteGroup::Audit,
        "src/suites/smoke.rs" => SuiteGroup::Smoke,
        _ => SuiteGroup::Documentation,
    };
    let definition = discover(repository, group)
        .map_err(error)?
        .into_iter()
        .find(|c| {
            c.file == task.file
                && c.case_id == task.case_id
                && c.eval_set == task.eval_set
                && c.full_name == task.full_name
        })
        .ok_or_else(|| error(format!("Unknown native eval: {}", task.full_name)))?;
    let (provider, id) = task
        .model
        .split_once('/')
        .ok_or_else(|| error("Expected provider/model in eval task"))?;
    let model = PiCodingAgentModelSelection {
        provider: provider.to_owned(),
        id: id.to_owned(),
    };
    let mut options = PiCodingAgentHarnessOptions {
        model: Some(model.clone()),
        expected_pi_documentation: (group == SuiteGroup::Documentation)
            .then_some(task.variant == DocumentationVariant::WithDocs),
        name: Some(
            if group == SuiteGroup::Documentation {
                task.variant.as_str()
            } else {
                &definition.eval_set
            }
            .to_owned(),
        ),
        tools: Some(
            DOCUMENTATION_EVAL_TOOLS
                .iter()
                .map(|t| (*t).to_owned())
                .collect(),
        ),
        ..Default::default()
    };
    if group == SuiteGroup::Documentation {
        options = crate::harness::create_pi_documentation_eval_harness(options)
            .map_err(error)?
            .options;
        if options.expected_pi_documentation != Some(task.variant == DocumentationVariant::WithDocs)
        {
            return Err(error(
                "Documentation task variant differs from the isolated image",
            ));
        }
    }
    let fixture = match file {
        "src/suites/custom_provider.rs" => Some(acme_server::Server::start(true).map_err(error)?),
        "src/suites/openai_provider.rs" => Some(acme_server::Server::start(false).map_err(error)?),
        _ => None,
    };
    let input = match file {
        "src/suites/custom_provider.rs" => {
            options.workspace_files.insert(
                "fixtures/acme-stream-api.json".into(),
                acme_server::api_documentation(),
            );
            custom_provider::input(
                fixture
                    .as_ref()
                    .ok_or_else(|| error("Missing Acme fixture"))?,
            )
        }
        "src/suites/openai_provider.rs" => openai_provider::input(
            fixture
                .as_ref()
                .ok_or_else(|| error("Missing Acme fixture"))?,
        ),
        "src/suites/extensions.rs" => {
            options
                .tools
                .get_or_insert_with(Vec::new)
                .push("hello".into());
            extensions::input()
        }
        "src/suites/models.rs" => models::input(),
        "src/suites/prompt_templates.rs" => {
            prompt_templates::input(prompt_templates::project(task))
        }
        "src/suites/tui.rs" => tui::input(),
        "src/suites/smoke.rs" => {
            options.tools = Some(Vec::new());
            smoke::input()
        }
        "src/suites/documentation_audit.rs" => {
            options.tools = Some(
                ["read", "grep", "find", "ls", documentation_audit::TOOL]
                    .map(str::to_owned)
                    .to_vec(),
            );
            documentation_audit::input(repository, task).map_err(error)?
        }
        _ => return Err(error("Unknown eval module")),
    };
    let workspace = EvalWorkspace::new(&options.workspace_files).map_err(error)?;
    if group == SuiteGroup::Audit {
        options
            .extensions
            .push(documentation_audit::install(&workspace).map_err(error)?);
    }
    let mut run = run_pi_coding_agent(pi, &workspace, &input, &options, signal).await?;
    let outcome: EvalResult<bool> = match file {
        "src/suites/custom_provider.rs" | "src/suites/openai_provider.rs" => {
            configured_runtime::provider(
                pi,
                &workspace,
                fixture
                    .as_ref()
                    .ok_or_else(|| error("Missing Acme fixture"))?,
            )
            .await
        }
        "src/suites/extensions.rs" => Ok(extensions::judge(&run)),
        "src/suites/models.rs" => configured_runtime::added_model(pi, &workspace).await,
        "src/suites/prompt_templates.rs" => {
            prompt_templates::judge(pi, &workspace, &run, prompt_templates::project(task)).await
        }
        "src/suites/tui.rs" => tui::judge(pi, &workspace).await,
        "src/suites/smoke.rs" => Ok(smoke::judge(&run, &model)),
        "src/suites/documentation_audit.rs" => {
            documentation_audit::judge(repository, &workspace, task, &run)
        }
        _ => Err(EvalError::message("Unknown eval module")),
    };
    match outcome {
        Ok(passed) => Ok((run, passed)),
        Err(e) => {
            run.errors.push(Value::String(e.to_string()));
            Err(HarnessError {
                message: e.to_string(),
                partial_run: Some(Box::new(run)),
            })
        }
    }
}
