//! The `p3` command. `p3 server` runs a Pi's server in the foreground (closed by SIGINT, SIGTERM or
//! the owner's stop); plain `p3` starts it in the background under its supervisor unless it runs, and
//! prints an owner link; `p3 stop` stops it; `p3 restart` stops, then starts it; `p3 status` says
//! whether it runs; `p3 list` lists the Pis. Whether a server runs: its data directory's lock is held
//! and `running.json` names its pid and port. The owner link and the stop are asked over HTTP with
//! the owner token file the sign-in plugin keeps.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use tokio::sync::Notify;

use crate::{
    client::{self, Request},
    files::{FileError, open_data_dir},
    install::{SUPERVISED_ENV, SUPERVISOR_ARG, WORKER_ARG, WorkerSpec, supervisor_args, version},
    pi::{PiChoice, data_dir_of, is_pi_name, keep_port, kept_port, named_pis, read_config},
    records::{running, supervisor_pid},
    server::{RunningServer, ServerOptions, start_server},
};

/// How long a background start may take.
const START_MS: u64 = 30_000;

/// How long a stop may take before the server is signalled, and then killed.
const STOP_MS: u64 = 10_000;

/// The usage text, as the command prints it.
pub const USAGE: &str = "Usage: p3 [command] [--pi <name> | --dir <path>] [--port <n>]

A Pi is its data directory: ~/.pi/p3 (the default Pi), ~/.pi/p3/pis/<name> (--pi) or any directory (--dir).

Commands:
  (none)     Start the Pi's server in the background unless it runs; print its port and an owner link (also: start)
  server     Run the Pi's server in the foreground
  status     Say whether the Pi's server runs: its port, pid, data directory, version and start time
  stop       Stop the Pi's server; returns once it is gone
  restart    Stop, then start in the background
  list       The Pis on this machine and their state

Options:
  --pi <name>   A named Pi (lower case letters, digits, - and _)
  --dir <path>  A Pi in any directory
  --port <n>    The port for this start, on 127.0.0.1 (0: a free one). Without it a Pi listens on its own port:
                8789 for the default Pi, else a free one taken at its first start and kept
  --version     Print p3's version
  --help        Print this help";

/// The command a line names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Server,
    Status,
    Stop,
    Restart,
    List,
}

/// A command line p3 understands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliCommand {
    pub action: Action,
    pub pi: Option<String>,
    pub dir: Option<String>,
    pub port: Option<u16>,
}

impl CliCommand {
    fn choice(&self) -> PiChoice<'_> {
        PiChoice {
            pi: self.pi.as_deref(),
            dir: self.dir.as_deref(),
        }
    }
}

/// What a command line asks the process to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Version,
    /// A session worker: the hidden mode the runner starts.
    Worker,
    /// The background server's supervisor: the hidden mode `p3` starts.
    Supervisor {
        data_dir: PathBuf,
        command: Vec<OsString>,
    },
    Command(CliCommand),
}

/// Why a command line is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Usage(pub String);

