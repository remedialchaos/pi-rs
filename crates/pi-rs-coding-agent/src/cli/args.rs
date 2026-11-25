//! Hand-written CLI grammar, including arguments registered by extensions.

use std::fmt::Write;

use indexmap::IndexMap;
use pi_rs_ai::ThinkingLevel;
use pi_rs_tui::TuiMode;

pub use crate::core::extensions::types::FlagValue;
use crate::core::extensions::types::{FlagDefinition, FlagKind};
use crate::core::settings_manager::is_tool_modifier;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListModels {
    All,
    Search(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ThinkingLevel>,
    pub is_continue: bool,
    pub resume: bool,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Vec<String>,
    pub no_extensions: bool,
    pub no_mcp: bool,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Vec<String>,
    pub prompt_templates: Vec<String>,
    pub no_prompt_templates: bool,
    pub themes: Vec<String>,
    pub use_theme: Option<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub list_models: Option<ListModels>,
    pub offline: bool,
    pub no_refresh: bool,
    pub tui_mode: Option<TuiMode>,
    pub verbose: bool,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    pub unknown_flags: IndexMap<String, FlagValue>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Args {
    fn diagnostic(&mut self, kind: DiagnosticKind, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            kind,
            message: message.into(),
        });
    }

    fn positional(&mut self, arg: &str) {
        if let Some(file) = arg.strip_prefix('@') {
            self.file_args.push(file.to_owned());
        } else {
            self.messages.push(arg.to_owned());
        }
    }
}

fn trim(value: &str) -> &str {
    // String.prototype.trim accepts BOM but not NEL (U+0085).
    value.trim_matches(|c| {
        matches!(c, '\u{0009}'..='\u{000D}' | ' ' | '\u{00A0}' | '\u{1680}' |
            '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' |
            '\u{205F}' | '\u{3000}' | '\u{FEFF}')
    })
}

pub fn normalize_session_name(value: &str) -> Option<&str> {
    let name = trim(value);
    (!name.is_empty()).then_some(name)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

fn tool_list_error(tools: &[String]) -> Option<String> {
    let modifiers = tools.iter().filter(|name| is_tool_modifier(name)).count();
    if modifiers == 0 {
        return None;
    }
    if modifiers < tools.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_owned());
    }
    tools.iter().find(|name| name.contains('*')).map(|pattern| {
        format!("+name and -name entries take exact tool names, not patterns: {pattern}")
    })
}

