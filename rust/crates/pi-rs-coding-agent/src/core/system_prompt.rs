//! System prompt construction and project context loading. Ported from
//! `core/system-prompt.ts` plus the `SYSTEM.md`/`APPEND_SYSTEM.md` discovery in
//! `core/resource-loader.ts`.
//!
//! The prompt is a set of ordered, independently replaceable sections. `preamble`
//! is untagged text; every other section is wrapped in a tag of the same name,
//! which is what the transcript's `SystemMessage.sections` replay expects.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use pi_rs_ai::{StringOrBlocks, SystemMessage, SystemRole, transcript::system_message_text};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::core::skills::{FileReadTool, Skill, format_skills_for_prompt};

/// Default preamble used when no custom prompt replaces it.
pub const DEFAULT_PREAMBLE: &str = "You are a coding assistant running inside pi, a coding agent harness. Help users inspect files, run commands, and modify code.";
pub const DEFAULT_SELECTED_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// A project context file injected into the `project_context` section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

/// Inputs to [`build_system_prompt_sections`].
#[derive(Clone, Debug, Default)]
pub struct SystemPromptOptions {
    /// Custom system prompt that replaces the default preamble and its tool/docs sections.
    pub custom_prompt: Option<String>,
    /// Exact full prompt replacement set by a `before_agent_start` handler.
    pub force_system_prompt: Option<String>,
    pub selected_tools: Option<Vec<String>>,
    pub hidden_tools: Option<Vec<String>>,
    pub tool_snippets: IndexMap<String, String>,
    pub tool_guidelines: IndexMap<String, Vec<String>>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: Option<String>,
    pub sections: IndexMap<String, String>,
    pub cwd: PathBuf,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
}

/// Fully populated options after applying upstream defaults.
#[derive(Clone, Debug)]
pub struct NormalizedSystemPromptOptions {
    pub custom_prompt: Option<String>,
    pub force_system_prompt: Option<String>,
    pub selected_tools: Vec<String>,
    pub hidden_tools: Vec<String>,
    pub tool_snippets: IndexMap<String, String>,
    pub tool_guidelines: IndexMap<String, Vec<String>>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: String,
    pub sections: IndexMap<String, String>,
    pub cwd: PathBuf,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
}

/// Ordered system prompt sections keyed by name.
pub type SystemPromptSections = IndexMap<String, String>;

/// The complete structured prompt state for a transcript system message.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemPromptState {
    pub content: String,
    pub sections: Option<SystemPromptSections>,
}

fn is_valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// Normalize prompt input into the collection-complete shape exposed to extensions.
pub fn normalize_system_prompt_options(
    input: &SystemPromptOptions,
) -> NormalizedSystemPromptOptions {
    let tool_guidelines = input
        .tool_guidelines
        .iter()
        .map(|(name, guidelines)| (name.clone(), guidelines.clone()))
        .collect();
    NormalizedSystemPromptOptions {
        custom_prompt: input.custom_prompt.clone(),
        force_system_prompt: input.force_system_prompt.clone(),
        selected_tools: input.selected_tools.clone().unwrap_or_else(|| {
            DEFAULT_SELECTED_TOOLS
                .iter()
                .map(|tool| (*tool).to_string())
                .collect()
        }),
        hidden_tools: input.hidden_tools.clone().unwrap_or_default(),
        tool_snippets: input.tool_snippets.clone(),
        tool_guidelines,
        prompt_guidelines: input.prompt_guidelines.clone(),
        append_system_prompt: input.append_system_prompt.clone().unwrap_or_default(),
        sections: input.sections.clone(),
        cwd: input.cwd.clone(),
        context_files: input.context_files.clone(),
        skills: input.skills.clone(),
    }
}

fn path_text(result: Result<PathBuf, String>) -> String {
    result.map_or_else(
        |_| String::new(),
        |path| path.to_string_lossy().into_owned(),
    )
}

fn render_project_context(context_files: &[ContextFile]) -> String {
    let mut lines = vec!["Project-specific instructions and guidelines:".to_string()];
    for file in context_files {
        lines.push(format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            file.path, file.content
        ));
    }
    lines.join("\n\n")
}