/// Parses a command line (the program's name included) into what the process should do.
pub fn parse(args: &[OsString]) -> Result<Invocation, Usage> {
    let mut args = args.iter().skip(1).cloned();
    let Some(first) = args.next() else {
        return Ok(Invocation::Command(CliCommand {
            action: Action::Start,
            pi: None,
            dir: None,
            port: None,
        }));
    };
    if first == WORKER_ARG {
        return Ok(Invocation::Worker);
    }
    if first == SUPERVISOR_ARG {
        let Some(data_dir) = args.next() else {
            return Err(Usage("the supervisor names no data directory".into()));
        };
        let command = args.collect::<Vec<_>>();
        if command.is_empty() {
            return Err(Usage("the supervisor names no server command".into()));
        }
        return Ok(Invocation::Supervisor {
            data_dir: PathBuf::from(data_dir),
            command,
        });
    }
    let mut pi = None;
    let mut dir = None;
    let mut port = None;
    let mut help = false;
    let mut show_version = false;
    let mut positionals = Vec::new();
    let mut rest = std::iter::once(first).chain(args).peekable();
    while let Some(argument) = rest.next() {
        let text = argument.to_string_lossy().into_owned();
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (text.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, Usage> {
            if let Some(inline) = inline.clone() {
                return Ok(inline);
            }
            match rest.next() {
                Some(value) => Ok(value.to_string_lossy().into_owned()),
                None => Err(Usage(format!("{name} names no value"))),
            }
        };
        match flag.as_str() {
            "--help" => help = true,
            "--version" => show_version = true,
            "--pi" => {
                if pi.is_some() {
                    return Err(Usage("--pi is named twice".into()));
                }
                pi = Some(value("--pi")?);
            }
            "--dir" => {
                if dir.is_some() {
                    return Err(Usage("--dir is named twice".into()));
                }
                dir = Some(value("--dir")?);
            }
            "--port" => {
                if port.is_some() {
                    return Err(Usage("--port is named twice".into()));
                }
                port = Some(value("--port")?);
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(Usage(format!("unknown option {other}")));
            }
            _ => positionals.push(text),
        }
    }
    if help {
        return Ok(Invocation::Help);
    }
    if show_version {
        return Ok(Invocation::Version);
    }
    let action = match positionals.first().map(String::as_str) {
        None | Some("start") => Action::Start,
        Some("server") => Action::Server,
        Some("status") => Action::Status,
        Some("stop") => Action::Stop,
        Some("restart") => Action::Restart,
        Some("list") => Action::List,
        Some(other) => return Err(Usage(format!("unknown command {other}"))),
    };
    if positionals.len() > 1 {
        return Err(Usage("too many arguments".into()));
    }
    if pi.is_some() && dir.is_some() {
        return Err(Usage("--pi and --dir name two Pis".into()));
    }
    if let Some(name) = &pi
        && !is_pi_name(name)
    {
        return Err(Usage(format!("{name} is no Pi's name")));
    }
    let starts = matches!(action, Action::Start | Action::Server | Action::Restart);
    let port = match port {
        None => None,
        Some(port) if starts => {
            let valid =
                !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit());
            let value = port.parse::<u32>().ok().filter(|value| *value <= 65535);
            match value.filter(|_| valid) {
                Some(value) => Some(value as u16),
                None => return Err(Usage(format!("{port} is no port"))),
            }
        }
        Some(_) => return Err(Usage("--port starts a server".into())),
    };
    if action == Action::List && (pi.is_some() || dir.is_some()) {
        return Err(Usage("list names no Pi".into()));
    }
    Ok(Invocation::Command(CliCommand {
        action,
        pi,
        dir,
        port,
    }))
}

/// Runs the process's command line and answers its exit code.
pub async fn run(args: Vec<OsString>) -> ExitCode {
    let invocation = match parse(&args) {
        Ok(invocation) => invocation,
        Err(usage) => {
            eprintln!("{}", USAGE);
            eprintln!("p3: {usage}");
            return ExitCode::from(2);
        }
    };
    let code = match invocation {
        Invocation::Help => {
            println!("{USAGE}");
            0
        }
        Invocation::Version => {
            println!("{}", version());
            0
        }
        Invocation::Worker => {
            // A worker finishes by exiting, as upstream's does: its runtime may still hold tasks
            // that would keep the process alive after the host closed.
            let code =
                match crate::worker::run_worker(tokio::io::stdin(), tokio::io::stdout()).await {
                    Ok(code) => i32::from(code),
                    Err(error) => {
                        eprintln!("{error}");
                        1
                    }
                };
            std::process::exit(code);
        }
        Invocation::Supervisor { data_dir, command } => {
            crate::supervisor::supervise(data_dir, command).await
        }
        Invocation::Command(command) => match run_command(&command).await {
            Ok(()) => 0,
            Err(Failure::Message(message)) => {
                eprintln!("{message}");
                1
            }
            Err(Failure::Silent) => 1,
        },
    };
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

enum Failure {
    Message(String),
    /// The command already said what it had to say (a status that is not running).
    Silent,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<FileError> for Failure {
    fn from(error: FileError) -> Self {
        Self::Message(error.to_string())
    }
}

async fn run_command(command: &CliCommand) -> Result<(), Failure> {
    let path = data_dir_of(command.choice())?;
    match command.action {
        Action::Server => serve(&path, command.port).await,
        Action::Start => start(&path, command.port).await,
        Action::Restart => {
            stop(&path).await?;
            start(&path, command.port).await
        }
        Action::Stop => stop(&path).await,
        Action::Status => status(&path).await,
        Action::List => list().await,
    }
}

/// Runs the server in the foreground until a signal or the owner's stop closes it.
async fn serve(path: &Path, port: Option<u16>) -> Result<(), Failure> {
    let data_dir = open_data_dir(path)?;
    let (own_port, kept) = kept_port(&data_dir).map_err(|error| error.to_string())?;
    let stop = std::sync::Arc::new(Notify::new());
    let stop_notice = stop.clone();
    let options = ServerOptions {
        data_dir: data_dir.clone(),
        port: port.unwrap_or(own_port),
        worker: WorkerSpec::current().map_err(|error| error.to_string())?,
        worker_idle_ms: crate::server::IDLE_MS,
        version: version().to_string(),
        on_stop: std::sync::Arc::new(move || stop_notice.notify_one()),
        on_error: std::sync::Arc::new(|error| eprintln!("{error}")),
    };
    let server = start_server(options, &pi_rs_chord::context::Context::background())
        .await
        .map_err(|error| error.to_string())?;
    if port.is_none() && !kept {
        let _ = keep_port(&data_dir, server.port);
    }
    served_line(&server, supervised()).await?;
    wait_for_stop(&stop).await;
    server
        .close(&pi_rs_chord::context::Context::background())
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn wait_for_stop(stop: &Notify) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                stop.notified().await;
                return;
            }
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(_) => {
                stop.notified().await;
                return;
            }
        };
        tokio::select! {
            _ = stop.notified() => {}
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        stop.notified().await;
    }
}