pub fn parse_args(args: &[String]) -> Args {
    use DiagnosticKind::{Error, Warning};

    let mut result = Args::default();
    let mut args = args.iter().map(String::as_str).peekable();
    while let Some(arg) = args.next() {
        let next = args.peek().copied();
        match arg {
            "--" => {
                for positional in args {
                    result.positional(positional);
                }
                break;
            }
            "--help" | "-h" => result.help = true,
            "--version" | "-v" => result.version = true,
            "--mode" => {
                if let Some(value) = next.filter(|value| !value.starts_with('-')) {
                    args.next();
                    match value {
                        "text" => result.mode = Some(Mode::Text),
                        "json" => result.mode = Some(Mode::Json),
                        "rpc" => result.mode = Some(Mode::Rpc),
                        _ => result.diagnostic(
                            Error,
                            format!("Invalid mode \"{value}\". Valid values: text, json, rpc"),
                        ),
                    }
                } else {
                    result.diagnostic(Error, "--mode requires text, json, or rpc");
                }
            }
            "--continue" | "-c" => result.is_continue = true,
            "--resume" | "-r" => result.resume = true,
            "--provider" if next.is_some() => result.provider = args.next().map(str::to_owned),
            "--model" if next.is_some() => result.model = args.next().map(str::to_owned),
            "--api-key" if next.is_some() => result.api_key = args.next().map(str::to_owned),
            "--system-prompt" if next.is_some() => {
                result.system_prompt = args.next().map(str::to_owned);
            }
            "--append-system-prompt" if next.is_some() => {
                result
                    .append_system_prompt
                    .extend(args.next().map(str::to_owned));
            }
            "--name" | "-n" => {
                if let Some(name) = args.next() {
                    result.name = Some(name.to_owned());
                } else {
                    result.diagnostic(Error, "--name requires a value");
                }
            }
            "--no-session" => result.no_session = true,
            "--session" if next.is_some() => result.session = args.next().map(str::to_owned),
            "--session-id" if next.is_some() => result.session_id = args.next().map(str::to_owned),
            "--fork" if next.is_some() => result.fork = args.next().map(str::to_owned),
            "--session-dir" if next.is_some() => {
                result.session_dir = args.next().map(str::to_owned)
            }
            "--models" if next.is_some() => result.models = args.next().map(split_csv),
            "--no-tools" | "-nt" => result.no_tools = true,
            "--no-builtin-tools" | "-nbt" => result.no_builtin_tools = true,
            "--tools" | "-t" if next.is_some() => {
                if let Some(tools) = args.next().map(split_csv) {
                    if let Some(error) = tool_list_error(&tools) {
                        result.diagnostic(Error, format!("{arg}: {error}"));
                    } else {
                        result.tools = Some(tools);
                    }
                }
            }
            "--exclude-tools" | "-xt" if next.is_some() => {
                result.exclude_tools = args.next().map(split_csv);
            }
            "--thinking" if next.is_some() => {
                if let Some(value) = args.next() {
                    if let Some(level) = ThinkingLevel::ALL
                        .into_iter()
                        .find(|level| level.as_str() == value)
                    {
                        result.thinking = Some(level);
                    } else {
                        result.diagnostic(Warning, format!("Invalid thinking level \"{value}\". Valid values: off, minimal, low, medium, high, xhigh, max"));
                    }
                }
            }
            "--print" | "-p" => {
                result.print = true;
                if let Some(value) = next.filter(|value| {
                    !value.starts_with('@') && (!value.starts_with('-') || value.starts_with("---"))
                }) {
                    args.next();
                    result.messages.push(value.to_owned());
                }
            }
            "--export" if next.is_some() => result.export = args.next().map(str::to_owned),
            "--extension" | "-e" if next.is_some() => {
                result.extensions.extend(args.next().map(str::to_owned));
            }
            "--no-extensions" | "-ne" => result.no_extensions = true,
            "--no-mcp" => result.no_mcp = true,
            "--skill" if next.is_some() => result.skills.extend(args.next().map(str::to_owned)),
            "--prompt-template" if next.is_some() => {
                result
                    .prompt_templates
                    .extend(args.next().map(str::to_owned));
            }
            "--theme" if next.is_some() => result.themes.extend(args.next().map(str::to_owned)),
            "--use-theme" => {
                if let Some(value) = next.filter(|value| !value.starts_with('-')) {
                    args.next();
                    result.use_theme = Some(value.to_owned());
                } else {
                    result.diagnostic(Error, "--use-theme requires a theme name");
                }
            }
            "--no-skills" | "-ns" => result.no_skills = true,
            "--no-prompt-templates" | "-np" => result.no_prompt_templates = true,
            "--no-themes" => result.no_themes = true,
            "--no-context-files" | "-nc" => result.no_context_files = true,
            "--list-models" => {
                result.list_models =
                    Some(match next.filter(|value| !value.starts_with(['-', '@'])) {
                        Some(value) => {
                            args.next();
                            ListModels::Search(value.to_owned())
                        }
                        None => ListModels::All,
                    });
            }
            "--tui-mode" => {
                if let Some(value) = next.filter(|value| !value.starts_with('-')) {
                    args.next();
                    match value {
                        "regular" | "main" => result.tui_mode = Some(TuiMode::Regular),
                        "fullscreen" | "alt" => result.tui_mode = Some(TuiMode::Fullscreen),
                        _ => result.diagnostic(
                            Error,
                            format!(
                                "Invalid TUI mode \"{value}\". Valid values: regular, fullscreen"
                            ),
                        ),
                    }
                } else {
                    result.diagnostic(Error, "--tui-mode requires regular or fullscreen");
                }
            }
            "--verbose" => result.verbose = true,
            "--approve" | "-a" => result.project_trust_override = Some(true),
            "--no-approve" | "-na" => result.project_trust_override = Some(false),
            "--offline" => result.offline = true,
            "--no-refresh" => result.no_refresh = true,
            _ if arg.starts_with('@') => result.positional(arg),
            _ if arg.starts_with("--") => {
                let flag = &arg[2..];
                let (name, value) = if let Some((name, value)) = flag.split_once('=') {
                    (name, FlagValue::Str(value.to_owned()))
                } else if let Some(value) = next.filter(|value| !value.starts_with(['-', '@'])) {
                    args.next();
                    (flag, FlagValue::Str(value.to_owned()))
                } else {
                    (flag, FlagValue::Bool(true))
                };
                result.unknown_flags.insert(name.to_owned(), value);
            }
            _ if arg.starts_with('-') => result.diagnostic(Error, format!("Unknown option: {arg}")),
            _ => result.positional(arg),
        }
    }
    result
}

