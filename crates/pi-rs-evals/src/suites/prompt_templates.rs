use super::*;
use pi_rs_coding_agent::{core::skills::SourceScope, modes::rpc::RpcSlashCommandSource};
use std::time::Duration;

const PROJECT: &str = "Create a project prompt template";
const USER: &str = "Create a user prompt template";
const TEXT: &str = "Tell me the current time and timezone.";
pub(super) fn cases() -> [Case; 2] {
    [PROJECT, USER].map(|set| {
        Case::new(
            "prompt_templates",
            set,
            "creates, reloads, and invokes /current-time",
        )
    })
}
pub(super) fn project(task: &EvalTask) -> bool {
    task.eval_set == PROJECT
}
pub(super) fn input(project: bool) -> PiCodingAgentInput {
    let request = if project {
        "This project is already trusted. Add a /current-time prompt template for this project"
    } else {
        "Make a /current-time prompt template available in all my Pi sessions"
    };
    steps(
        format!("{request} containing exactly: {TEXT}"),
        Some("/current-time"),
    )
}
pub(super) async fn judge(
    pi: &Path,
    ws: &EvalWorkspace,
    run: &SimpleHarnessResult<Value>,
    project: bool,
) -> EvalResult<bool> {
    let project_file = ws.workspace.join(".pi/prompts/current-time.md");
    let user_file = ws.agent_dir.join("prompts/current-time.md");
    let (expected, other) = if project {
        (project_file, user_file)
    } else {
        (user_file, project_file)
    };
    let (_child, rpc) = configured_runtime::probe(pi, ws)?;
    let commands = tokio::time::timeout(Duration::from_secs(30), rpc.get_commands())
        .await
        .map_err(|_| EvalError::message("Prompt catalog probe timed out"))??;
    let templates: Vec<_> = commands
        .iter()
        .filter(|c| matches!(c.source, RpcSlashCommandSource::Prompt) && c.name == "current-time")
        .collect();
    let expected_scope = if project {
        SourceScope::Project
    } else {
        SourceScope::User
    };
    for dir in [
        ws.workspace.join(".pi/extensions"),
        ws.agent_dir.join("extensions"),
    ] {
        match std::fs::read_dir(&dir) {
            Ok(mut files) => {
                if files.next().transpose()?.is_some() {
                    return Ok(false);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let expanded = run.events.iter().rev().find_map(|e| match e {
        TranscriptEvent::Message {
            role: TranscriptRole::User,
            content,
        } => content.as_str(),
        _ => None,
    });
    Ok(
        matches!(templates.as_slice(),[t] if Path::new(&t.source_info.path)==expected && t.source_info.scope==expected_scope)
            && !other.try_exists()?
            && !commands
                .iter()
                .any(|c| matches!(c.source, RpcSlashCommandSource::Skill))
            && expanded.is_some_and(|p| p.trim() == TEXT),
    )
}
