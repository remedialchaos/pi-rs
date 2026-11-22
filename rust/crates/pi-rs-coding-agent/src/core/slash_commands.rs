//! Slash command parsing and registries. Ported from `core/slash-commands.ts`
//! plus the built-in command table and prompt-template matching used by the
//! interactive mode.

use crate::config::APP_NAME;
use crate::core::prompt_templates::{PromptTemplate, substitute_args};
use crate::core::skills::{Skill, SourceInfo, create_synthetic_source_info};

/// Where a slash command came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommandSource {
    Extension,
    Prompt,
    Skill,
}

impl SlashCommandSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Prompt => "prompt",
            Self::Skill => "skill",
        }
    }
}

/// Metadata shown in the command picker.
#[derive(Clone, Debug, PartialEq)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: SourceInfo,
}

/// A command exposed to the user, with an optional prompt template body.
#[derive(Clone, Debug, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: SourceInfo,
    pub prompt: Option<String>,
}

impl From<SlashCommand> for SlashCommandInfo {
    fn from(command: SlashCommand) -> Self {
        Self {
            name: command.name,
            description: command.description,
            source: command.source,
            source_info: command.source_info,
        }
    }
}

/// A command defined by the agent itself (no extension or file backing it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

/// Built-in commands in the order shown to the user.
pub const BUILTIN_SLASH_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand {
        name: "settings",
        description: "Open settings menu",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "model",
        description: "Select model (opens selector UI)",
        argument_hint: Some("<provider/model>"),
    },
    BuiltinSlashCommand {
        name: "tree",
        description: "Navigate session tree (switch branches)",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "thinking",
        description: "Set thinking level",
        argument_hint: Some("<level>"),
    },
    BuiltinSlashCommand {
        name: "scoped-models",
        description: "Enable/disable models for Ctrl+P cycling",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "export",
        description: "Export session (HTML default, or specify path: .html/.jsonl)",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "import",
        description: "Import a JSONL session, preserving its ID and working directory",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "share",
        description: "Share session via a Radius artifact or a GitHub gist",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "bug",
        description: "Report a bug to the Pi developers",
        argument_hint: Some("<description>"),
    },
    BuiltinSlashCommand {
        name: "copy",
        description: "Copy last agent message to clipboard",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "name",
        description: "Set session display name",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "session",
        description: "Show session info and stats",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "changelog",
        description: "Show changelog entries",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "hotkeys",
        description: "Show all keyboard shortcuts",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "fork",
        description: "Create a new fork from a previous user message",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "clone",
        description: "Duplicate the current session at the current position",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "trust",
        description: "Save project trust decision for future sessions",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("<provider>"),
    },
    BuiltinSlashCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "compact",
        description: "Manually compact the session context",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "resume",
        description: "Resume another session",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "reload",
        description: "Reload keybindings, extensions, skills, prompts, themes, and context files",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "quit",
        description: QUIT_DESCRIPTION,
        argument_hint: None,
    },
];

const QUIT_DESCRIPTION: &str = "Quit pi";

/// Parsed `/command args` input.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedSlashCommand {
    pub name: String,
    pub args: String,
}

/// Parse a leading slash command. Returns `None` for plain text or a lone `/`.
pub fn parse_slash_command(input: &str) -> Option<ParsedSlashCommand> {
    let text = input.trim_start();
    let after_slash = text.strip_prefix('/')?;
    let split = after_slash.find(char::is_whitespace);
    match split {
        Some(index) => Some(ParsedSlashCommand {
            name: after_slash[..index].to_string(),
            args: after_slash[index..].trim().to_string(),
        }),
        None if !after_slash.is_empty() => Some(ParsedSlashCommand {
            name: after_slash.to_string(),
            args: String::new(),
        }),
        None => None,
    }
}

/// True if the text looks like a slash command.
pub fn is_slash_command(input: &str) -> bool {
    parse_slash_command(input).is_some()
}

fn synthetic_info(path: &str, source: SlashCommandSource) -> SourceInfo {
    let source = match source {
        SlashCommandSource::Extension => "extension",
        SlashCommandSource::Prompt => "prompt",
        SlashCommandSource::Skill => "skill",
    };
    create_synthetic_source_info(std::path::Path::new(path), source, None, None, None)
}

/// Built-in commands converted to the common `SlashCommand` shape.
pub fn builtin_commands() -> Vec<SlashCommand> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| SlashCommand {
            name: command.name.to_string(),
            description: Some(command.description.to_string()),
            argument_hint: command.argument_hint.map(str::to_string),
            source: SlashCommandSource::Extension,
            source_info: synthetic_info("<builtin>", SlashCommandSource::Extension),
            prompt: None,
        })
        .collect()
}

/// Prompt templates exposed as slash commands.
pub fn prompt_commands(templates: &[PromptTemplate]) -> Vec<SlashCommand> {
    templates
        .iter()
        .map(|template| SlashCommand {
            name: template.name.clone(),
            description: Some(template.description.clone()),
            argument_hint: template.argument_hint.clone(),
            source: SlashCommandSource::Prompt,
            source_info: template.source_info.clone(),
            prompt: Some(template.content.clone()),
        })
        .collect()
}