/// The default unstyled help output, including the blank line emitted by the upstream CLI.
pub fn help_text() -> &'static str {
    HELP
}

/// Add extension flags in registration order without changing the base help text.
pub fn help_text_with_extensions(flags: &[FlagDefinition]) -> String {
    if flags.is_empty() {
        return HELP.to_owned();
    }
    let (before, after) = HELP.split_once("\n\nExamples:").unwrap_or((HELP, ""));
    let mut output = String::with_capacity(HELP.len() + 22 + 60 * flags.len());
    output.push_str(before);
    output.push_str("\nExtension CLI Flags:\n");
    for flag in flags {
        let value = if flag.kind == FlagKind::String {
            " <value>"
        } else {
            ""
        };
        let width = 4 + flag.name.encode_utf16().count() + value.len();
        let _ = write!(
            output,
            "  --{}{value}{:width$}",
            flag.name,
            "",
            width = 30_usize.saturating_sub(width)
        );
        if let Some(description) = &flag.description {
            output.push_str(description);
        } else {
            let _ = write!(output, "Registered by {}", flag.extension_path);
        }
        output.push('\n');
    }
    output.push_str("\n\nExamples:");
    output.push_str(after);
    output
}

const HELP: &str = r#"pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [--] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
  pi list                      List installed extensions from settings
  pi config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  pi auth <command>            Log in or out, print credentials, or check provider readiness
  pi mcp <command>             Manage MCP servers
  pi <command> --help          Show help for install/remove/uninstall/update/list/config/auth/mcp

Options:
  --provider <name>              Provider to search for --model (requires --model)
  --model <pattern>              Model pattern or ID (supports "provider/id" and optional ":<thinking>")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue the most recent session for the current directory
  --resume, -r                   Pick a session to resume
  --session <path|id>            Open a session by file path or full/partial ID
  --session-id <id>              Use the current directory's existing session with the ID, creating it otherwise
  --fork <path|id>               Fork a session by file path or full/partial ID into the current directory
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names or patterns (*) to enable
                                 Keeps MCP tools unless an entry starts with mcp__
                                 Only +name/-name entries add to or remove from the defaults
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names or patterns (*) to disable
                                 Applies to all tools, MCP tools included
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file or builtin:<name> (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery and built-in extensions (explicit -e paths still work)
  --no-mcp                       Disable built-in MCP support: no servers connect and no MCP tools
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --use-theme <name[/name]>      Set the initial interactive theme for this run
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file> [output]       Export session file to HTML and exit
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --tui-mode <mode>              TUI mode: fullscreen (default) or regular
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PI_OFFLINE=1)
  --no-refresh                   Disable startup background model catalog refresh
  --                             End option parsing; treat remaining arguments as messages/files
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).