fn build_rules(
    selected_tools: &[String],
    tool_guidelines: &IndexMap<String, Vec<String>>,
    prompt_guidelines: &[String],
) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut add = |rule: &str| {
        let normalized = rule.trim();
        if normalized.is_empty() || !seen.insert(normalized.to_string()) {
            return;
        }
        rules.push(normalized.to_string());
    };

    let has = |name: &str| selected_tools.iter().any(|tool| tool == name);
    let has_bash = has("bash");
    let has_powershell = has("powershell");
    let has_grep = has("grep");
    let has_find = has("find");
    let has_ls = has("ls");
    if (has_bash || has_powershell) && !has_grep && !has_find && !has_ls {
        if has_bash && has_powershell {
            add(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
            );
        } else if has_powershell {
            add("Use PowerShell for file operations like listing, searching, and finding files");
        } else {
            add("Use bash for file operations like ls, rg, find");
        }
    }
    for name in selected_tools {
        if let Some(guidelines) = tool_guidelines.get(name) {
            for rule in guidelines {
                add(rule);
            }
        }
    }
    for rule in prompt_guidelines {
        add(rule);
    }
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules
        .into_iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn docs_section() -> String {
    format!(
        "Pi documentation (read only when the user asks about the harness itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: {}\n- Additional docs: {}\n- Examples: {} (extensions, custom tools, SDK)\n- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), packages (docs/packages.md), environment variables (docs/environment-variables.md), MCP servers (docs/mcp.md), codemode scripts and non-LLM models such as classifiers and image models (docs/codemode.md)\n- Before implementing harness changes, read the relevant docs and examples in full, following related cross-references.",
        path_text(config::get_readme_path()),
        path_text(config::get_docs_path()),
        path_text(config::get_examples_path())
    )
}

/// Build the ordered, independently replaceable sections of the prompt.
pub fn build_system_prompt_sections(
    input: &SystemPromptOptions,
) -> Result<SystemPromptSections, String> {
    let options = normalize_system_prompt_options(input);
    for name in options.sections.keys() {
        if name == "preamble" || !is_valid_section_name(name) {
            return Err(format!("Invalid system prompt section name: {name}"));
        }
    }

    let declared_tools: Vec<String> = options
        .selected_tools
        .iter()
        .filter(|name| !options.hidden_tools.contains(name))
        .cloned()
        .collect();
    let mut prompt_sections: IndexMap<String, String> = IndexMap::new();
    match &options.custom_prompt {
        Some(custom) if !custom.is_empty() => {
            prompt_sections.insert("preamble".into(), custom.clone());
        }
        _ => {
            prompt_sections.insert("preamble".into(), DEFAULT_PREAMBLE.to_string());
            let visible: Vec<&String> = declared_tools
                .iter()
                .filter(|name| options.tool_snippets.contains_key(*name))
                .collect();
            let tools = if visible.is_empty() {
                "(none)".to_string()
            } else {
                visible
                    .iter()
                    .map(|name| format!("- {}: {}", name, options.tool_snippets[*name]))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            prompt_sections.insert(
                "tools".into(),
                format!("{tools}\n\nAdditional tools may be available."),
            );
            prompt_sections.insert(
                "rules".into(),
                build_rules(
                    &declared_tools,
                    &options.tool_guidelines,
                    &options.prompt_guidelines,
                ),
            );
            prompt_sections.insert("docs".into(), docs_section());
        }
    }

    if !options.append_system_prompt.is_empty() {
        prompt_sections.insert("addendum".into(), options.append_system_prompt.clone());
    }
    if !options.context_files.is_empty() {
        prompt_sections.insert(
            "project_context".into(),
            render_project_context(&options.context_files),
        );
    }
    let readers = ["read", "bash"];
    let skill_file_read_tool = readers
        .iter()
        .find(|tool| declared_tools.iter().any(|name| name == **tool))
        .map(|tool| {
            if *tool == "read" {
                FileReadTool::Read
            } else {
                FileReadTool::Bash
            }
        })
        .or_else(|| {
            if readers
                .iter()
                .any(|tool| options.selected_tools.iter().any(|name| name == *tool))
            {
                Some(FileReadTool::Indirect)
            } else {
                None
            }
        });
    if let Some(tool) = skill_file_read_tool
        && !options.skills.is_empty()
    {
        let skills_prompt = format_skills_for_prompt(&options.skills, tool)
            .trim()
            .to_string();
        if !skills_prompt.is_empty() {
            prompt_sections.insert("skills".into(), skills_prompt);
        }
    }
    prompt_sections.insert(
        "cwd".into(),
        options.cwd.to_string_lossy().replace('\\', "/"),
    );
    for (name, content) in options.sections {
        if !content.is_empty() {
            prompt_sections.insert(name, content);
        }
    }

    let mut sections = SystemPromptSections::new();
    if let Some(preamble) = prompt_sections.shift_remove("preamble") {
        sections.insert("preamble".into(), preamble);
    }
    for (name, content) in prompt_sections {
        sections.insert(name.clone(), format!("<{name}>\n{content}\n</{name}>"));
    }
    Ok(sections)
}

/// Build the full prompt state: a forced prompt is opaque and has no sections.
pub fn build_system_prompt_state(input: &SystemPromptOptions) -> Result<SystemPromptState, String> {
    if let Some(forced) = &input.force_system_prompt {
        return Ok(SystemPromptState {
            content: forced.clone(),
            sections: None,
        });
    }
    Ok(SystemPromptState {
        content: String::new(),
        sections: Some(build_system_prompt_sections(input)?),
    })
}

/// Build the system prompt text exactly as the transcript replays it.
pub fn build_system_prompt(input: &SystemPromptOptions) -> Result<String, String> {
    let state = build_system_prompt_state(input)?;
    let mut message = SystemMessage {
        role: SystemRole::System,
        content: StringOrBlocks::String(state.content),
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
    };
    if let Some(sections) = state.sections {
        message.sections = Some(
            sections
                .into_iter()
                .map(|(name, content)| (name, Some(content)))
                .collect(),
        );
    }
    Ok(system_message_text(&message))
}

/// Diff the sections currently in a transcript against the desired ones.
pub fn diff_system_prompt_sections(
    previous: &IndexMap<String, Option<String>>,
    current: &SystemPromptSections,
) -> Option<IndexMap<String, Option<String>>> {
    let mut patch = IndexMap::new();
    for (name, text) in current {
        if previous.get(name).and_then(Option::as_deref) != Some(text.as_str()) {
            patch.insert(name.clone(), Some(text.clone()));
        }
    }
    for name in previous.keys() {
        if !current.contains_key(name) {
            patch.insert(name.clone(), None);
        }
    }
    if patch.is_empty() { None } else { Some(patch) }
}

/// Files discovered for the project/global system prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemPromptFiles {
    pub custom_prompt: Option<String>,
    pub append_text: Option<String>,
    pub custom_prompt_path: Option<PathBuf>,
    pub append_prompt_path: Option<PathBuf>,
}

fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Discover `SYSTEM.md` and `APPEND_SYSTEM.md` in the project (`.pi/`) and agent
/// directories. Project files take precedence; upstream gates them on project
/// trust, which callers express with `project_trusted`.
pub fn load_system_prompt_files(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
) -> SystemPromptFiles {
    let project_custom = if project_trusted {
        cwd.join(config::CONFIG_DIR_NAME).join("SYSTEM.md")
    } else {
        PathBuf::new()
    };
    let project_append = if project_trusted {
        cwd.join(config::CONFIG_DIR_NAME).join("APPEND_SYSTEM.md")
    } else {
        PathBuf::new()
    };
    let global_custom = agent_dir.join("SYSTEM.md");
    let global_append = agent_dir.join("APPEND_SYSTEM.md");

    let custom = if project_trusted {
        read_optional(&project_custom).map(|text| (text, Some(project_custom.clone())))
    } else {
        None
    }
    .or_else(|| read_optional(&global_custom).map(|text| (text, Some(global_custom.clone()))));
    let append = if project_trusted {
        read_optional(&project_append).map(|text| (text, Some(project_append.clone())))
    } else {
        None
    }
    .or_else(|| read_optional(&global_append).map(|text| (text, Some(global_append.clone()))));

    SystemPromptFiles {
        custom_prompt: custom.as_ref().map(|(text, _)| text.clone()),
        append_text: append.as_ref().map(|(text, _)| text.clone()),
        custom_prompt_path: custom.and_then(|(_, path)| path),
        append_prompt_path: append.and_then(|(_, path)| path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(cwd: &Path) -> SystemPromptOptions {
        SystemPromptOptions {
            cwd: cwd.to_path_buf(),
            selected_tools: Some(Vec::new()),
            ..SystemPromptOptions::default()
        }
    }

    #[test]
    fn empty_tools_shows_none_and_file_path_guideline() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let prompt = build_system_prompt(&options(dir.path()))?;
        assert!(prompt.contains("<tools>\n(none)\n"));
        assert!(prompt.contains("Show file paths clearly"));
        Ok(())
    }

    #[test]
    fn custom_prompt_is_followed_by_sections() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut input = options(dir.path());
        input.custom_prompt = Some("You are Exact.".into());
        let prompt = build_system_prompt(&input)?;
        assert!(prompt.starts_with("You are Exact.\n\n<cwd>"));
        Ok(())
    }

    #[test]
    fn forced_prompt_has_no_sections() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut input = options(dir.path());
        input.force_system_prompt = Some("exact".into());
        assert_eq!(build_system_prompt(&input)?, "exact");
        Ok(())
    }

    #[test]
    fn addendum_project_context_and_cwd_have_stable_sections() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut input = options(dir.path());
        input.custom_prompt = Some("You are Exact.".into());
        input.append_system_prompt = Some("Additional instructions.".into());
        input.context_files.push(ContextFile {
            path: "/tmp/AGENTS.md".into(),
            content: "Project instructions.".into(),
        });
        let prompt = build_system_prompt(&input)?;
        assert!(prompt.contains("<addendum>\nAdditional instructions.\n</addendum>"));
        assert!(prompt.contains(
            "<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/tmp/AGENTS.md\">"
        ));
        assert!(prompt.contains(&format!("<cwd>\n{}\n</cwd>", dir.path().display())));
        Ok(())
    }

    #[test]
    fn tool_snippets_and_guidelines_are_deduplicated() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut input = options(dir.path());
        input.selected_tools = Some(vec!["read".into(), "custom".into()]);
        input
            .tool_snippets
            .insert("read".into(), "Read files".into());
        input
            .tool_snippets
            .insert("custom".into(), "Do custom work".into());
        input.prompt_guidelines = vec![
            "  Use custom for summaries.  ".into(),
            "Use custom for summaries.".into(),
        ];
        let prompt = build_system_prompt(&input)?;
        assert!(prompt.contains("- read: Read files"));
        assert!(prompt.contains("- custom: Do custom work"));
        assert_eq!(prompt.matches("- Use custom for summaries.").count(), 1);
        Ok(())
    }

    #[test]
    fn hidden_tools_are_omitted_from_tools_and_rules() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut input = options(dir.path());
        input.selected_tools = Some(vec!["read".into(), "bash".into(), "run".into()]);
        input.hidden_tools = Some(vec!["read".into(), "bash".into()]);
        input
            .tool_snippets
            .insert("read".into(), "Read files".into());
        input
            .tool_snippets
            .insert("run".into(), "Run a task".into());
        input
            .tool_guidelines
            .insert("read".into(), vec!["Use read for files.".into()]);
        input
            .tool_guidelines
            .insert("run".into(), vec!["Prefer run.".into()]);
        let prompt = build_system_prompt(&input)?;
        assert!(prompt.contains("<tools>\n- run: Run a task\n"));
        assert!(!prompt.contains("- read: "));
        assert!(!prompt.contains("Use read for files."));
        assert!(prompt.contains("- Prefer run."));
        Ok(())
    }

    #[test]
    fn shell_specific_guidance_follows_selected_tools() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut input = options(dir.path());
        input.selected_tools = Some(vec!["powershell".into()]);
        assert!(build_system_prompt(&input)?.contains("Use PowerShell for file operations"));
        input.selected_tools = Some(vec!["bash".into(), "powershell".into()]);
        assert!(
            build_system_prompt(&input)?.contains("Use bash or PowerShell for file operations")
        );
        Ok(())
    }

    #[test]
    fn invalid_section_names_are_rejected() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        for name in ["preamble", "Bad", "1bad", "bad.name"] {
            let mut input = options(dir.path());
            input.sections.insert(name.into(), "text".into());
            let error = build_system_prompt(&input).expect_err("invalid name must fail");
            assert!(error.contains("Invalid system prompt section name"));
        }
        Ok(())
    }

    #[test]
    fn diff_reports_added_changed_and_removed_sections() -> Result<(), String> {
        let mut previous = IndexMap::new();
        previous.insert("same".to_string(), Some("value".to_string()));
        previous.insert("gone".to_string(), Some("old".to_string()));
        previous.insert("changed".to_string(), Some("before".to_string()));
        let mut current = SystemPromptSections::new();
        current.insert("same".into(), "value".into());
        current.insert("changed".into(), "after".into());
        current.insert("new".into(), "fresh".into());
        let patch = diff_system_prompt_sections(&previous, &current).ok_or("expected patch")?;
        assert_eq!(patch.get("same"), None);
        assert_eq!(patch.get("gone"), Some(&None));
        assert_eq!(
            patch.get("changed").and_then(Option::as_deref),
            Some("after")
        );
        assert_eq!(patch.get("new").and_then(Option::as_deref), Some("fresh"));
        Ok(())
    }

    #[test]
    fn project_context_files_override_global_but_require_trust() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agent = tempfile::tempdir().map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir.path().join(".pi")).map_err(|e| e.to_string())?;
        std::fs::write(
            dir.path().join(".pi/SYSTEM.md"),
            "\u{feff}project system".as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(agent.path().join("SYSTEM.md"), "global system")
            .map_err(|e| e.to_string())?;
        std::fs::write(agent.path().join("APPEND_SYSTEM.md"), "global append")
            .map_err(|e| e.to_string())?;

        let untrusted = load_system_prompt_files(dir.path(), agent.path(), false);
        assert_eq!(untrusted.custom_prompt.as_deref(), Some("global system"));
        assert_eq!(untrusted.append_text.as_deref(), Some("global append"));

        let trusted = load_system_prompt_files(dir.path(), agent.path(), true);
        assert_eq!(trusted.custom_prompt.as_deref(), Some("project system"));
        assert_eq!(trusted.append_text.as_deref(), Some("global append"));
        Ok(())
    }
}
