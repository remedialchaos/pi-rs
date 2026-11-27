//! Native `pi` entry point.

#![forbid(unsafe_code)]

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use pi_rs_coding_agent::{
    cli::{
        args::{
            Args, DiagnosticKind, ListModels, Mode, help_text_with_extensions,
            normalize_session_name, parse_args,
        },
        auth_command,
        initial_message::prepare_initial_message,
        list_models::format_models,
        mcp::run_command as mcp_command,
        session_picker, setup, startup_ui,
    },
    config::{self, ENV_SESSION_DIR, VERSION},
    core::{
        agent_session::SessionReason,
        agent_session_runtime::{DiagnosticLevel, RuntimeTarget, create_agent_session_runtime},
        agent_session_services::{DiskSession, create_factory, export_to_html},
        auth_guidance::{docs_path, no_models_available},
        crash_log,
        project_trust::AppMode,
        session_cwd::missing_session_cwd,
        session_manager::{
            SessionInfo, SessionManager, assert_valid_session_id, get_default_session_dir,
        },
        settings_diagnostics::{collect_settings_diagnostics, deduplicate_diagnostics},
        settings_manager::{CreateOptions, SettingsManager},
        timings,
    },
    migrations,
    modes::{self, diagnostics_stderr::write_diagnostics_to_stderr, print::PrintOptions},
    package_manager_cli,
};
use tokio::io::AsyncReadExt;

