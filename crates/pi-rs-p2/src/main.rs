#![forbid(unsafe_code)]

use std::{
    env,
    error::Error,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use pi_rs_p2::{
    hosts::server::{ServerOptions, start_server},
    runtime::logs::{LogLevel, ProcessCapture, PublishedLogs, process_logs},
    upstream,
};

fn main() -> ExitCode {
    let worker = env::args_os().nth(1).as_deref() == Some("--worker".as_ref());
    let result = start(worker);
    match result {
        Ok(ServerExit::Stopped) => ExitCode::SUCCESS,
        Ok(ServerExit::Restart) => ExitCode::from(pi_rs_p2::hosts::dev::DEV_RESTART_EXIT_CODE),
        Err(error) => {
            eprintln!("p2: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerExit {
    Stopped,
    Restart,
}

type HostResult<T = ()> = Result<T, Box<dyn Error>>;

fn start(worker: bool) -> HostResult<ServerExit> {
    if env::args_os().nth(1).as_deref() == Some("tui".as_ref()) {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(pi_rs_p2::hosts::tui::run())?;
        return Ok(ServerExit::Stopped);
    }
    #[cfg(unix)]
    if env::args_os().skip(1).any(|arg| arg == "--dev-supervisor") {
        let directory = match env::var_os("P2_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => env::home_dir()
                .ok_or("Cannot determine home directory")?
                .join(".pi/p2"),
        };
        let port = env::var_os("PORT")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string_lossy().parse())
            .transpose()?
            .unwrap_or(8788);
        let rebuild_cmd = env::var(pi_rs_p2::hosts::dev::DEV_REBUILD_CMD_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty());
        let supervisor_options = pi_rs_p2::hosts::dev::supervisor::SupervisorOptions {
            binary: env::current_exe()?,
            data_dir: directory,
            port,
            fake: env::var("P2_FAKE").as_deref() == Ok("1"),
            rebuild_cmd,
            extra_env: Vec::new(),
            extra_args: Vec::new(),
        };
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async move {
                let supervisor =
                    pi_rs_p2::hosts::dev::supervisor::DevSupervisor::start(supervisor_options)
                        .await?;
                let mut state = supervisor.state();
                if let Some(addr) = &state.borrow().address {
                    println!("p2 supervisor: {addr}");
                }
                tokio::select! {
                    _ = wait_for_shutdown() => {}
                    _ = async {
                        while state.borrow_and_update().running {
                            if state.changed().await.is_err() {
                                break;
                            }
                        }
                    } => {}
                }
                supervisor.stop().await?;
                Ok::<_, Box<dyn Error>>(())
            })?;
        return Ok(ServerExit::Stopped);
    }
    let directory = match env::var_os("P2_DATA_DIR") {
        Some(path) => PathBuf::from(path),
        None => env::home_dir()
            .ok_or("Cannot determine home directory")?
            .join(".pi/p2"),
    };
    let runtime = || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
    };
    if !worker {
        let mut exit = ServerExit::Stopped;
        published_at("server", &directory, "server.logs.json", |capture| {
            exit = runtime()?.block_on(run_server(directory.clone(), capture))?;
            Ok(())
        })?;
        return Ok(exit);
    }
    use pi_rs_p2::api::v1::{PluginRegistry, worker::WorkerSession};
    use pi_rs_p2::hosts::worker::{WorkerInit, start_session_worker};
    use pi_rs_p2::runtime::host_dirs::session_directory;
    // Until the parent supplies a validated session ID, only startup failures
    // have a snapshot. A healthy worker publishes in its own session directory.
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => {
            return published_at(
                "worker-startup",
                &directory,
                "worker-startup.logs.json",
                |_| Err(e.into()),
            )
            .map(|()| ServerExit::Stopped);
        }
    };
    let handshake = rt.block_on(async {
        let worker = WorkerSession::start(tokio::io::stdin(), tokio::io::stdout()).await?;
        let init: WorkerInit = serde_json::from_value(worker.start.clone())?;
        let id = init.session()?.id;
        let path = session_directory(&init.data_dir, &id)?;
        Ok::<_, Box<dyn Error>>((worker, init.data_dir, id, path))
    });
    let (worker, data_dir, id, path) = match handshake {
        Ok(parts) => parts,
        Err(e) => {
            rt.shutdown_background();
            return published_at(
                "worker-startup",
                &directory,
                "worker-startup.logs.json",
                |_| Err(e),
            )
            .map(|()| ServerExit::Stopped);
        }
    };
    process_logs().add_directory(data_dir);
    let result = published_at(&format!("session:{id}"), &path, "worker.logs.json", |_| {
        rt.block_on(async {
            let mut registry = PluginRegistry::new();
            pi_rs_p2::plugins::register_builtins(&mut registry)?;
            Ok(start_session_worker(worker, &registry, None)
                .await?
                .run()
                .await?)
        })
    });
    rt.shutdown_background();
    result.map(|()| ServerExit::Stopped)
}

fn published_at(
    source: &str,
    path: &Path,
    file: &str,
    host: impl FnOnce(&ProcessCapture) -> HostResult,
) -> HostResult {
    let store = process_logs();
    let logs = PublishedLogs::start_at(store.clone(), source, path, file)?;
    logs.run(|capture| {
        store.append(source, LogLevel::Event, "Starting");
        host(capture)?;
        store.append(source, LogLevel::Event, "Closed");
        Ok(())
    })
}

async fn run_server(data_dir: PathBuf, capture: &ProcessCapture) -> HostResult<ServerExit> {
    let provider = env::var_os("P2_PROVIDER").or_else(|| env::var_os("PI_MOBILE_PROVIDER"));
    let model = env::var_os("P2_MODEL").or_else(|| env::var_os("PI_MOBILE_MODEL"));
    if provider.is_some_and(|p| !p.is_empty()) != model.is_some_and(|m| !m.is_empty()) {
        return Err("Set both P2_PROVIDER and P2_MODEL".into());
    }
    let mut options = ServerOptions::new(data_dir);
    pi_rs_p2::plugins::register_builtins(&mut options.registry)?;
    options.dev |= env::args_os().skip(1).any(|arg| arg == "--dev");
    options.app_dir = upstream::find_app(&env::current_dir()?)?;
    if let Some(port) = env::var_os("PORT").filter(|value| !value.is_empty()) {
        options.port = port.to_string_lossy().parse()?;
    }
    options.fake = env::var("P2_FAKE").as_deref() == Ok("1");
    options.allowed_hosts = pi_rs_p2::runtime::http::parse_allowed_hosts(
        &env::var("P2_ALLOWED_HOSTS").unwrap_or_default(),
    );
    let server = start_server(options).await?;
    writeln!(
        capture.stdout(std::io::stdout()),
        "p2: http://{} (data {})",
        server.address,
        server.data_dir.display()
    )?;
    let exit = tokio::select! {
        res = wait_for_shutdown() => {
            res?;
            ServerExit::Stopped
        }
        _ = server.wait_for_restart() => ServerExit::Restart,
    };
    server.close().await?;
    Ok(exit)
}

async fn wait_for_shutdown() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
            _ = hangup.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
