#![forbid(unsafe_code)]

use pi_rs_voice_server::{
    Error, Result,
    models::{Config, init_runtime},
    router,
};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pi-voice-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let token = std::env::var("VOICE_TOKEN")
        .map_err(|_| Error::Config("VOICE_TOKEN is required".into()))?;
    let config = Config::from_env()?;
    init_runtime(None)?;
    let server = config.load(&token)?;
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, config.port)).await?;
    eprintln!(
        "pi-voice-server listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, router(server)).await?;
    Ok(())
}