Examples:
  # Print a provider API key for an external client
  pi auth print-api-key --provider openai

  # Print an OAuth bearer token for an external client (refreshes if expired)
  pi auth print-bearer-token --provider openai-codex

  # Interactive mode
  pi

  # Interactive mode with initial prompt
  pi "List all .ts files in src/"

  # Include files in initial message
  pi @prompt.md @image.png "What color is the sky?"

  # Non-interactive mode (process and exit)
  pi -p "List all .ts files in src/"

  # Prompt beginning with a dash
  pi -p -- "- Summarize these points"

  # Multiple messages (interactive)
  pi "Read package.json" "What dependencies do we have?"

  # Continue previous session
  pi --continue "What did we discuss?"

  # Start a named session
  pi --name "Refactor auth module"

  # Use different model
  pi --provider openai --model gpt-4o-mini "Help me refactor this code"

  # Use model with provider prefix (no --provider needed)
  pi --model openai/gpt-4o "Help me refactor this code"

  # Use model with thinking level shorthand
  pi --model sonnet:high "Solve this complex problem"

  # Limit model cycling to specific models
  pi --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  pi --models "github-copilot/*"

  # Cycle models with fixed thinking levels
  pi --models sonnet:high,haiku:low

  # Start with a specific thinking level
  pi --thinking high "Solve this complex problem"

  # Read-only mode (no file modifications possible)
  pi --tools read,grep,find,ls -p "Review the code in src/"

  # Add codemode to the default tools
  pi --tools +codemode

  # Codemode with only the tools of one MCP server
  pi --tools read,bash,codemode,'mcp__radius__*'

  # Disable one tool while keeping the rest available
  pi --exclude-tools ask_question

  # Export a session file to HTML
  pi --export ~/.pi/agent/sessions/--path--/session.jsonl
  pi --export session.jsonl output.html

Environment Variables:
  ANTHROPIC_AUTH_TOKEN             - Anthropic bearer auth token
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI/Cognitive Services base URL (e.g. https://{resource}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  BASETEN_API_KEY                  - Baseten API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  META_API_KEY                     - Meta Model API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  QWEN_TOKEN_PLAN_TEAM_API_KEY     - Qwen Token Plan Team API key (international region)
  QWEN_TOKEN_PLAN_TEAM_CN_API_KEY  - Qwen Token Plan Team API key (China region)
  QWEN_TOKEN_PLAN_INDIVIDUAL_API_KEY - Qwen Token Plan Individual API key (international region)
  QWEN_TOKEN_PLAN_INDIVIDUAL_CN_API_KEY - Qwen Token Plan Individual API key (China region)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  PI_CODING_AGENT_DIR              - Config directory (default: ~/.pi/agent)
  PI_CODING_AGENT_SESSION_DIR      - Session storage directory (overridden by --session-dir)
  PI_PACKAGE_DIR                   - Override package directory (for Nix/Guix store paths)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes
  PI_SELF_UPDATE_COMMAND           - Command pi update runs to update pi itself ({packageName}, {version}, {installSpec})
  PI_TELEMETRY                     - Override install telemetry when set to 1/true/yes or 0/false/no
  PI_SHARE_VIEWER_URL              - Base URL for /share command (default: https://pi.dev/session/)

Built-in Tool Names:
  read       - Read file contents
  bash       - Execute bash commands
  powershell - Execute PowerShell commands on Windows
  edit       - Edit files with find/replace
  write      - Write files (creates/overwrites)
  grep       - Search file contents (read-only, off by default)
  find       - Find files by glob pattern (read-only, off by default)
  ls         - List directory contents (read-only, off by default)

"#;