fn main() -> ExitCode {
    setup::setup_cli();
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(&args));
    // Tokio stdin waits in a blocking read. A signal or RPC shutdown can end pi
    // before the client closes stdin, so do not wait for that read on exit.
    runtime.shutdown_background();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(argv: &[String]) -> Result<u8, String> {
    let cwd = std::env::current_dir().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => "The current working directory no longer exists. Change to an existing directory and try again.".to_owned(),
        _ => error.to_string(),
    })?;
    let agent_dir = config::get_agent_dir()?;
    install_crash_panic_hook(&agent_dir, &cwd);
    if let Some(code) = auth_command::run(argv, &cwd, &agent_dir).await {
        return Ok(code);
    }
    package_manager_cli::cleanup_installation();
    if let Some(code) = package_manager_cli::run(argv, &cwd, &agent_dir).await {
        return Ok(code);
    }
    if let Some(code) = mcp_command(argv, &cwd, &agent_dir).await {
        return Ok(code);
    }

    let mut args = parse_args(argv);
    for diagnostic in &args.diagnostics {
        let prefix = match diagnostic.kind {
            DiagnosticKind::Warning => "Warning",
            DiagnosticKind::Error => "Error",
        };
        eprintln!("{prefix}: {}", diagnostic.message);
    }
    if args
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.kind == DiagnosticKind::Error)
    {
        return Ok(1);
    }
    if args.version {
        println!("{VERSION}");
        return Ok(0);
    }
    if let Some(input) = &args.export {
        let input = config::resolve_path(input, &cwd, true)?;
        if !input.exists() {
            return Err(format!("File not found: {}", input.display()));
        }
        let session = SessionManager::open(input)?;
        let output = export_to_html(&session, args.messages.first().map(String::as_str), None)?;
        println!("Exported to: {output}");
        return Ok(0);
    }

    let stdin_tty = io::stdin().is_terminal();
    let mode = match args.mode {
        Some(Mode::Rpc) => AppMode::Rpc,
        Some(Mode::Json) => AppMode::Json,
        _ if args.print || !stdin_tty || !io::stdout().is_terminal() => AppMode::Print,
        _ => AppMode::Interactive,
    };
    if mode == AppMode::Rpc && !args.file_args.is_empty() {
        return Err("@file arguments are not supported in RPC mode".to_owned());
    }
    validate_session_flags(&args)?;
    let migration_result = migrations::run_migrations(&cwd);
    if mode != AppMode::Interactive && !migration_result.deprecation_warnings.is_empty() {
        for warning in
            migrations::format_deprecation_warnings(&migration_result.deprecation_warnings)
        {
            eprintln!("{warning}");
        }
    }
    let mut startup_settings =
        SettingsManager::create_with_dirs(&cwd, &agent_dir, CreateOptions::default());
    if mode == AppMode::Interactive
        && !args.help
        && args.list_models.is_none()
        && startup_ui::should_run_first_time_setup(&agent_dir.join("settings.json"))
    {
        startup_ui::show_first_time_setup(&mut startup_settings, &agent_dir).await?;
    }
    let mut diagnostics = collect_settings_diagnostics(&mut startup_settings);
    if mode == AppMode::Interactive
        && let Some(theme) = &args.use_theme
    {
        startup_settings.apply_overrides(serde_json::json!({ "theme": theme }));
    }
    let session_dir = args
        .session_dir
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .map(|dir| config::normalize_path(dir, true))
        .or_else(|| {
            std::env::var(ENV_SESSION_DIR)
                .ok()
                .filter(|dir| !dir.is_empty())
                .map(|dir| config::expand_tilde_path(&dir))
        })
        .or_else(|| startup_settings.get_session_dir())
        .map(PathBuf::from);
    let Some(mut manager) = select_session(
        &args,
        &cwd,
        session_dir.clone(),
        &startup_settings,
        &agent_dir,
    )
    .await?
    else {
        return Ok(0);
    };
    if let Some(issue) = missing_session_cwd(&manager, &cwd) {
        if mode != AppMode::Interactive {
            return Err(issue.to_string());
        }
        let choice = startup_ui::show_startup_selector(
            &startup_settings,
            &agent_dir,
            issue.to_string(),
            vec![
                ("Continue in the current working directory".into(), false),
                ("Fork into the current working directory".into(), true),
            ],
        )
        .await?;
        manager = match choice {
            None => return Ok(0),
            Some(false) => SessionManager::open_with(issue.session_file, session_dir, Some(&cwd))?,
            Some(true) => SessionManager::fork_from(issue.session_file, &cwd, session_dir, None)?,
        };
    }
    if let Some(name) = &args.name {
        let name = normalize_session_name(name).ok_or("--name requires a non-empty value")?;
        manager.append_session_info(name)?;
    }
    let target = RuntimeTarget {
        cwd: manager.get_cwd().to_owned(),
        agent_dir: Some(agent_dir.to_string_lossy().into_owned()),
        session_manager: Arc::new(DiskSession::new(manager)),
        reason: SessionReason::Startup,
    };
    let list = args.list_models.clone();
    let stdin_content = if args.help || list.is_some() || mode == AppMode::Rpc || stdin_tty {
        None
    } else {
        let mut bytes = Vec::new();
        tokio::io::stdin()
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        let stdin = String::from_utf8_lossy(&bytes);
        normalize_session_name(&stdin).map(str::to_owned)
    };
    let initial = if args.help || list.is_some() {
        None
    } else {
        Some(
            prepare_initial_message(&mut args, &cwd, stdin_content)
                .map_err(|error| error.to_string())?,
        )
    };
    let args = Arc::new(args);
    let startup_cwd = cwd.clone();
    let factory = create_factory(args.clone(), cwd, mode);
    let mut runtime = create_agent_session_runtime(factory, target)
        .await
        .map_err(|error| error.to_string())?;
    diagnostics.extend(runtime.take_unreported_diagnostics());
    deduplicate_diagnostics(&mut diagnostics);
    write_diagnostics_to_stderr(&diagnostics);
    if args.help {
        let flags: Vec<_> = runtime
            .services()
            .extensions
            .native()
            .map(|runner| runner.get_flags().into_values().collect())
            .unwrap_or_default();
        print!("{}", help_text_with_extensions(&flags));
        return Ok(0);
    }
    if let Some(list) = list {
        let search = match &list {
            ListModels::All => None,
            ListModels::Search(value) => Some(value.as_str()),
        };
        print!(
            "{}",
            format_models(&runtime.session().available_models(), search, &docs_path())
        );
        return Ok(0);
    }
    if runtime
        .diagnostics()
        .iter()
        .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
    {
        if runtime
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Failed to load extension"))
        {
            eprintln!(r#"Hint: Start without extensions using "pi -ne"."#);
        }
        return Ok(1);
    }
    if mode != AppMode::Interactive && runtime.session().model().provider == "unknown" {
        eprintln!("{}", no_models_available());
        return Ok(1);
    }
    let startup_benchmark =
        std::env::var("PI_STARTUP_BENCHMARK").is_ok_and(|value| config::is_truthy_value(&value));
    if startup_benchmark && mode != AppMode::Interactive {
        return Err("PI_STARTUP_BENCHMARK only supports interactive mode".to_owned());
    }
    if mode == AppMode::Interactive
        && !startup_benchmark
        && !migration_result.deprecation_warnings.is_empty()
    {
        migrations::show_deprecation_warnings(
            &migration_result.deprecation_warnings,
            stdin_tty && io::stdout().is_terminal(),
        )
        .await;
    }
    let unnotified_crash = if mode == AppMode::Interactive {
        crash_log::take_unnotified_crash(&agent_dir)
    } else {
        None
    };
    timings::print_timings();
    let _catalog_refresh = pi_rs_coding_agent::core::startup::spawn_startup_catalog_refresh(
        pi_rs_coding_agent::core::startup::StartupRefreshPolicy::from_args(
            &args,
            mode,
            startup_benchmark,
        ),
        agent_dir.clone(),
        startup_cwd,
        pi_rs_coding_agent::core::startup::default_catalog_refresher(),
    );
    match mode {
        AppMode::Rpc => modes::rpc::run(&mut runtime)
            .await
            .map_err(|error| error.to_string()),
        AppMode::Print | AppMode::Json => {
            let (initial_message, initial_images) = initial.map_or((None, Vec::new()), |initial| {
                (
                    initial.initial_message,
                    initial.initial_images.unwrap_or_default(),
                )
            });
            modes::print::run(
                &mut runtime,
                PrintOptions {
                    json: mode == AppMode::Json,
                    initial_message,
                    initial_images,
                    messages: args.messages.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string())
        }
        AppMode::Interactive => {
            modes::interactive::run(
                runtime,
                modes::interactive::InteractiveOptions {
                    agent_dir,
                    tui_mode: args.tui_mode,
                    theme: args.use_theme.clone(),
                    project_trust_override: args.project_trust_override,
                    initial: initial.unwrap_or_default(),
                    messages: args.messages.clone(),
                    offline: args.offline,
                    no_refresh: args.no_refresh,
                    startup_benchmark,
                    migrated_auth_providers: migration_result.migrated_auth_providers,
                    unnotified_crash,
                },
            )
            .await
        }
    }
}

fn install_crash_panic_hook(agent_dir: &Path, cwd: &Path) {
    let agent_dir = agent_dir.to_path_buf();
    let cwd_str = cwd.to_string_lossy().into_owned();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(msg) = info.payload().downcast_ref::<&str>() {
            (*msg).to_owned()
        } else if let Some(msg) = info.payload().downcast_ref::<String>() {
            msg.clone()
        } else {
            "panic".to_owned()
        };
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));
        let backtrace = std::backtrace::Backtrace::capture().to_string();
        let stack = match location {
            Some(loc) => Some(format!("{loc}\n{backtrace}")),
            None => Some(backtrace),
        };
        let _ = crash_log::record_crash(
            &agent_dir,
            crash_log::CrashKind::UncaughtException,
            message,
            stack,
            None,
            &cwd_str,
        );
        previous(info);
    }));
}

