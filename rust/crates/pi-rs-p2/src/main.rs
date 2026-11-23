#![forbid(unsafe_code)]

use std::{env, error::Error, path::PathBuf, process::ExitCode};

use pi_rs_p2::{
    hosts::server::{ServerOptions, start_server},
    upstream,
};

fn main() -> ExitCode {
    let worker = env::args_os().nth(1).as_deref() == Some("--worker".as_ref());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let result = runtime.map_err(Box::<dyn Error>::from).and_then(|runtime| {
        let result = runtime.block_on(run(worker));
        if worker {
            // Chord has closed the worker. Tokio stdin may still be waiting for a frame.
            runtime.shutdown_background();
        }
        result
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("p2: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(worker: bool) -> Result<(), Box<dyn Error>> {
    if worker {
        use pi_rs_p2::api::v1::worker::WorkerSession;
        use pi_rs_p2::{api::v1::PluginRegistry, hosts::worker::start_session_worker};
        let worker = WorkerSession::start(tokio::io::stdin(), tokio::io::stdout()).await?;
        return Ok(start_session_worker(worker, &PluginRegistry::new(), None)
            .await?
            .run()
            .await?);
    }
    let provider = env::var_os("P2_PROVIDER").or_else(|| env::var_os("PI_MOBILE_PROVIDER"));
    let model = env::var_os("P2_MODEL").or_else(|| env::var_os("PI_MOBILE_MODEL"));
    if provider.is_some_and(|p| !p.is_empty()) != model.is_some_and(|m| !m.is_empty()) {
        return Err("Set both P2_PROVIDER and P2_MODEL".into());
    }
    let data_dir = match env::var_os("P2_DATA_DIR") {
        Some(directory) => PathBuf::from(directory),
        None => env::home_dir()
            .ok_or("Cannot determine home directory")?
            .join(".pi/p2"),
    };
    let mut options = ServerOptions::new(data_dir);
    options.app_dir = upstream::find_app(&env::current_dir()?)?;
    if let Some(port) = env::var_os("PORT").filter(|value| !value.is_empty()) {
        options.port = port.to_string_lossy().parse()?;
    }
    options.fake = env::var("P2_FAKE").as_deref() == Ok("1");
    options.allowed_hosts = pi_rs_p2::runtime::http::parse_allowed_hosts(
        &env::var("P2_ALLOWED_HOSTS").unwrap_or_default(),
    );
    let server = start_server(options).await?;
    println!(
        "p2: http://{} (data {})",
        server.address,
        server.data_dir.display()
    );
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
