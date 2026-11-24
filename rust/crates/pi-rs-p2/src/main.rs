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
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("p2: {error}");
            ExitCode::FAILURE
        }
    }
}

type HostResult = Result<(), Box<dyn Error>>;

fn start(worker: bool) -> HostResult {
    if env::args_os().nth(1).as_deref() == Some("tui".as_ref()) {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(pi_rs_p2::hosts::tui::run());
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
        return published_at("server", &directory, "server.logs.json", |capture| {
            runtime()?.block_on(run_server(directory.clone(), capture))
        });
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
            );
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
            );
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
    result
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

async fn run_server(data_dir: PathBuf, capture: &ProcessCapture) -> HostResult {
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
    wait_for_shutdown().await?;
    Ok(server.close().await?)
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