fn validate_session_flags(args: &Args) -> Result<(), String> {
    let mut conflicts = Vec::new();
    if args.session.as_ref().is_some_and(|arg| !arg.is_empty()) {
        conflicts.push("--session");
    }
    if args.is_continue {
        conflicts.push("--continue");
    }
    if args.resume {
        conflicts.push("--resume");
    }
    if args.fork.as_ref().is_some_and(|arg| !arg.is_empty()) {
        if args.no_session {
            conflicts.push("--no-session");
        }
        if !conflicts.is_empty() {
            return Err(format!(
                "--fork cannot be combined with {}",
                conflicts.join(", ")
            ));
        }
    }
    if let Some(id) = &args.session_id {
        if !conflicts.is_empty() {
            return Err(format!(
                "--session-id cannot be combined with {}",
                conflicts.join(", ")
            ));
        }
        assert_valid_session_id(id)?;
    }
    Ok(())
}

async fn select_session(
    args: &Args,
    cwd: &Path,
    dir: Option<PathBuf>,
    startup_settings: &SettingsManager,
    agent_dir: &Path,
) -> Result<Option<SessionManager>, String> {
    let id = args.session_id.as_deref();
    if args.no_session || args.help || args.list_models.is_some() {
        return SessionManager::in_memory_with_entries(cwd, id, Vec::new()).map(Some);
    }
    let local_dir = dir.clone().unwrap_or_else(|| get_default_session_dir(cwd));
    let exact = id.and_then(|id| SessionManager::find_by_id(&local_dir, id));
    if let Some(source) = args.fork.as_deref().filter(|source| !source.is_empty()) {
        if exact.is_some() {
            return Err(format!(
                "Session already exists with id '{}'",
                id.unwrap_or_default()
            ));
        }
        let (path, _) = locate_session(source, cwd, dir.as_deref())?;
        return SessionManager::fork_from(path, cwd, dir, id).map(Some);
    }
    if let Some(source) = args.session.as_deref().filter(|source| !source.is_empty()) {
        let (path, other_cwd) = locate_session(source, cwd, dir.as_deref())?;
        if let Some(other_cwd) = other_cwd {
            println!("Session found in different project: {other_cwd}");
            print!("Fork this session into current directory? [y/N] ");
            io::stdout().flush().map_err(|error| error.to_string())?;
            let mut answer = String::new();
            use tokio::io::AsyncBufReadExt;
            tokio::io::BufReader::new(tokio::io::stdin())
                .read_line(&mut answer)
                .await
                .map_err(|error| error.to_string())?;
            if !answer.trim().eq_ignore_ascii_case("y")
                && !answer.trim().eq_ignore_ascii_case("yes")
            {
                println!("Aborted.");
                return Ok(None);
            }
            return SessionManager::fork_from(path, cwd, dir, None).map(Some);
        }
        return SessionManager::open_with(path, dir, None).map(Some);
    }
    if args.resume {
        let selected =
            session_picker::select_session(cwd, dir.clone(), startup_settings, agent_dir).await?;
        if selected.is_none() {
            println!("No session selected");
        }
        return selected
            .map(|info| SessionManager::open_with(info.path, dir, None))
            .transpose();
    }
    if args.is_continue {
        return SessionManager::continue_recent(cwd, dir).map(Some);
    }
    if let Some(path) = exact {
        return SessionManager::open_with(path, dir, None).map(Some);
    }
    if let Some(id) = id {
        eprintln!(
            "Warning: No project session found with id '{id}'; creating a new session with that id."
        );
    }
    SessionManager::create_in(cwd, local_dir, id).map(Some)
}

