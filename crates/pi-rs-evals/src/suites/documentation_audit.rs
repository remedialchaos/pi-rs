use super::*;
use std::path::{Component, PathBuf};
pub(super) const TOOL: &str = "submit_documentation_audit";
const DOCS: &str = "crates/pi-rs-coding-agent/assets/docs";
const SOURCE: &str = "crates/pi-rs-coding-agent/src";
const SUFFIX: &str = " matches the implementation";

pub(super) fn cases(repo: &Path) -> EvalResult<Vec<Case>> {
    let docs = repo.join(DOCS);
    let mut pending = vec![docs.clone()];
    let mut pages = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let path = entry.path();
            if ty.is_dir() {
                pending.push(path);
            } else if ty.is_file() && path.extension().is_some_and(|s| s == "md") {
                pages.push(
                    path.strip_prefix(&docs)
                        .map_err(|e| EvalError::message(e.to_string()))?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    if pages.is_empty() {
        return Err(EvalError::message(format!(
            "No documentation pages under {}",
            docs.display()
        )));
    }
    pages.sort();
    Ok(pages
        .into_iter()
        .map(|p| {
            Case::new(
                "documentation_audit",
                "Audit documentation against implementation",
                format!("{p}{SUFFIX}"),
            )
        })
        .collect())
}
fn page(repo: &Path, task: &EvalTask) -> EvalResult<PathBuf> {
    let name = task
        .case_id
        .strip_suffix(SUFFIX)
        .ok_or_else(|| EvalError::message("Invalid audit page identity"))?;
    if Path::new(name)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(EvalError::message("Invalid audit page path"));
    }
    Ok(repo.join(DOCS).join(name).canonicalize()?)
}
pub(super) fn input(repo: &Path, task: &EvalTask) -> EvalResult<PiCodingAgentInput> {
    Ok(format!("Audit this Pi documentation page against the repository implementation.\n\nDocumentation page: {}\nRepository root: {}\n\nRead the complete page and the relevant implementation. Report a mismatch only for a clear, user-visible contradiction between an explicit documentation claim and actual runtime behavior. Follow the runtime path; names, comments, types, isolated helpers, and tests are not sufficient evidence by themselves. Missing internal detail, ambiguous wording, hypothetical misuse, and undocumented edge cases are not mismatches. If the evidence is not decisive, report inconclusive. Otherwise report match. For a mismatch, quote the claim, cite the implementation path and symbol, and state the concrete behavior a user would observe. Call {TOOL} exactly once as your final action. Do not return prose.",page(repo,task)?.display(),repo.display()).into())
}
pub(super) fn install(ws: &EvalWorkspace) -> EvalResult<PathBuf> {
    let dir = ws.agent_dir.join("extensions");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("submit-documentation-audit.wasm");
    std::fs::write(&path, include_bytes!("documentation_audit/guest.wasm"))?;
    Ok(path)
}
pub(super) fn judge(
    repo: &Path,
    ws: &EvalWorkspace,
    task: &EvalTask,
    run: &SimpleHarnessResult<Value>,
) -> EvalResult<bool> {
    let doc = page(repo, task)?;
    let src = repo.join(SOURCE).canonicalize()?;
    let mut read_page = false;
    let mut read_source = false;
    let mut submissions = 0;
    let mut matched = false;
    let mut last_tool = None;
    for e in &run.events {
        let TranscriptEvent::ToolCall {
            id,
            name,
            arguments,
        } = e
        else {
            continue;
        };
        last_tool = Some(name.as_str());
        let ok=run.events.iter().any(|e|matches!(e,TranscriptEvent::ToolResult{tool_call_id,error:None,..} if tool_call_id==id));
        if name == TOOL {
            submissions += 1;
            matched = ok
                && arguments["verdict"] == "match"
                && arguments.as_object().is_some_and(|o| o.len() == 2)
                && arguments["explanation"]
                    .as_str()
                    .is_some_and(|s| (1..=2000).contains(&s.chars().count()));
        } else if name == "read"
            && ok
            && let Some(path) = arguments["path"].as_str()
            && let Ok(actual) = ws.workspace.join(path).canonicalize()
        {
            read_page |= actual == doc;
            read_source |= actual.starts_with(&src) && actual.is_file();
        }
    }
    Ok(read_page && read_source && submissions == 1 && matched && last_tool == Some(TOOL))
}