/// Prints what the server started as: `p3 on <data dir>, port <port>, and the owner link the command
/// shows (a supervised server prints none: `p3` prints a fresh one).
async fn served_line(server: &RunningServer, supervised: bool) -> Result<(), Failure> {
    let link = if supervised {
        String::new()
    } else {
        format!(
            ". Open {}",
            owner_link(&server.data_dir, server.port).await?
        )
    };
    println!(
        "p3 on {}, port {}{link}",
        server.data_dir.display(),
        server.port
    );
    Ok(())
}

fn supervised() -> bool {
    std::env::var_os(SUPERVISED_ENV).is_some()
}

/// Starts the server in the background under its supervisor unless it runs, and prints its port and
/// an owner link.
async fn start(path: &Path, port: Option<u16>) -> Result<(), Failure> {
    let data_dir = open_data_dir(path)?;
    let mut record = running(&data_dir);
    if record.is_none() {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let mut server_args = vec![
            executable.clone().into_os_string(),
            OsString::from("server"),
            OsString::from("--dir"),
            data_dir.as_os_str().to_owned(),
        ];
        if let Some(port) = port {
            server_args.push(OsString::from("--port"));
            server_args.push(OsString::from(port.to_string()));
        }
        let args = supervisor_args(&data_dir, &server_args);
        let mut command = std::process::Command::new(executable);
        command
            .args(&args)
            .env(SUPERVISED_ENV, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.spawn().map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_millis(START_MS);
        while record.is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            record = running(&data_dir);
        }
        let Some(record) = record else {
            return Err(Failure::Message(format!(
                "p3: the server did not start; see {}",
                data_dir.join("logs").join("server.log").display()
            )));
        };
        let link = owner_link(&data_dir, record.port).await?;
        println!(
            "p3 on {}, port {}. Open {link}",
            data_dir.display(),
            record.port
        );
        return Ok(());
    }
    let Some(record) = record else {
        return Err(Failure::Message("p3: the server did not start".into()));
    };
    let link = owner_link(&data_dir, record.port).await?;
    println!(
        "p3 on {}, port {}. Open {link}",
        data_dir.display(),
        record.port
    );
    Ok(())
}