fn locate_session(
    source: &str,
    cwd: &Path,
    dir: Option<&Path>,
) -> Result<(PathBuf, Option<String>), String> {
    if source.contains(['/', '\\']) || source.ends_with(".jsonl") {
        return config::resolve_path(source, cwd, true).map(|path| (path, None));
    }
    let local_dir = dir.map_or_else(|| get_default_session_dir(cwd), Path::to_path_buf);
    if let Some(path) = SessionManager::find_by_id(&local_dir, source) {
        return Ok((path, None));
    }
    if let Some(session) = SessionManager::list(cwd, dir.map(Path::to_path_buf))?
        .into_iter()
        .find(|session| session.id.starts_with(source))
    {
        return Ok((session.path, None));
    }
    let mut sessions = match dir {
        Some(dir) => SessionManager::list_all(dir)?,
        None => {
            let mut sessions = Vec::new();
            let root = config::get_sessions_dir()?;
            if let Ok(dirs) = std::fs::read_dir(root) {
                for entry in dirs.flatten().filter(|entry| entry.path().is_dir()) {
                    sessions.extend(SessionManager::list_all(entry.path())?);
                }
            }
            sessions
                .sort_by_key(|session: &SessionInfo| std::cmp::Reverse(session.modified_millis));
            sessions
        }
    };
    let index = sessions
        .iter()
        .position(|session| session.id == source)
        .or_else(|| {
            sessions
                .iter()
                .position(|session| session.id.starts_with(source))
        });
    if let Some(index) = index {
        let session = sessions.swap_remove(index);
        return Ok((session.path, Some(session.cwd)));
    }
    Err(format!("No session found matching '{source}'"))
}