/// Skills exposed as slash commands that load their SKILL.md file.
pub fn skill_commands(skills: &[Skill]) -> Vec<SlashCommand> {
    skills
        .iter()
        .map(|skill| SlashCommand {
            name: skill.name.clone(),
            description: Some(skill.description.clone()),
            argument_hint: None,
            source: SlashCommandSource::Skill,
            source_info: skill.source_info.clone(),
            prompt: Some(format!(
                "Load the '{}' skill by reading its file: {}",
                skill.name,
                skill.file_path.display()
            )),
        })
        .collect()
}

/// Merge built-ins, prompt templates, and skills. Earlier entries win on name.
pub fn list_commands(templates: &[PromptTemplate], skills: &[Skill]) -> Vec<SlashCommand> {
    let mut names = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for command in builtin_commands()
        .into_iter()
        .chain(prompt_commands(templates))
        .chain(skill_commands(skills))
    {
        if names.insert(command.name.clone()) {
            commands.push(command);
        }
    }
    commands
}

/// Find a command by exact name.
pub fn find_command<'a>(commands: &'a [SlashCommand], name: &str) -> Option<&'a SlashCommand> {
    commands.iter().find(|command| command.name == name)
}

/// Expand a prompt template command's `$1`/`$@` placeholders with parsed args.
pub fn render_command_prompt(command: &SlashCommand, args: &str) -> Option<String> {
    let prompt = command.prompt.as_ref()?;
    let parsed = crate::core::prompt_templates::parse_command_args(args);
    Some(substitute_args(prompt, &parsed))
}

/// Convenience: parse a slash command and expand its prompt in one step.
pub fn match_prompt_command(input: &str, commands: &[SlashCommand]) -> Option<String> {
    let parsed = parse_slash_command(input)?;
    let command = find_command(commands, &parsed.name)?;
    render_command_prompt(command, &parsed.args)
}

/// The `quit` command description uses the configured application name.
pub fn quit_description() -> String {
    format!("Quit {APP_NAME}")
}

/// Verify the built-in table stays in sync with the app name.
pub fn is_builtin_name(name: &str) -> bool {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .any(|command| command.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_and_args() -> Result<(), String> {
        let parsed =
            parse_slash_command("/model anthropic/claude-sonnet-4-5").ok_or("no command")?;
        assert_eq!(parsed.name, "model");
        assert_eq!(parsed.args, "anthropic/claude-sonnet-4-5");
        Ok(())
    }

    #[test]
    fn parses_command_without_args_and_trims_input() {
        assert_eq!(
            parse_slash_command("  /compact  "),
            Some(ParsedSlashCommand {
                name: "compact".into(),
                args: String::new()
            })
        );
        assert_eq!(parse_slash_command("plain text"), None);
        assert_eq!(parse_slash_command("/"), None);
    }

    #[test]
    fn builtin_table_has_all_commands_and_quit_description() {
        assert_eq!(BUILTIN_SLASH_COMMANDS.len(), 24);
        assert!(is_builtin_name("settings"));
        assert!(is_builtin_name("quit"));
        assert!(!is_builtin_name("unknown"));
        assert_eq!(quit_description(), QUIT_DESCRIPTION);
    }

    #[test]
    fn model_command_has_argument_hint() -> Result<(), String> {
        let commands = builtin_commands();
        let command = find_command(&commands, "model").ok_or("missing model command")?;
        assert_eq!(command.argument_hint.as_deref(), Some("<provider/model>"));
        assert_eq!(
            command.description.as_deref(),
            Some("Select model (opens selector UI)")
        );
        Ok(())
    }

    #[test]
    fn prompt_template_renders_arguments() {
        let template = PromptTemplate {
            name: "review".into(),
            description: "Review a file".into(),
            argument_hint: Some("<path>".into()),
            content: "Review $1".into(),
            source_info: synthetic_info("/prompts/review.md", SlashCommandSource::Prompt),
            file_path: std::path::PathBuf::from("/prompts/review.md"),
        };
        let commands = prompt_commands(std::slice::from_ref(&template));
        assert_eq!(
            match_prompt_command("/review src/lib.rs", &commands).as_deref(),
            Some("Review src/lib.rs")
        );
    }

    #[test]
    fn listing_deduplicates_builtin_names() {
        let template = PromptTemplate {
            name: "model".into(),
            description: "Shadow".into(),
            argument_hint: None,
            content: "shadow".into(),
            source_info: synthetic_info("/prompts/model.md", SlashCommandSource::Prompt),
            file_path: std::path::PathBuf::from("/prompts/model.md"),
        };
        let commands = list_commands(std::slice::from_ref(&template), &[]);
        assert_eq!(commands.len(), BUILTIN_SLASH_COMMANDS.len());
        assert!(find_command(&commands, "model").is_some_and(|command| command.prompt.is_none()));
    }

    #[test]
    fn slash_command_detection_ignores_plain_text() {
        assert!(is_slash_command("/new"));
        assert!(!is_slash_command("new"));
    }
}