/// Stops the server and its supervisor: the server is asked over HTTP as the owner (its supervisor
/// ends with it), a server that does not answer is signalled; returns once both are gone.
async fn stop(path: &Path) -> Result<(), Failure> {
    let data_dir = std::fs::canonicalize(path).ok();
    let record = data_dir.as_ref().and_then(|dir| running(dir));
    let supervisor = data_dir.as_ref().and_then(|dir| supervisor_pid(dir));
    let Some(data_dir) = data_dir.filter(|_| record.is_some() || supervisor.is_some()) else {
        println!("not running");
        return Ok(());
    };
    let asked = match &record {
        Some(record) => ask_owner(&data_dir, record.port, crate::plugins::control::STOP_PATH)
            .await
            .ok(),
        None => None,
    };
    if !asked.is_some_and(|status| (200..300).contains(&status)) {
        if let Some(pid) = supervisor {
            signal(pid, "TERM");
        } else if let Some(record) = &record {
            signal(record.pid, "TERM");
        }
    }
    let deadline = Instant::now() + Duration::from_millis(STOP_MS);
    while running(&data_dir).is_some() || supervisor_pid(&data_dir).is_some() {
        if Instant::now() > deadline {
            if let Some(record) = &record {
                signal(record.pid, "KILL");
            }
            if let Some(pid) = supervisor {
                signal(pid, "KILL");
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("stopped");
    Ok(())
}

/// Says whether the server runs, and what it is.
async fn status(path: &Path) -> Result<(), Failure> {
    let data_dir = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match running(&data_dir) {
        None => {
            let supervisor = supervisor_pid(&data_dir).is_some();
            println!(
                "{}\n  data directory  {}",
                if supervisor {
                    "starting"
                } else {
                    "not running"
                },
                data_dir.display()
            );
            Err(Failure::Silent)
        }
        Some(record) => {
            let rows = [
                ("data directory", data_dir.display().to_string()),
                ("port", record.port.to_string()),
                ("pid", record.pid.to_string()),
                ("version", record.version),
                ("started", iso_time(record.started_at)),
            ];
            let mut lines = vec!["running".to_string()];
            for (name, value) in rows {
                lines.push(format!("  {name:<16}{value}"));
            }
            println!("{}", lines.join("\n"));
            Ok(())
        }
    }
}

/// Lists the default Pi (once it started) and the named ones, each with its state, port and data
/// directory.
async fn list() -> Result<(), Failure> {
    let mut pis = Vec::new();
    if let Ok(default) = crate::files::default_data_dir() {
        pis.push(("(default)".to_string(), default));
    }
    for pi in named_pis().map_err(|error| error.to_string())? {
        pis.push((pi.name, pi.data_dir));
    }
    let mut rows = Vec::new();
    for (name, path) in pis {
        let data_dir = match std::fs::canonicalize(&path) {
            Ok(data_dir) => data_dir,
            Err(_) => continue,
        };
        let Some(config) = read_config(&data_dir).map_err(|error| error.to_string())? else {
            continue;
        };
        let record = running(&data_dir);
        let state = if record.is_some() {
            "running"
        } else if supervisor_pid(&data_dir).is_some() {
            "starting"
        } else {
            "stopped"
        };
        let port = record
            .map(|record| record.port)
            .or(config.port)
            .map(|port| port.to_string())
            .unwrap_or_else(|| "-".into());
        rows.push((
            name,
            state.to_string(),
            port,
            data_dir.display().to_string(),
        ));
    }
    if rows.is_empty() {
        println!("no Pi yet: `p3` starts the default one");
        return Ok(());
    }
    let width = |column: usize| {
        rows.iter()
            .map(|row| [&row.0, &row.1, &row.2, &row.3][column].chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths = [width(0), width(1), width(2)];
    for (name, state, port, dir) in rows {
        println!(
            "{name:<w0$}  {state:<w1$}  {port:<w2$}  {dir}",
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        );
    }
    Ok(())
}

/// A fresh owner link from the server on `port`, asked with the owner token file.
async fn owner_link(data_dir: &Path, port: u16) -> Result<String, Failure> {
    let deadline = Instant::now() + Duration::from_millis(5000);
    loop {
        if let Some(link) = request_owner_link(data_dir, port).await {
            return Ok(link);
        }
        if Instant::now() > deadline {
            return Err(Failure::Message(format!(
                "p3: the server on port {port} gave no owner link"
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn request_owner_link(data_dir: &Path, port: u16) -> Option<String> {
    let data_dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let token = std::fs::read_to_string(crate::plugins::sign_in::owner_token_file(&data_dir))
            .ok()?
            .trim()
            .to_string();
        let answer = client::post(
            port,
            crate::plugins::sign_in::OWNER_LINK_PATH,
            Request {
                bearer: Some(&token),
                ..Request::default()
            },
        )
        .ok()?;
        if !(200..300).contains(&answer.status) {
            return None;
        }
        answer.json()?.get("link")?.as_str().map(str::to_string)
    })
    .await
    .ok()
    .flatten()
}

/// Posts to `path` on the server on `port` as the owner (the owner token file's token as a bearer).
async fn ask_owner(data_dir: &Path, port: u16, path: &str) -> Result<u16, Failure> {
    let data_dir = data_dir.to_path_buf();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let token = std::fs::read_to_string(crate::plugins::sign_in::owner_token_file(&data_dir))
            .map_err(|error| error.to_string())?
            .trim()
            .to_string();
        client::post(
            port,
            &path,
            Request {
                bearer: Some(&token),
                ..Request::default()
            },
        )
        .map(|answer| answer.status)
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(Failure::Message)
}

/// Sends `name` to `pid`; one that left meanwhile is fine.
fn signal(pid: u32, name: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn iso_time(ms: u64) -> String {
    let seconds = (ms / 1000) as i64;
    let millis = ms % 1000;
    // INFALLIBLE: any i64 is a valid timestamp, and the epoch is one.
    let datetime = chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_else(|| {
        chrono::DateTime::from_timestamp(0, 0).expect("the epoch is a timestamp")
    });
    format!("{}.{millis:03}Z", datetime.format("%Y-%m-%dT%H:%M:%S"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<OsString> {
        std::iter::once("p3")
            .chain(line.split_whitespace())
            .map(OsString::from)
            .collect()
    }

    fn command(line: &str) -> CliCommand {
        match parse(&args(line)).expect("parse") {
            Invocation::Command(command) => command,
            other => panic!("not a command: {other:?}"),
        }
    }

    #[test]
    fn plain_p3_starts_the_default_pi() {
        assert_eq!(
            command(""),
            CliCommand {
                action: Action::Start,
                pi: None,
                dir: None,
                port: None
            }
        );
        assert_eq!(command("start").action, Action::Start);
    }

    #[test]
    fn every_command_is_recognized() {
        for (line, action) in [
            ("server", Action::Server),
            ("status", Action::Status),
            ("stop", Action::Stop),
            ("restart", Action::Restart),
            ("list", Action::List),
        ] {
            assert_eq!(command(line).action, action, "{line}");
        }
    }

    #[test]
    fn a_pi_is_named_by_pi_dir_and_port() {
        let named = command("--pi two");
        assert_eq!(named.pi.as_deref(), Some("two"));
        let directed = command("server --dir /tmp/pi --port 0");
        assert_eq!(directed.dir.as_deref(), Some("/tmp/pi"));
        assert_eq!(directed.port, Some(0));
        assert_eq!(command("--pi=two").pi.as_deref(), Some("two"));
        assert_eq!(command("--port=9123").port, Some(9123));
    }

    #[test]
    fn help_and_version_win_a_parsed_line() {
        assert_eq!(parse(&args("--help")).expect("parse"), Invocation::Help);
        assert_eq!(
            parse(&args("--version")).expect("parse"),
            Invocation::Version
        );
        assert_eq!(
            parse(&args("server --help")).expect("parse"),
            Invocation::Help
        );
    }

    #[test]
    fn the_hidden_modes_are_their_own_invocations() {
        assert_eq!(parse(&args("__worker")).expect("parse"), Invocation::Worker);
        match parse(&args("__supervisor /tmp/pi server --dir /tmp/pi")).expect("parse") {
            Invocation::Supervisor { data_dir, command } => {
                assert_eq!(data_dir, PathBuf::from("/tmp/pi"));
                assert_eq!(
                    command,
                    [
                        OsString::from("server"),
                        OsString::from("--dir"),
                        OsString::from("/tmp/pi")
                    ]
                );
            }
            other => panic!("not a supervisor: {other:?}"),
        }
        assert!(parse(&args("__supervisor")).is_err());
        assert!(parse(&args("__supervisor /tmp/pi")).is_err());
    }

    #[test]
    fn a_line_it_does_not_understand_is_refused() {
        for line in [
            "bogus",
            "status --port 1",
            "--pi Not A Name",
            "server --pi one --dir /tmp/one",
            "list --pi two",
            "list --dir /tmp/one",
            "start --port 65536",
            "start --port -1",
            "start --port 123456",
            "server --port x",
            "server extra",
            "--bogus",
            "--pi",
        ] {
            assert!(parse(&args(line)).is_err(), "{line}");
        }
    }

    #[test]
    fn a_start_time_is_an_iso_instant() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    }
}
