#![forbid(unsafe_code)]

use std::error::Error;
use std::fs;
use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};
use pi_rs_ai::{Api, BaseModel, Model, ModelInputModality, ThinkingLevel};
use pi_rs_coding_agent::cli::args::{
    Args, Diagnostic, DiagnosticKind, FlagValue, ListModels, Mode, help_text,
    help_text_with_extensions, normalize_session_name, parse_args,
};
use pi_rs_coding_agent::cli::file_processor::{FileError, ProcessedFiles, process_file_arguments};
use pi_rs_coding_agent::cli::initial_message::{build_initial_message, prepare_initial_message};
use pi_rs_coding_agent::cli::list_models::format_models;
use pi_rs_coding_agent::cli::project_trust::create_project_trust_context;
use pi_rs_coding_agent::core::extensions::context::UiInject;
use pi_rs_coding_agent::core::extensions::types::{
    ExtensionMode, ExtensionUi, FlagDefinition, FlagKind,
};
use pi_rs_coding_agent::core::project_trust::AppMode;
use pi_rs_tui::TuiMode;
use serde_json::json;

fn parse(args: &[&str]) -> Args {
    parse_args(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn print_preserves_frontmatter_and_option_terminator() {
    let frontmatter = "---\ntitle: hello\n---\nSay hi.";
    let parsed = parse(&[
        "-p",
        frontmatter,
        "--print",
        "@before",
        "-p",
        "--provider",
        "openai",
        "--",
        "--model",
        "- leading dash",
        "@after",
        "--",
        "@",
        "-",
    ]);
    assert!(parsed.print);
    assert_eq!(parsed.provider.as_deref(), Some("openai"));
    assert_eq!(parsed.model, None);
    assert_eq!(
        parsed.messages,
        [frontmatter, "--model", "- leading dash", "--", "-"]
    );
    assert_eq!(parsed.file_args, ["before", "after", ""]);
    assert!(parsed.unknown_flags.is_empty());
    assert!(parsed.diagnostics.is_empty());
}

#[test]
fn unknown_flags_keep_map_order_and_do_not_consume_files() {
    let parsed = parse(&[
        "--plan",
        "--label=one=two",
        "--bucket",
        "chosen",
        "--optional",
        "@pic",
        "--label=",
        "--model=literal",
        "--=blank-name",
        "--provider",
    ]);
    let flags: Vec<_> = parsed.unknown_flags.into_iter().collect();
    assert_eq!(
        flags,
        [
            ("plan".into(), FlagValue::Bool(true)),
            ("label".into(), FlagValue::Str(String::new())),
            ("bucket".into(), FlagValue::Str("chosen".into())),
            ("optional".into(), FlagValue::Bool(true)),
            ("model".into(), FlagValue::Str("literal".into())),
            (String::new(), FlagValue::Str("blank-name".into())),
            ("provider".into(), FlagValue::Bool(true)),
        ]
    );
    assert_eq!(
        parse(&["--label=one=two"]).unknown_flags["label"],
        FlagValue::Str("one=two".into())
    );
    assert!(parsed.messages.is_empty());
    assert_eq!(parsed.file_args, ["pic"]);
    assert!(parsed.diagnostics.is_empty());
}

#[test]
fn diagnostics_preserve_source_order_and_preceding_valid_values() {
    use DiagnosticKind::{Error, Warning};

    let parsed = parse(&[
        "--mode",
        "json",
        "--mode",
        "yaml",
        "--mode",
        "--version",
        "--thinking",
        "max",
        "--thinking",
        "ultra",
        "--use-theme",
        "--help",
        "--tui-mode",
        "regular",
        "--tui-mode",
        "other",
        "--tui-mode",
        "-z",
        "--tools",
        "+codemode,-write",
        "-t",
        "read,+grep",
        "--tools",
        "+mcp__s__*",
        "--name",
    ]);
    assert!(parsed.version && parsed.help);
    assert_eq!(parsed.mode, Some(Mode::Json));
    assert_eq!(parsed.thinking, Some(ThinkingLevel::Max));
    assert_eq!(parsed.tui_mode, Some(TuiMode::Regular));
    assert_eq!(
        parsed.tools,
        Some(vec!["+codemode".into(), "-write".into()])
    );
    assert_eq!(parsed.diagnostics, [
        (Error, "Invalid mode \"yaml\". Valid values: text, json, rpc"),
        (Error, "--mode requires text, json, or rpc"),
        (Warning, "Invalid thinking level \"ultra\". Valid values: off, minimal, low, medium, high, xhigh, max"),
        (Error, "--use-theme requires a theme name"),
        (Error, "Invalid TUI mode \"other\". Valid values: regular, fullscreen"),
        (Error, "--tui-mode requires regular or fullscreen"),
        (Error, "Unknown option: -z"),
        (Error, "-t: tool names cannot be mixed with +name or -name entries"),
        (Error, "--tools: +name and -name entries take exact tool names, not patterns: +mcp__s__*"),
        (Error, "--name requires a value"),
    ].map(|(kind, message)| Diagnostic { kind, message: message.into() }));
    assert!(parsed.messages.is_empty());
    assert!(parsed.unknown_flags.is_empty());
}

#[test]
fn parsing_distinguishes_repeated_values_empty_lists_and_boolean_overrides() {
    let parsed = parse(&[
        "-v",
        "-h",
        "-c",
        "-r",
        "--no-session",
        "--session-id",
        "orch-123",
        "--session-dir",
        "sessions",
        "--session",
        "old.jsonl",
        "--fork",
        "branch",
        "--api-key",
        "key",
        "--system-prompt",
        "system",
        "--append-system-prompt",
        "first",
        "--append-system-prompt",
        "@raw",
        "--models",
        "a, , \u{feff}b,",
        "-n",
        "--provider",
        "-nt",
        "-nbt",
        "-xt",
        ", ",
        "-e",
        "a.wasm",
        "--extension",
        "builtin:b",
        "-ne",
        "--no-mcp",
        "--skill",
        "a",
        "-ns",
        "--prompt-template",
        "b",
        "-np",
        "--theme",
        "c",
        "--use-theme",
        "vendor/name",
        "--no-themes",
        "-nc",
        "--verbose",
        "--offline",
        "-a",
        "-na",
        "--list-models",
        "OpenAI/gpt",
        "--export",
        "session.jsonl",
        "output.html",
    ]);
    assert!(
        parsed.help && parsed.version && parsed.is_continue && parsed.resume && parsed.no_session
    );
    assert_eq!(parsed.session_id.as_deref(), Some("orch-123"));
    assert_eq!(parsed.session_dir.as_deref(), Some("sessions"));
    assert_eq!(parsed.session.as_deref(), Some("old.jsonl"));
    assert_eq!(parsed.fork.as_deref(), Some("branch"));
    assert_eq!(parsed.api_key.as_deref(), Some("key"));
    assert_eq!(parsed.system_prompt.as_deref(), Some("system"));
    assert_eq!(parsed.append_system_prompt, ["first", "@raw"]);
    assert_eq!(parsed.models, Some(vec!["a".into(), "b".into()]));
    assert_eq!(parsed.name.as_deref(), Some("--provider"));
    assert_eq!(parsed.provider, None);
    assert_eq!(parsed.exclude_tools, Some(Vec::new()));
    assert!(parsed.no_tools && parsed.no_builtin_tools && parsed.no_extensions && parsed.no_mcp);
    assert_eq!(parsed.extensions, ["a.wasm", "builtin:b"]);
    assert_eq!(parsed.skills, ["a"]);
    assert_eq!(parsed.prompt_templates, ["b"]);
    assert_eq!(parsed.themes, ["c"]);
    assert_eq!(parsed.use_theme.as_deref(), Some("vendor/name"));
    assert!(
        parsed.no_skills
            && parsed.no_prompt_templates
            && parsed.no_themes
            && parsed.no_context_files
    );
    assert!(parsed.verbose && parsed.offline);
    assert_eq!(parsed.project_trust_override, Some(false));
    assert_eq!(
        parsed.list_models,
        Some(ListModels::Search("OpenAI/gpt".into()))
    );
    assert_eq!(parsed.export.as_deref(), Some("session.jsonl"));
    assert_eq!(parsed.messages, ["output.html"]);
    assert!(parsed.diagnostics.is_empty());
    assert_eq!(
        parse(&["--list-models", "@file"]).list_models,
        Some(ListModels::All)
    );
    assert_eq!(parse(&["--mode", "rpc"]).mode, Some(Mode::Rpc));
    assert_eq!(parse(&["--mode", "text"]).mode, Some(Mode::Text));
    assert_eq!(
        parse(&["--no-approve", "--approve"]).project_trust_override,
        Some(true)
    );
    assert_eq!(
        normalize_session_name("\u{feff}\t named session \u{00a0}"),
        Some("named session")
    );
    assert_eq!(normalize_session_name("  \u{feff} \n"), None);
    assert_eq!(normalize_session_name("\u{0085}"), Some("\u{0085}"));
}

#[test]
fn help_inserts_extension_flag_columns_in_registration_order() {
    let plain = help_text();
    assert!(plain.starts_with("pi - AI coding assistant with read, bash, edit, write tools\n\nUsage:\n  pi [options] [--] [@files...] [messages...]\n"));
    assert!(plain.contains("  PI_CODING_AGENT_SESSION_DIR      - Session storage directory (overridden by --session-dir)\n"));
    assert!(
        plain.ends_with("  ls         - List directory contents (read-only, off by default)\n\n")
    );
    assert_eq!(help_text_with_extensions(&[]), plain);
    let flags = [
        FlagDefinition {
            name: "plan".into(),
            description: Some("Plan mode".into()),
            kind: FlagKind::Boolean,
            default: None,
            extension_path: "plan.wasm".into(),
        },
        FlagDefinition {
            name: "label".into(),
            description: None,
            kind: FlagKind::String,
            default: None,
            extension_path: "labels.wasm".into(),
        },
    ];
    let styled = help_text_with_extensions(&flags);
    let flag_rows = concat!(
        "\nExtension CLI Flags:\n",
        "  --plan                      Plan mode\n",
        "  --label <value>             Registered by labels.wasm\n",
    );
    assert!(styled.contains(&format!("extension).{flag_rows}\n\nExamples:")));
    assert_eq!(styled.replace(flag_rows, ""), plain);
}

const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

#[test]
fn file_text_and_magic_byte_images_merge_before_only_the_first_message()
-> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let note = dir.path().join("Screenshot\u{202f}PM.txt");
    let picture = dir.path().join("picture.dat");
    fs::write(&note, b"\xef\xbb\xbfnotes\xff")?;
    fs::write(dir.path().join("empty"), [])?;
    fs::write(&picture, STANDARD.decode(TINY_PNG)?)?;
    let mut parsed = parse(&[
        "-p",
        "@Screenshot PM.txt",
        "@empty",
        "@picture.dat",
        "first",
        "later",
    ]);
    let prompt = prepare_initial_message(&mut parsed, dir.path(), Some("stdin\n".into()))?;
    assert_eq!(
        prompt.initial_message,
        Some(format!(
            "stdin\n<file name=\"{}\">\nnotes\u{fffd}\n</file>\n<file name=\"{}\"></file>\nfirst",
            note.display(),
            picture.display(),
        ))
    );
    let images = prompt.initial_images.ok_or("missing image attachment")?;
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(images[0].data, TINY_PNG);
    assert_eq!(parsed.messages, ["later"]);
    Ok(())
}

#[test]
fn file_conversion_and_missing_paths_report_what_happened() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let bmp = dir.path().join("test.bmp");
    let mut bitmap = [0; 58];
    bitmap[..2].copy_from_slice(b"BM");
    bitmap[2..6].copy_from_slice(&58_u32.to_le_bytes());
    bitmap[10..14].copy_from_slice(&54_u32.to_le_bytes());
    bitmap[14..18].copy_from_slice(&40_u32.to_le_bytes());
    bitmap[18..22].copy_from_slice(&1_i32.to_le_bytes());
    bitmap[22..26].copy_from_slice(&1_i32.to_le_bytes());
    bitmap[26..28].copy_from_slice(&1_u16.to_le_bytes());
    bitmap[28..30].copy_from_slice(&24_u16.to_le_bytes());
    bitmap[34..38].copy_from_slice(&4_u32.to_le_bytes());
    bitmap[56] = 255;
    fs::write(&bmp, bitmap)?;
    let files = process_file_arguments(&["test.bmp".into()], dir.path(), false)?;
    assert_eq!(files.images.len(), 1);
    assert_eq!(files.images[0].mime_type, "image/png");
    assert!(
        STANDARD
            .decode(&files.images[0].data)?
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    assert_eq!(
        files.text,
        format!(
            "<file name=\"{}\">[Image converted from image/bmp to image/png.]</file>\n",
            bmp.display(),
        )
    );
    let corrupt = dir.path().join("corrupt.bmp");
    fs::write(&corrupt, &bitmap[..54])?;
    let omitted = process_file_arguments(&["corrupt.bmp".into()], dir.path(), false)?;
    assert!(omitted.images.is_empty());
    assert_eq!(
        omitted.text,
        format!(
            "<file name=\"{}\">[Image omitted: could not be converted to a supported inline image format.]</file>\n",
            corrupt.display(),
        )
    );
    let missing = process_file_arguments(&["missing.txt".into()], dir.path(), false);
    assert!(
        matches!(&missing, Err(FileError::NotFound(path)) if *path == dir.path().join("missing.txt"))
    );
    assert_eq!(
        missing.err().map(|error| error.to_string()),
        Some(format!(
            "File not found: {}",
            dir.path().join("missing.txt").display(),
        ))
    );
    Ok(())
}

#[test]
fn initial_message_distinguishes_no_inputs_and_an_explicit_empty_input() {
    let mut parsed = Args::default();
    let absent = build_initial_message(&mut parsed, ProcessedFiles::default(), None);
    assert_eq!(absent.initial_message, None);
    assert_eq!(absent.initial_images, None);
    let empty = build_initial_message(&mut parsed, ProcessedFiles::default(), Some(String::new()));
    assert_eq!(empty.initial_message, Some(String::new()));
    let mut parsed = parse(&["-p", ""]);
    let empty = build_initial_message(&mut parsed, ProcessedFiles::default(), None);
    assert_eq!(empty.initial_message, Some(String::new()));
    assert!(parsed.messages.is_empty());
}

fn model(provider: &str, id: &str, context: u64, max_tokens: u64, capabilities: bool) -> Model {
    Model {
        base: BaseModel {
            id: id.into(),
            name: id.into(),
            api: Api::OpenAiCompletions,
            provider: provider.into(),
            base_url: "https://example.invalid".into(),
            input: if capabilities {
                vec![ModelInputModality::Text, ModelInputModality::Image]
            } else {
                vec![ModelInputModality::Text]
            },
            input_limits: None,
            cost: Default::default(),
            headers: None,
        },
        kind: None,
        reasoning: capabilities,
        thinking_level_map: None,
        prompt_cache: None,
        context_window: context,
        max_tokens,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        compat: None,
    }
}

#[test]
fn model_list_matches_table_padding_order_and_number_rounding() {
    let models = [
        model("Beta", "omni", 999, 9, false),
        model("alpha", "a-1", 1_250_000, 1250, true),
        model("alpha", "a_1", 999_999, 1150, false),
    ];
    let docs = Path::new("docs");
    let header = "provider  model  context  max-out  thinking  images\n";
    assert_eq!(
        format_models(&models, None, docs),
        format!(
            "{header}alpha     a_1    1000.0K  1.1K     no        no    \n\
         alpha     a-1    1.3M     1.3K     yes       yes   \n\
         Beta      omni   999      9        no        no    \n"
        )
    );
    assert_eq!(
        format_models(&models, Some("BETA/omni"), docs),
        format!("{header}Beta      omni   999      9        no        no    \n")
    );
    assert_eq!(
        format_models(&models, Some("nothing"), docs),
        "No models matching \"nothing\"\n"
    );
    assert_eq!(
        format_models(&[], Some("nothing"), docs),
        format!(
            "No models available. Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}\n",
            docs.join("providers.md").display(),
            docs.join("models.md").display(),
        )
    );
}

#[tokio::test]
async fn project_trust_dialogs_require_interactive_startup_ui() {
    let cwd = Path::new("project");
    let startup_ui = ExtensionUi::from_inject(
        ExtensionMode::Tui,
        UiInject::fixed(Some("Yes".into()), false, Some("input".into())),
    );
    let dialogs = create_project_trust_context(cwd, AppMode::Interactive, true)
        .with_startup_ui(startup_ui.clone());
    assert_eq!(
        dialogs.extension_event(),
        json!({"type":"project_trust","cwd":"project","mode":"tui","hasUI":true})
    );
    assert!(dialogs.confirm("Trust", "Allow project extensions?").await);
    assert_eq!(
        dialogs.input("Label".into(), None).await.as_deref(),
        Some("input")
    );
    for (mode, expected_mode, has_ui) in [
        (AppMode::Interactive, "tui", false),
        (AppMode::Print, "print", false),
        (AppMode::Json, "json", false),
        (AppMode::Rpc, "rpc", true),
    ] {
        let noninteractive =
            create_project_trust_context(cwd, mode, has_ui).with_startup_ui(startup_ui.clone());
        assert_eq!(noninteractive.extension_event()["mode"], expected_mode);
        assert_eq!(noninteractive.extension_event()["hasUI"], has_ui);
        assert_eq!(
            noninteractive
                .select("Trust".into(), vec!["Yes".into()])
                .await,
            None
        );
        assert!(
            !noninteractive
                .confirm("Trust", "Allow project extensions?")
                .await
        );
        assert_eq!(noninteractive.input("Label".into(), None).await, None);
    }
}
