use pi_rs_p2_mac::{
    Error, Result,
    appearance::Appearance,
    diagnostics::{Diagnostics, bridge_sample},
    server_config::{Bundled, JsonPreferences, Launch, environment, home},
    server_controller::{Ownership, PROBE_INTERVAL, Phase, ServerController},
};
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => graphical(),
        [arg] if arg == "--help" || arg == "-h" => {
            println!("Usage: pi-mac [--check-config | --diag | --serve]");
            Ok(())
        }
        [arg] if arg == "--check-config" || arg == "--diag" || arg == "--serve" => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(Error::from)
                .and_then(|rt| rt.block_on(headless(arg)))
        }
        _ => Err(Error::Protocol(
            "usage: pi-mac [--check-config | --diag | --serve]".into(),
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pi-mac: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
fn graphical() -> Result<()> {
    pi_rs_p2_mac::macos::run()
}
#[cfg(not(target_os = "macos"))]
fn graphical() -> Result<()> {
    Err(Error::Protocol(
        "the AppKit UI requires macOS; use --check-config, --diag, or --serve on this platform"
            .into(),
    ))
}

async fn headless(arg: &std::ffi::OsStr) -> Result<()> {
    let env = environment();
    let home = home(&env);
    let prefs = env
        .get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("pi-mac/settings.json");
    let mut prefs = JsonPreferences::open(prefs)?;
    let bundle = std::env::current_exe()
        .ok()
        .and_then(|exe| Bundled::find(&exe.parent()?.parent()?.join("Resources")));
    let launch = Launch::load(&env, &prefs, bundle.as_ref())?;
    if arg == "--check-config" {
        println!("{}", serde_json::to_string_pretty(&launch.selected)?);
        return Ok(());
    }
    let mut server = ServerController::new(launch.selected.clone(), &launch.home, env)?;
    server.poll().await?;
    if arg == "--diag" {
        let mut capture = serde_json::json!({"config":launch.selected,"server":server.state(),"appearance":Appearance::load(&prefs)});
        if let Some(mut diagnostics) = Diagnostics::new(&launch)
            && let Some(preference) = diagnostics.consume_bridge(true)
        {
            let record = bridge_sample(
                &server.config.origin(),
                Appearance::load(&prefs),
                &preference,
            )?;
            diagnostics.bridge(1, &record)?;
            capture["bridge"] = serde_json::to_value(record)?;
        }
        println!("{}", serde_json::to_string_pretty(&capture)?);
        return Ok(());
    }
    if server.can_start() {
        server.prepare_upgrade(env!("CARGO_PKG_VERSION"), &mut prefs);
        server.start().await?;
    } else if server.state().ownership == Ownership::None {
        return Err(Error::Protocol(
            "no p2 server or valid checkout selected".into(),
        ));
    }
    let mut last = Phase::Checking;
    let mut interval = tokio::time::interval(PROBE_INTERVAL);
    let shutdown = shutdown();
    tokio::pin!(shutdown);
    loop {
        let state = server.state();
        if state.phase != last {
            println!("{}", serde_json::to_string(&state)?);
            if let Phase::Failed(error) = state.phase {
                server.stop().await?;
                return Err(Error::Protocol(error));
            }
            last = state.phase;
        }
        tokio::select! {
            result = &mut shutdown => { result?; return server.stop().await; }
            _ = interval.tick() => server.poll().await?,
        }
    }
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { res = tokio::signal::ctrl_c() => res?, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}
